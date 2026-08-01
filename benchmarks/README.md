# Runtime benchmarks

`sqlite_comparison.c` executes the same SQL, PRAGMAs, dataset, prepared-statement loops and concurrency pattern through three engines:

- the system SQLite C API;
- CSGDB in explicit plaintext mode;
- CSGDB with a fixed 256-bit benchmark key.

The workload contains bulk inserts in one transaction, `synchronous=FULL` durable autocommit writes, random point reads, indexed range scans, random updates, and a sustained four-reader/one-writer WAL stress phase. The stress writer commits 32 updates per transaction. Every reader uses a separate connection and consumes returned payload bytes so the compiler cannot eliminate the query.

All connections use an explicit 4 MiB page cache. This keeps the comparison symmetric and prevents the CSGDB 16 MiB default from hiding encrypted cache-miss costs. The pressure phase uses public C interfaces and therefore includes CSGDB C ABI state validation and locking; it is not a `DatabasePool` or Group Commit benchmark.

Build CSGDB in release mode, then compile the harness against the CSGDB shared library and the platform SQLite library:

```bash
cargo build -p csgdb-ffi --release --locked

cc -O3 -DNDEBUG -std=c11 -Wall -Wextra -Werror \
  benchmarks/sqlite_comparison.c -Iinclude \
  -Ltarget/release -lcsgdb -lsqlite3 -lpthread \
  -Wl,-rpath,"$PWD/target/release" \
  -o target/sqlite-comparison
```

The checked-in comparison uses CSGDB's default `FULLMUTEX` compatibility mode.
For a controlled single-owner connection experiment, build a second binary
with `-DCSGDB_BENCH_MUTEX_FLAG=CSGDB_OPEN_NOMUTEX`; each stress worker owns a
different connection, so this changes only connection-local engine mutexes.
Use `-DCSGDB_BENCH_CACHE_SIZE_BYTES='(16ULL*1024ULL*1024ULL)'` to build a
separate cache-curve binary; the selected size is applied symmetrically to
every system SQLite and CSGDB connection and is reported in JSON output.

Use `-DCSGDB_BENCH_FUSED_FFI=1` only for a controlled C ABI experiment. It
lets CSGDB combine Reset/Clear-Bindings and Blob-Pointer/Length calls while
leaving the system SQLite path unchanged. The macro defaults to zero, so the
standard benchmark API sequence and JSON schema remain unchanged.

Run each engine in a separate process and use a fresh path:

```bash
target/sqlite-comparison sqlite /tmp/sqlite.db \
  100000 200000 5000 100000 1000 10

target/sqlite-comparison csgdb-plaintext /tmp/csgdb-plain.db \
  100000 200000 5000 100000 1000 10

target/sqlite-comparison csgdb-encrypted /tmp/csgdb-encrypted.db \
  100000 200000 5000 100000 1000 10
```

The program refuses to overwrite an existing database or sidecar, runs `integrity_check` after pressure, and emits one JSON object on success. An existing result file can be checked again without running the workload:

```bash
target/sqlite-comparison verify csgdb-encrypted /tmp/csgdb-encrypted.db
```

Use the complete-workload gate for a balanced, auditable comparison. It uses a
fresh database for every invocation, excludes warm-up pairs, alternates AB/BA,
pins every child to the requested CPUs, hashes the benchmark and its resolved
dynamic libraries, verifies the deterministic point/range checksum, and checks
that encrypted output does not expose the SQLite plaintext header:

```bash
python3 benchmarks/run_engine_comparison_gate.py \
  --binary target/sqlite-comparison \
  --candidate-engine csgdb-encrypted \
  --cpu-affinity 0-8 \
  --rounds 8 \
  --output benchmarks/results/engine-comparison.json
```

An old/new comparison uses the same engine on two separately packaged binaries
and sibling libraries:

```bash
python3 benchmarks/run_engine_comparison_gate.py \
  --baseline-binary /tmp/baseline/sqlite-comparison \
  --binary /tmp/candidate/sqlite-comparison \
  --baseline-engine csgdb-encrypted \
  --candidate-engine csgdb-encrypted \
  --cpu-affinity 0-8 \
  --rounds 8
```

For a read-only concurrency gate against one unchanged database, use:

```bash
python3 benchmarks/run_read_stress_gate.py \
  --baseline-binary /tmp/baseline/sqlite-comparison \
  --candidate-binary /tmp/candidate/sqlite-comparison \
  --engine csgdb-encrypted \
  --database /tmp/existing-encrypted.db \
  --rows 100000 --seconds 10 --rounds 8 --cpu-affinity 0-8
```

The Rust scalar-query gate separates direct preparation, the public one-shot
path, and explicit bounded-cache reuse. It includes fixed, 16-query hot-set,
cache-overflow, and high-cardinality workloads:

```bash
python3 benchmarks/run_query_gate.py \
  --binary target/release/examples/query_gate \
  --api-candidate-mode cached \
  --securities encrypted \
  --workloads fixed hot16 overflow17 \
  --rounds 8 --cpu-affinity 0
```

All three runners default to a strict zero-percent median Pareto gate. A single
throughput, latency, file-size, or peak-memory regression fails the relevant
scenario; `--report-only` preserves that failure in JSON while returning zero
for exploratory automation. Measured round counts must be even so each role
runs first equally often. Dynamic-linker overrides are rejected because they
would invalidate the recorded library identities. The engine and read-stress
gates record `ldd` output and verify the hashes of the libraries that were
actually loaded. The read-stress gate also rejects identical baseline/candidate
artifacts and fails if the database, WAL, or shared-memory file changes. The
query gate rejects identical A/B artifacts and validates `p50 <= p95 <= p99`
before applying its Pareto rules.

For manual runs, use at least one warm-up pair and four measured repetitions,
balance AB/BA order, and report the median. Do not compare results collected on
different filesystems or with different `journal_mode`, `synchronous`, cache
sizes, data sizes, compiler optimization levels, or thermal states.

The first published baseline and its raw repetitions are in:

- [`docs/performance-2026-08-01.md`](../docs/performance-2026-08-01.md)
- [`benchmarks/results/2026-08-01-x86_64.json`](results/2026-08-01-x86_64.json)
- [`docs/performance-optimization-2026-08-01.md`](../docs/performance-optimization-2026-08-01.md)
- [`benchmarks/results/2026-08-01-performance-optimization.json`](results/2026-08-01-performance-optimization.json)
- [`docs/performance-optimization-round2-2026-08-01.md`](../docs/performance-optimization-round2-2026-08-01.md)
- [`benchmarks/results/2026-08-01-performance-optimization-round2.json`](results/2026-08-01-performance-optimization-round2.json)
- [`docs/query-performance-round3-2026-08-02.html`](../docs/query-performance-round3-2026-08-02.html)
- [`benchmarks/results/2026-08-02-query-performance-round3.json`](results/2026-08-02-query-performance-round3.json)

The fixed key is test material only. It must never be reused by an application.
