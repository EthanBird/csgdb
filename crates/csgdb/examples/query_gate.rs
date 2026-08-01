use csgdb::{Database, KeySource, SecretKey, DEFAULT_PREPARED_STATEMENT_CACHE_CAPACITY};
use std::env;
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

const ROW_COUNT: usize = 10_000;
const BENCHMARK_KEY: [u8; 32] = [
    0x63, 0x73, 0x67, 0x64, 0x62, 0x2d, 0x71, 0x75, 0x65, 0x72, 0x79, 0x2d, 0x67, 0x61, 0x74, 0x65,
    0x2d, 0x6b, 0x65, 0x79, 0x2d, 0x30, 0x30, 0x30, 0x31, 0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x17,
];
const CHECKSUM_SEED: u64 = 0xcbf2_9ce4_8422_2325;

type AnyResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy)]
enum Implementation {
    Reference,
    Public,
    Cached,
}

impl Implementation {
    fn parse(value: &str) -> AnyResult<Self> {
        match value {
            "reference" | "direct" => Ok(Self::Reference),
            "public" | "query_i64" => Ok(Self::Public),
            "cached" | "query_i64_cached" => Ok(Self::Cached),
            _ => Err(invalid_input(
                "implementation must be reference, public, or cached",
            )),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Reference => "direct-reference",
            Self::Public => "public-query_i64",
            Self::Cached => "public-query_i64_cached",
        }
    }
}

#[derive(Clone, Copy)]
enum Security {
    Plaintext,
    Encrypted,
}

impl Security {
    fn parse(value: &str) -> AnyResult<Self> {
        match value {
            "plaintext" => Ok(Self::Plaintext),
            "encrypted" => Ok(Self::Encrypted),
            _ => Err(invalid_input("security must be plaintext or encrypted")),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Plaintext => "plaintext",
            Self::Encrypted => "encrypted",
        }
    }
}

#[derive(Clone, Copy)]
enum Workload {
    Fixed,
    Hot16,
    Overflow17,
    Overflow64,
    Unique10000,
}

impl Workload {
    fn parse(value: &str) -> AnyResult<Self> {
        match value {
            "fixed" => Ok(Self::Fixed),
            "hot16" => Ok(Self::Hot16),
            "overflow17" => Ok(Self::Overflow17),
            "overflow64" => Ok(Self::Overflow64),
            "unique10000" => Ok(Self::Unique10000),
            _ => Err(invalid_input(
                "workload must be fixed, hot16, overflow17, overflow64, or unique10000",
            )),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Hot16 => "hot16",
            Self::Overflow17 => "overflow17",
            Self::Overflow64 => "overflow64",
            Self::Unique10000 => "unique10000",
        }
    }

    const fn query_count(self) -> usize {
        match self {
            Self::Fixed => 1,
            Self::Hot16 => 16,
            Self::Overflow17 => 17,
            Self::Overflow64 => 64,
            Self::Unique10000 => ROW_COUNT,
        }
    }
}

struct Config {
    implementation: Implementation,
    security: Security,
    workload: Workload,
    path: PathBuf,
    iterations: usize,
    warmup_iterations: usize,
}

struct Measurements {
    elapsed_ns: u64,
    throughput_ops_s: f64,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    checksum: u64,
}

fn invalid_input(message: &'static str) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidInput, message))
}

fn parse_usize(value: &str, field: &'static str, allow_zero: bool) -> AnyResult<usize> {
    let parsed = value.parse::<usize>().map_err(|_| invalid_input(field))?;
    if !allow_zero && parsed == 0 {
        return Err(invalid_input(field));
    }
    Ok(parsed)
}

