use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use futures::stream::StreamExt;
use itertools::Itertools;
use parquet::{
    arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder},
    file::{reader::FileReader, serialized_reader::SerializedFileReader},
};
use std::{env, fs, time::Duration};
use tokio::runtime::Runtime;

use zn_perf::match_udf;

fn parquet_sample_path() -> String {
    env::var("FILE").expect("Set FILE environment variable")
}

fn new_parquet_file_reader() -> SerializedFileReader<Bytes> {
    let buf = fs::read(parquet_sample_path()).unwrap(); // load the entire file into memory
    SerializedFileReader::new(buf.into()).unwrap()
}

fn new_parquet_arrow_reader(batch_size: usize) -> ParquetRecordBatchReader {
    let buf = fs::read(parquet_sample_path()).unwrap(); // load the entire file into memory
    ParquetRecordBatchReaderBuilder::try_new(<Vec<u8> as Into<Bytes>>::into(buf))
        .unwrap()
        .with_batch_size(batch_size)
        .build()
        .unwrap()
}

async fn new_datafusion_session_context(
    batch_size: usize,
    optimized_p: bool,
) -> datafusion::prelude::SessionContext {
    let ctx = zn_perf::datafusion::new_session_context(batch_size, optimized_p);
    ctx.register_parquet("tbl", &parquet_sample_path(), Default::default())
        .await
        .unwrap();
    ctx
}

fn bench_file_search(c: &mut Criterion) {
    let parquet_reader = new_parquet_file_reader();

    let size = zn_perf::file::byte_array_columns_uncompressed_size(parquet_reader.metadata());

    let mut group = c.benchmark_group("file-search");
    group
        .measurement_time(Duration::from_secs(15))
        .throughput(Throughput::Bytes(size));

    group.bench_function("count-occurrences", |b| {
        b.iter(|| zn_perf::file::count_occurrences(&parquet_reader, b"search_string").unwrap())
    });
    group.finish();
}

