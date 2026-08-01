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

Run at least one warm-up and three measured repetitions, rotate engine order, and report the median. Do not compare results collected on different filesystems or with different `journal_mode`, `synchronous`, cache sizes, data sizes, compiler optimization levels, or thermal states.

The first published baseline and its raw repetitions are in:

- [`docs/performance-2026-08-01.md`](../docs/performance-2026-08-01.md)
- [`benchmarks/results/2026-08-01-x86_64.json`](results/2026-08-01-x86_64.json)
- [`docs/performance-optimization-2026-08-01.md`](../docs/performance-optimization-2026-08-01.md)
- [`benchmarks/results/2026-08-01-performance-optimization.json`](results/2026-08-01-performance-optimization.json)
- [`docs/performance-optimization-round2-2026-08-01.md`](../docs/performance-optimization-round2-2026-08-01.md)
- [`benchmarks/results/2026-08-01-performance-optimization-round2.json`](results/2026-08-01-performance-optimization-round2.json)

The fixed key is test material only. It must never be reused by an application.