fn parse_config() -> AnyResult<Config> {
    let mut arguments = env::args();
    let program = arguments.next().unwrap_or_else(|| "query_gate".to_owned());
    let values: Vec<String> = arguments.collect();
    if !(4..=6).contains(&values.len()) {
        eprintln!(
            "usage: {program} <reference|public|cached> <plaintext|encrypted> \
             <fixed|hot16|overflow17|overflow64|unique10000> <database-path> \
             [iterations] [warmup-iterations]"
        );
        return Err(invalid_input("invalid argument count"));
    }

    let iterations = values.get(4).map_or(Ok(50_000), |value| {
        parse_usize(value, "iterations must be a positive integer", false)
    })?;
    let warmup_iterations = values.get(5).map_or(Ok(10_000), |value| {
        parse_usize(
            value,
            "warmup iterations must be a non-negative integer",
            true,
        )
    })?;

    Ok(Config {
        implementation: Implementation::parse(&values[0])?,
        security: Security::parse(&values[1])?,
        workload: Workload::parse(&values[2])?,
        path: PathBuf::from(&values[3]),
        iterations,
        warmup_iterations,
    })
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

fn ensure_fresh_path(path: &Path) -> AnyResult<()> {
    if path.exists() || sidecar_path(path, "-wal").exists() || sidecar_path(path, "-shm").exists() {
        return Err(invalid_input(
            "database path or a database sidecar already exists",
        ));
    }
    Ok(())
}

fn open_database(config: &Config) -> AnyResult<Database> {
    ensure_fresh_path(&config.path)?;
    match config.security {
        Security::Plaintext => Ok(Database::open_plaintext(&config.path)?),
        Security::Encrypted => {
            let key = SecretKey::from_slice(&BENCHMARK_KEY)?;
            Ok(Database::open_with_key(&config.path, KeySource::Raw(key))?)
        }
    }
}

fn initialize_database(database: &Database) -> AnyResult<()> {
    database.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         CREATE TABLE query_gate(
             id INTEGER PRIMARY KEY,
             value INTEGER NOT NULL
         );
         WITH RECURSIVE sequence(id) AS (
             VALUES(1)
             UNION ALL
             SELECT id + 1 FROM sequence WHERE id < 10000
         )
         INSERT INTO query_gate(id, value)
         SELECT id, id * 37 + 11 FROM sequence;",
    )?;

    let row_count = query_reference(database, "SELECT count(*) FROM query_gate")?;
    if row_count != i64::try_from(ROW_COUNT)? {
        return Err(Box::new(io::Error::other(
            "query gate dataset has an unexpected row count",
        )));
    }
    Ok(())
}

fn build_queries(workload: Workload) -> Vec<String> {
    (1..=workload.query_count())
        .map(|id| format!("SELECT value FROM query_gate WHERE id = {id}"))
        .collect()
}

fn query_reference(database: &Database, sql: &str) -> AnyResult<i64> {
    let mut statement = database.prepare(black_box(sql))?;
    let mut rows = statement.query(&[])?;
    let value = {
        let row = rows
            .next_row()?
            .ok_or_else(|| io::Error::other("reference query did not return the expected row"))?;
        row.get_i64(0)?
    };
    Ok(value)
}

fn query_once(database: &Database, implementation: Implementation, sql: &str) -> AnyResult<i64> {
    match implementation {
        Implementation::Reference => query_reference(database, sql),
        Implementation::Public => Ok(database.query_i64(black_box(sql))?),
        Implementation::Cached => Ok(database.query_i64_cached(black_box(sql))?),
    }
}

fn expected_value(query_index: usize) -> AnyResult<i64> {
    let id = i64::try_from(query_index + 1)?;
    Ok(id * 37 + 11)
}

fn warm_up(database: &Database, config: &Config, queries: &[String]) -> AnyResult<u64> {
    let mut checksum = CHECKSUM_SEED;
    for operation in 0..config.warmup_iterations {
        let query_index = operation % queries.len();
        let value = query_once(
            database,
            config.implementation,
            black_box(&queries[query_index]),
        )?;
        if value != expected_value(query_index)? {
            return Err(Box::new(io::Error::other(
                "warm-up query returned an unexpected value",
            )));
        }
        checksum = update_checksum(checksum, value, operation)?;
    }
    Ok(black_box(checksum))
}