fn bench_arrow_search(c: &mut Criterion) {
    let size: usize = new_parquet_arrow_reader(4096)
        .into_iter()
        .map(|batch| batch.unwrap().get_array_memory_size())
        .sum();

    let mut group = c.benchmark_group("arrow-search");
    group
        .measurement_time(Duration::from_secs(8))
        .throughput(Throughput::Bytes(size as u64));

    for batch_size in [1024, 4096, 8192] {
        group.bench_function(BenchmarkId::from_parameter(batch_size), |b| {
            b.iter_batched(
                || new_parquet_arrow_reader(batch_size),
                |parquet_reader| {
                    zn_perf::arrow::count_occurrences(parquet_reader, "search_string").unwrap()
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

fn bench_datafusion_queries(c: &mut Criterion) {
    const QUERIES: &[&str] = &[
        "select * from tbl",
        r#"select * from tbl where "kubernetes.labels.operator.prometheus.io/name" = 'k8s'"#,
        r#"select * from tbl where "kubernetes.labels.controller-revision-hash" like '%ziox%'"#,
        "select * from tbl where log like '%k8s%'",
        "select * from tbl where strpos(log, 'k8s') > 0",
    ];

    let mut group = c.benchmark_group("datafusion/queries");
    group.measurement_time(Duration::from_secs(15));

    let rt = Runtime::new().unwrap();
    for query in QUERIES {
        for batch_size in [1024, 4096, 8192] {
            for optimized_p in [false] {
                group.bench_function(
                    BenchmarkId::from_parameter(format!(
                        "{batch_size}-O{}/{query}",
                        optimized_p as u8
                    )),
                    |b| {
                        b.to_async(&rt).iter(|| async {
                            let ctx = new_datafusion_session_context(batch_size, optimized_p).await;
                            let df = ctx.sql(query).await.unwrap();
                            let mut stream = df.execute_stream().await.unwrap();
                            while let Some(batch) = stream.next().await {
                                let _ = batch.unwrap().num_rows();
                            }
                        })
                    },
                );
            }
        }
    }
    group.finish();
}

fn bench_datafusion_search(c: &mut Criterion) {
    let f = fs::File::open(parquet_sample_path()).unwrap();
    let mut total_size = 0; // uncompressed size of text columns
    let text_columns = zn_perf::metadata::text_columns(&f)
        .unwrap()
        .into_iter()
        .filter_map(|(name, size)| {
            // HACK: `SessionContext::sql()` doesn't like "@timestamp" column:
            // ```
            // thread 'main' panicked at 'called `Result::unwrap()` on an `Err` value: Execution("variable [\"@timestamp\"] has no type information")'
            // ```
            (name != "@timestamp").then(|| {
                total_size += size;
                name
            })
        })
        .collect_vec();

    let mut group = c.benchmark_group("datafusion/search");
    group.throughput(Throughput::Bytes(total_size));

    for batch_size in [1024, 4096, 8192] {
        for op in ["like", "strpos"] {
            for optimized_p in [false] {
                let where_clause = text_columns
                    .iter()
                    .map(|column| {
                        if op == "like" {
                            format!("\"{column}\" like '%k8s%'")
                        } else {
                            format!("strpos(\"{column}\", 'k8s') > 0")
                        }
                    })
                    .join(" or ");
                let sql = format!("select * from tbl where {where_clause}");

                let rt = Runtime::new().unwrap();
                group.bench_function(
                    BenchmarkId::from_parameter(format!(
                        "{batch_size}-O{}/{op}",
                        optimized_p as u8
                    )),
                    |b| {
                        b.to_async(&rt).iter(|| async {
                            let ctx = new_datafusion_session_context(batch_size, optimized_p).await;
                            let df = ctx.sql(&sql).await.unwrap();
                            let mut stream = df.execute_stream().await.unwrap();
                            while let Some(batch) = stream.next().await {
                                let _ = batch.unwrap().num_rows();
                            }
                        })
                    },
                );
            }
        }
    }
    group.finish();
}

fn bench_datafusion_search_memchr(c: &mut Criterion) {
    let f = fs::File::open(parquet_sample_path()).unwrap();
    let mut total_size = 0; // uncompressed size of text columns
    let text_columns = zn_perf::metadata::text_columns(&f)
        .unwrap()
        .into_iter()
        .filter_map(|(name, size)| {
            // HACK: `SessionContext::sql()` doesn't like "@timestamp" column:
            // ```
            // thread 'main' panicked at 'called `Result::unwrap()` on an `Err` value: Execution("variable [\"@timestamp\"] has no type information")'
            // ```
            (name != "@timestamp").then(|| {
                total_size += size;
                name
            })
        })
        .collect_vec();

    let mut group = c.benchmark_group("datafusion/searchUDF");
    group.throughput(Throughput::Bytes(total_size));

    for batch_size in [1024, 4096, 8192] {
        for op in ["like", "str_match"] {
            for optimized_p in [false] {
                let where_clause = text_columns
                    .iter()
                    .map(|column| {
                        if op == "like" {
                            format!("\"{column}\" like '%k8s%'")
                        } else {
                            format!("str_match(\"{column}\", 'k8s') ")
                        }
                    })
                    .join(" or ");
                let sql = format!("select * from tbl where {where_clause}");

                let rt = Runtime::new().unwrap();
                group.bench_function(
                    BenchmarkId::from_parameter(format!(
                        "{batch_size}-O{}/{op}",
                        optimized_p as u8
                    )),
                    |b| {
                        b.to_async(&rt).iter(|| async {
                            let ctx = new_datafusion_session_context(batch_size, optimized_p).await;
                            ctx.register_udf(match_udf::MATCH_UDF.clone());
                            let df = ctx.sql(&sql).await.unwrap();
                            let mut stream = df.execute_stream().await.unwrap();
                            while let Some(batch) = stream.next().await {
                                let _ = batch.unwrap().num_rows();
                            }
                        })
                    },
                );
            }
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    // bench_file_search,
    bench_arrow_search,
    // bench_datafusion_queries,
    bench_datafusion_search,
    bench_datafusion_search_memchr,
);
criterion_main!(benches);