fn update_checksum(checksum: u64, value: i64, operation: usize) -> AnyResult<u64> {
    let value_bits = u64::from_ne_bytes(value.to_ne_bytes());
    let operation = u64::try_from(operation)?;
    Ok(checksum
        .rotate_left(7)
        .wrapping_mul(0x0000_0100_0000_01b3)
        .wrapping_add(value_bits ^ operation))
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    debug_assert!(!sorted.is_empty());
    debug_assert!((1..=100).contains(&percentile));
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

#[allow(clippy::cast_precision_loss)]
fn throughput(iterations: usize, elapsed_seconds: f64) -> f64 {
    iterations as f64 / elapsed_seconds
}

fn measure(database: &Database, config: &Config, queries: &[String]) -> AnyResult<Measurements> {
    let mut latencies = Vec::with_capacity(config.iterations);
    let mut checksum = CHECKSUM_SEED;
    let total_start = Instant::now();

    for operation in 0..config.iterations {
        let query_index = operation % queries.len();
        let operation_start = Instant::now();
        let value = query_once(
            database,
            config.implementation,
            black_box(&queries[query_index]),
        )?;
        let latency = operation_start.elapsed().as_nanos();
        latencies.push(u64::try_from(latency).unwrap_or(u64::MAX));

        if value != expected_value(query_index)? {
            return Err(Box::new(io::Error::other(
                "measured query returned an unexpected value",
            )));
        }
        checksum = update_checksum(checksum, value, operation)?;
    }

    let elapsed = total_start.elapsed();
    latencies.sort_unstable();
    Ok(Measurements {
        elapsed_ns: u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
        throughput_ops_s: throughput(config.iterations, elapsed.as_secs_f64()),
        p50_ns: percentile(&latencies, 50),
        p95_ns: percentile(&latencies, 95),
        p99_ns: percentile(&latencies, 99),
        checksum: black_box(checksum),
    })
}

fn vm_hwm_kib() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn main() -> AnyResult<()> {
    let config = parse_config()?;
    let setup_start = Instant::now();
    let database = open_database(&config)?;
    initialize_database(&database)?;
    let queries = build_queries(config.workload);
    let setup_ns = u64::try_from(setup_start.elapsed().as_nanos()).unwrap_or(u64::MAX);

    let warmup_start = Instant::now();
    let warmup_checksum = warm_up(&database, &config, &queries)?;
    let warmup_ns = u64::try_from(warmup_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let measurements = measure(&database, &config, &queries)?;
    let vm_hwm = vm_hwm_kib();
    database.close()?;

    let implementation = config.implementation.name();
    let security = config.security.name();
    let workload = config.workload.name();
    let query_count = queries.len();
    let iterations = config.iterations;
    let warmup_iterations = config.warmup_iterations;
    let elapsed_ns = measurements.elapsed_ns;
    let throughput_ops_s = measurements.throughput_ops_s;
    let p50_ns = measurements.p50_ns;
    let p95_ns = measurements.p95_ns;
    let p99_ns = measurements.p99_ns;
    let checksum = measurements.checksum;
    let vm_hwm_json = vm_hwm.map_or_else(|| "null".to_owned(), |value| value.to_string());

    println!(
        "{{\"schema_version\":1,\"implementation\":\"{implementation}\",\
         \"security\":\"{security}\",\"workload\":\"{workload}\",\
         \"statement_cache_capacity\":{DEFAULT_PREPARED_STATEMENT_CACHE_CAPACITY},\
         \"query_count\":{query_count},\"iterations\":{iterations},\
         \"warmup_iterations\":{warmup_iterations},\"setup_ns\":{setup_ns},\
         \"warmup_ns\":{warmup_ns},\"warmup_checksum\":{warmup_checksum},\
         \"elapsed_ns\":{elapsed_ns},\"throughput_ops_s\":{throughput_ops_s:.3},\
         \"p50_ns\":{p50_ns},\"p95_ns\":{p95_ns},\"p99_ns\":{p99_ns},\
         \"vm_hwm_kib\":{vm_hwm_json},\"checksum\":{checksum}}}"
    );
    Ok(())
}
