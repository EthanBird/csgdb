#define _POSIX_C_SOURCE 200809L

#include "csgdb.h"

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <time.h>

/* Keep the comparison independent from a development-header package. */
typedef struct sqlite3 sqlite3;
typedef struct sqlite3_stmt sqlite3_stmt;

extern int sqlite3_open(const char *filename, sqlite3 **database);
extern int sqlite3_close(sqlite3 *database);
extern int sqlite3_exec(
    sqlite3 *database,
    const char *sql,
    int (*callback)(void *, int, char **, char **),
    void *context,
    char **error_message
);
extern int sqlite3_prepare_v2(
    sqlite3 *database,
    const char *sql,
    int sql_bytes,
    sqlite3_stmt **statement,
    const char **tail
);
extern int sqlite3_finalize(sqlite3_stmt *statement);
extern int sqlite3_reset(sqlite3_stmt *statement);
extern int sqlite3_clear_bindings(sqlite3_stmt *statement);
extern int sqlite3_bind_int64(sqlite3_stmt *statement, int index, int64_t value);
extern int sqlite3_bind_double(sqlite3_stmt *statement, int index, double value);
extern int sqlite3_bind_blob(
    sqlite3_stmt *statement,
    int index,
    const void *value,
    int length,
    void (*destructor)(void *)
);
extern int sqlite3_step(sqlite3_stmt *statement);
extern int64_t sqlite3_column_int64(sqlite3_stmt *statement, int column);
extern double sqlite3_column_double(sqlite3_stmt *statement, int column);
extern const void *sqlite3_column_blob(sqlite3_stmt *statement, int column);
extern const unsigned char *sqlite3_column_text(sqlite3_stmt *statement, int column);
extern int sqlite3_column_bytes(sqlite3_stmt *statement, int column);
extern const char *sqlite3_errmsg(sqlite3 *database);
extern int sqlite3_busy_timeout(sqlite3 *database, int milliseconds);
extern const char *sqlite3_libversion(void);

#define SQLITE_OK_LOCAL 0
#define SQLITE_ROW_LOCAL 100
#define SQLITE_DONE_LOCAL 101
#define SQLITE_TRANSIENT_LOCAL ((void (*)(void *))-1)

#define PAYLOAD_BYTES 160
#define TENANTS 64
#define READER_THREADS 4
#define WRITE_BATCH 32
#define READER_SAMPLE_CAPACITY 262144
#define WRITER_SAMPLE_CAPACITY 65536
#define CACHE_SIZE_BYTES (4ULL * 1024ULL * 1024ULL)

typedef enum engine_kind {
    ENGINE_SQLITE,
    ENGINE_CSGDB_PLAINTEXT,
    ENGINE_CSGDB_ENCRYPTED
} engine_kind;

typedef struct database_handle {
    engine_kind engine;
    sqlite3 *sqlite;
    csgdb *csgdb;
} database_handle;

typedef struct statement_handle {
    engine_kind engine;
    sqlite3_stmt *sqlite;
    csgdb_stmt *csgdb;
} statement_handle;

typedef struct latency_samples {
    uint64_t *values;
    size_t length;
    size_t capacity;
    uint64_t total;
} latency_samples;

typedef struct stress_control {
    atomic_bool start;
    atomic_bool stop;
} stress_control;

typedef struct reader_context {
    database_handle database;
    stress_control *control;
    latency_samples samples;
    uint64_t operations;
    uint64_t errors;
    uint64_t checksum;
    uint64_t seed;
    uint64_t row_count;
} reader_context;

typedef struct writer_context {
    database_handle database;
    stress_control *control;
    latency_samples samples;
    uint64_t rows;
    uint64_t commits;
    uint64_t errors;
    uint64_t seed;
    uint64_t row_count;
} writer_context;

typedef struct benchmark_result {
    char engine_sqlite_version[32];
    double open_ms;
    double bulk_insert_ms;
    double durable_autocommit_ms;
    double point_read_ms;
    double range_scan_ms;
    double update_ms;
    uint64_t range_rows;
    uint64_t checksum;
    double stress_seconds;
    uint64_t stress_reads;
    uint64_t stress_writes;
    uint64_t stress_commits;
    uint64_t stress_errors;
    double read_p50_us;
    double read_p95_us;
    double read_p99_us;
    double commit_p50_us;
    double commit_p95_us;
    double commit_p99_us;
    uint64_t database_bytes;
    uint64_t max_rss_kib;
    int integrity_ok;
} benchmark_result;

static const uint8_t DATABASE_KEY[32] = {
    0x63, 0x73, 0x67, 0x64, 0x62, 0x2d, 0x62, 0x65,
    0x6e, 0x63, 0x68, 0x6d, 0x61, 0x72, 0x6b, 0x2d,
    0x6b, 0x65, 0x79, 0x2d, 0x30, 0x30, 0x30, 0x31,
    0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x17, 0x28
};

static uint64_t monotonic_ns(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        return 0;
    }
    return (uint64_t)value.tv_sec * 1000000000ULL + (uint64_t)value.tv_nsec;
}

static uint64_t xorshift64(uint64_t *state) {
    uint64_t value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    return value;
}

static const char *engine_name(engine_kind engine) {
    switch (engine) {
        case ENGINE_SQLITE:
            return "sqlite";
        case ENGINE_CSGDB_PLAINTEXT:
            return "csgdb-plaintext";
        case ENGINE_CSGDB_ENCRYPTED:
            return "csgdb-encrypted";
    }
    return "unknown";
}

static int database_open(database_handle *database, engine_kind engine, const char *path) {
    memset(database, 0, sizeof(*database));
    database->engine = engine;
    if (engine == ENGINE_SQLITE) {
        int result = sqlite3_open(path, &database->sqlite);
        if (result == SQLITE_OK_LOCAL) {
            result = sqlite3_busy_timeout(database->sqlite, 5000);
        }
        if (result == SQLITE_OK_LOCAL) {
            result = sqlite3_exec(
                database->sqlite,
                "PRAGMA cache_size=-4096",
                NULL,
                NULL,
                NULL
            );
        }
        return result;
    }
    csgdb_open_options options;
    int result = csgdb_open_options_init(&options);
    if (result != CSGDB_OK) {
        return result;
    }
    options.cache_size_bytes = CACHE_SIZE_BYTES;
    if (engine == ENGINE_CSGDB_PLAINTEXT) {
        options.flags = CSGDB_OPEN_READWRITE | CSGDB_OPEN_CREATE |
            CSGDB_OPEN_PLAINTEXT | CSGDB_OPEN_FULLMUTEX;
    } else {
        options.flags = CSGDB_OPEN_READWRITE | CSGDB_OPEN_CREATE |
            CSGDB_OPEN_ENCRYPTED | CSGDB_OPEN_FULLMUTEX;
        options.key.kind = CSGDB_KEY_RAW;
        options.key.data = DATABASE_KEY;
        options.key.data_len = sizeof(DATABASE_KEY);
    }
    return csgdb_open_v3(path, &database->csgdb, &options);
}

static int database_close(database_handle *database) {
    if (database->engine == ENGINE_SQLITE) {
        int result = sqlite3_close(database->sqlite);
        database->sqlite = NULL;
        return result;
    }
    int result = csgdb_close(database->csgdb);
    database->csgdb = NULL;
    return result;
}

static const char *database_error(database_handle *database) {
    if (database->engine == ENGINE_SQLITE) {
        return database->sqlite == NULL ? "sqlite handle is null" : sqlite3_errmsg(database->sqlite);
    }
    return database->csgdb == NULL ? "csgdb handle is null" : csgdb_errmsg(database->csgdb);
}

static int database_exec(database_handle *database, const char *sql) {
    if (database->engine == ENGINE_SQLITE) {
        return sqlite3_exec(database->sqlite, sql, NULL, NULL, NULL);
    }
    return csgdb_exec(database->csgdb, sql);
}

static int statement_prepare(
    database_handle *database,
    const char *sql,
    statement_handle *statement
) {
    memset(statement, 0, sizeof(*statement));
    statement->engine = database->engine;
    if (database->engine == ENGINE_SQLITE) {
        return sqlite3_prepare_v2(database->sqlite, sql, -1, &statement->sqlite, NULL);
    }
    return csgdb_prepare_v3(
        database->csgdb,
        sql,
        -1,
        CSGDB_PREPARE_PERSISTENT,
        &statement->csgdb,
        NULL
    );
}

static int statement_finalize(statement_handle *statement) {
    if (statement->engine == ENGINE_SQLITE) {
        return sqlite3_finalize(statement->sqlite);
    }
    return csgdb_finalize(statement->csgdb);
}

static int statement_reset(statement_handle *statement) {
    int result;
    if (statement->engine == ENGINE_SQLITE) {
        result = sqlite3_reset(statement->sqlite);
        if (result == SQLITE_OK_LOCAL) {
            result = sqlite3_clear_bindings(statement->sqlite);
        }
        return result;
    }
    result = csgdb_reset(statement->csgdb);
    if (result == CSGDB_OK) {
        result = csgdb_clear_bindings(statement->csgdb);
    }
    return result;
}

static int statement_bind_i64(statement_handle *statement, int index, int64_t value) {
    if (statement->engine == ENGINE_SQLITE) {
        return sqlite3_bind_int64(statement->sqlite, index, value);
    }
    return csgdb_bind_int64(statement->csgdb, index, value);
}

static int statement_bind_double(statement_handle *statement, int index, double value) {
    if (statement->engine == ENGINE_SQLITE) {
        return sqlite3_bind_double(statement->sqlite, index, value);
    }
    return csgdb_bind_double(statement->csgdb, index, value);
}

static int statement_bind_blob(
    statement_handle *statement,
    int index,
    const void *value,
    size_t length
) {
    if (statement->engine == ENGINE_SQLITE) {
        if (length > INT32_MAX) {
            return 1;
        }
        return sqlite3_bind_blob(
            statement->sqlite,
            index,
            value,
            (int)length,
            SQLITE_TRANSIENT_LOCAL
        );
    }
    return csgdb_bind_blob(statement->csgdb, index, value, length);
}

static int statement_step(statement_handle *statement) {
    if (statement->engine == ENGINE_SQLITE) {
        return sqlite3_step(statement->sqlite);
    }
    return csgdb_step(statement->csgdb);
}

static int64_t statement_column_i64(statement_handle *statement, int column) {
    if (statement->engine == ENGINE_SQLITE) {
        return sqlite3_column_int64(statement->sqlite, column);
    }
    return csgdb_column_int64(statement->csgdb, column);
}

static double statement_column_double(statement_handle *statement, int column) {
    if (statement->engine == ENGINE_SQLITE) {
        return sqlite3_column_double(statement->sqlite, column);
    }
    return csgdb_column_double(statement->csgdb, column);
}

static int statement_column_bytes(statement_handle *statement, int column) {
    if (statement->engine == ENGINE_SQLITE) {
        return sqlite3_column_bytes(statement->sqlite, column);
    }
    return csgdb_column_bytes(statement->csgdb, column);
}

static const unsigned char *statement_column_text(statement_handle *statement, int column) {
    if (statement->engine == ENGINE_SQLITE) {
        return sqlite3_column_text(statement->sqlite, column);
    }
    return csgdb_column_text(statement->csgdb, column);
}

static int expect_ok(database_handle *database, int result, const char *operation) {
    if (result == SQLITE_OK_LOCAL) {
        return 1;
    }
    fprintf(
        stderr,
        "%s: %s failed (%d): %s\n",
        engine_name(database->engine),
        operation,
        result,
        database_error(database)
    );
    return 0;
}

static void fill_payload(uint8_t payload[PAYLOAD_BYTES]) {
    for (size_t index = 0; index < PAYLOAD_BYTES; ++index) {
        payload[index] = (uint8_t)((index * 37U + 11U) & 0xffU);
    }
}

static int initialize_schema(database_handle *database) {
    return expect_ok(
        database,
        database_exec(
            database,
            "PRAGMA journal_mode=WAL;"
            "PRAGMA synchronous=NORMAL;"
            "PRAGMA temp_store=MEMORY;"
            "PRAGMA wal_autocheckpoint=1000;"
            "CREATE TABLE bench("
            "id INTEGER PRIMARY KEY,"
            "tenant INTEGER NOT NULL,"
            "score REAL NOT NULL,"
            "payload BLOB NOT NULL"
            ");"
            "CREATE INDEX idx_bench_tenant_id ON bench(tenant,id);"
            "CREATE TABLE durable(id INTEGER PRIMARY KEY, value INTEGER NOT NULL);"
        ),
        "initialize schema"
    );
}

static int read_engine_version(database_handle *database, char output[32]) {
    statement_handle statement;
    if (!expect_ok(
            database,
            statement_prepare(database, "SELECT sqlite_version()", &statement),
            "prepare engine version"
        )) {
        return 0;
    }
    if (statement_step(&statement) != SQLITE_ROW_LOCAL) {
        statement_finalize(&statement);
        return 0;
    }
    const unsigned char *version = statement_column_text(&statement, 0);
    if (version == NULL) {
        statement_finalize(&statement);
        return 0;
    }
    snprintf(output, 32, "%s", (const char *)version);
    if (statement_step(&statement) != SQLITE_DONE_LOCAL) {
        statement_finalize(&statement);
        return 0;
    }
    return expect_ok(database, statement_finalize(&statement), "finalize engine version");
}

static int verify_integrity(database_handle *database) {
    statement_handle statement;
    if (!expect_ok(
            database,
            statement_prepare(database, "PRAGMA integrity_check", &statement),
            "prepare integrity check"
        )) {
        return 0;
    }
    int result = statement_step(&statement);
    const unsigned char *value = result == SQLITE_ROW_LOCAL
        ? statement_column_text(&statement, 0)
        : NULL;
    int ok = value != NULL && strcmp((const char *)value, "ok") == 0 &&
        statement_step(&statement) == SQLITE_DONE_LOCAL;
    if (statement_finalize(&statement) != SQLITE_OK_LOCAL) {
        ok = 0;
    }
    if (!ok) {
        fprintf(stderr, "%s: integrity check did not return ok\n", engine_name(database->engine));
    }
    return ok;
}

static int run_bulk_insert(
    database_handle *database,
    uint64_t rows,
    double *elapsed_ms
) {
    uint8_t payload[PAYLOAD_BYTES];
    fill_payload(payload);
    statement_handle statement;
    if (!expect_ok(database, database_exec(database, "BEGIN IMMEDIATE"), "begin bulk insert")) {
        return 0;
    }
    if (!expect_ok(
            database,
            statement_prepare(
                database,
                "INSERT INTO bench(id,tenant,score,payload) VALUES(?,?,?,?)",
                &statement
            ),
            "prepare bulk insert"
        )) {
        database_exec(database, "ROLLBACK");
        return 0;
    }
    uint64_t start = monotonic_ns();
    for (uint64_t row = 1; row <= rows; ++row) {
        int ok = statement_bind_i64(&statement, 1, (int64_t)row) == SQLITE_OK_LOCAL &&
            statement_bind_i64(&statement, 2, (int64_t)(row % TENANTS)) == SQLITE_OK_LOCAL &&
            statement_bind_double(&statement, 3, (double)(row % 10000U) / 10000.0) ==
                SQLITE_OK_LOCAL &&
            statement_bind_blob(&statement, 4, payload, sizeof(payload)) == SQLITE_OK_LOCAL &&
            statement_step(&statement) == SQLITE_DONE_LOCAL &&
            statement_reset(&statement) == SQLITE_OK_LOCAL;
        if (!ok) {
            fprintf(stderr, "%s: bulk insert failed at row %llu: %s\n",
                engine_name(database->engine),
                (unsigned long long)row,
                database_error(database));
            statement_finalize(&statement);
            database_exec(database, "ROLLBACK");
            return 0;
        }
    }
    if (!expect_ok(database, statement_finalize(&statement), "finalize bulk insert") ||
        !expect_ok(database, database_exec(database, "COMMIT"), "commit bulk insert")) {
        return 0;
    }
    *elapsed_ms = (double)(monotonic_ns() - start) / 1000000.0;
    return expect_ok(
        database,
        database_exec(database, "PRAGMA wal_checkpoint(TRUNCATE);ANALYZE"),
        "checkpoint bulk insert"
    );
}

static int run_durable_autocommit(
    database_handle *database,
    uint64_t operations,
    double *elapsed_ms
) {
    statement_handle statement;
    if (!expect_ok(database, database_exec(database, "PRAGMA synchronous=FULL"), "set full sync") ||
        !expect_ok(
            database,
            statement_prepare(database, "INSERT INTO durable(value) VALUES(?)", &statement),
            "prepare durable insert"
        )) {
        return 0;
    }
    uint64_t start = monotonic_ns();
    for (uint64_t operation = 0; operation < operations; ++operation) {
        int ok = statement_bind_i64(&statement, 1, (int64_t)operation) == SQLITE_OK_LOCAL &&
            statement_step(&statement) == SQLITE_DONE_LOCAL &&
            statement_reset(&statement) == SQLITE_OK_LOCAL;
        if (!ok) {
            fprintf(stderr, "%s: durable insert failed: %s\n",
                engine_name(database->engine), database_error(database));
            statement_finalize(&statement);
            return 0;
        }
    }
    *elapsed_ms = (double)(monotonic_ns() - start) / 1000000.0;
    if (!expect_ok(database, statement_finalize(&statement), "finalize durable insert")) {
        return 0;
    }
    return expect_ok(
        database,
        database_exec(database, "PRAGMA synchronous=NORMAL"),
        "restore normal sync"
    );
}

static int run_point_reads(
    database_handle *database,
    uint64_t row_count,
    uint64_t operations,
    double *elapsed_ms,
    uint64_t *checksum
) {
    statement_handle statement;
    if (!expect_ok(
            database,
            statement_prepare(database, "SELECT score,payload FROM bench WHERE id=?", &statement),
            "prepare point read"
        )) {
        return 0;
    }
    uint64_t seed = 0x9e3779b97f4a7c15ULL;
    uint64_t sum = 0;
    uint64_t start = monotonic_ns();
    for (uint64_t operation = 0; operation < operations; ++operation) {
        int64_t id = (int64_t)(xorshift64(&seed) % row_count + 1U);
        if (statement_bind_i64(&statement, 1, id) != SQLITE_OK_LOCAL ||
            statement_step(&statement) != SQLITE_ROW_LOCAL) {
            fprintf(stderr, "%s: point read failed: %s\n",
                engine_name(database->engine), database_error(database));
            statement_finalize(&statement);
            return 0;
        }
        sum += (uint64_t)(statement_column_double(&statement, 0) * 1000000.0);
        sum += (uint64_t)statement_column_bytes(&statement, 1);
        if (statement_step(&statement) != SQLITE_DONE_LOCAL ||
            statement_reset(&statement) != SQLITE_OK_LOCAL) {
            fprintf(stderr, "%s: point read reset failed: %s\n",
                engine_name(database->engine), database_error(database));
            statement_finalize(&statement);
            return 0;
        }
    }
    *elapsed_ms = (double)(monotonic_ns() - start) / 1000000.0;
    *checksum = sum;
    return expect_ok(database, statement_finalize(&statement), "finalize point read");
}

static int run_range_scans(
    database_handle *database,
    uint64_t row_count,
    uint64_t operations,
    double *elapsed_ms,
    uint64_t *rows_read,
    uint64_t *checksum
) {
    statement_handle statement;
    if (!expect_ok(
            database,
            statement_prepare(
                database,
                "SELECT id,payload FROM bench WHERE tenant=? AND id>=? ORDER BY id LIMIT 100",
                &statement
            ),
            "prepare range scan"
        )) {
        return 0;
    }
    uint64_t seed = 0xd1b54a32d192ed03ULL;
    uint64_t count = 0;
    uint64_t sum = 0;
    uint64_t start = monotonic_ns();
    for (uint64_t operation = 0; operation < operations; ++operation) {
        uint64_t random = xorshift64(&seed);
        int64_t tenant = (int64_t)(random % TENANTS);
        int64_t start_id = (int64_t)(xorshift64(&seed) % (row_count / 2U) + 1U);
        if (statement_bind_i64(&statement, 1, tenant) != SQLITE_OK_LOCAL ||
            statement_bind_i64(&statement, 2, start_id) != SQLITE_OK_LOCAL) {
            statement_finalize(&statement);
            return 0;
        }
        int result;
        while ((result = statement_step(&statement)) == SQLITE_ROW_LOCAL) {
            sum += (uint64_t)statement_column_i64(&statement, 0);
            sum += (uint64_t)statement_column_bytes(&statement, 1);
            ++count;
        }
        if (result != SQLITE_DONE_LOCAL || statement_reset(&statement) != SQLITE_OK_LOCAL) {
            fprintf(stderr, "%s: range scan failed: %s\n",
                engine_name(database->engine), database_error(database));
            statement_finalize(&statement);
            return 0;
        }
    }
    *elapsed_ms = (double)(monotonic_ns() - start) / 1000000.0;
    *rows_read = count;
    *checksum += sum;
    return expect_ok(database, statement_finalize(&statement), "finalize range scan");
}

static int run_updates(
    database_handle *database,
    uint64_t row_count,
    uint64_t operations,
    double *elapsed_ms
) {
    statement_handle statement;
    if (!expect_ok(database, database_exec(database, "BEGIN IMMEDIATE"), "begin update") ||
        !expect_ok(
            database,
            statement_prepare(database, "UPDATE bench SET score=? WHERE id=?", &statement),
            "prepare update"
        )) {
        database_exec(database, "ROLLBACK");
        return 0;
    }
    uint64_t seed = 0x94d049bb133111ebULL;
    uint64_t start = monotonic_ns();
    for (uint64_t operation = 0; operation < operations; ++operation) {
        int64_t id = (int64_t)(xorshift64(&seed) % row_count + 1U);
        double score = (double)(operation % 10000U) / 10000.0;
        if (statement_bind_double(&statement, 1, score) != SQLITE_OK_LOCAL ||
            statement_bind_i64(&statement, 2, id) != SQLITE_OK_LOCAL ||
            statement_step(&statement) != SQLITE_DONE_LOCAL ||
            statement_reset(&statement) != SQLITE_OK_LOCAL) {
            fprintf(stderr, "%s: update failed: %s\n",
                engine_name(database->engine), database_error(database));
            statement_finalize(&statement);
            database_exec(database, "ROLLBACK");
            return 0;
        }
    }
    if (!expect_ok(database, statement_finalize(&statement), "finalize update") ||
        !expect_ok(database, database_exec(database, "COMMIT"), "commit update")) {
        return 0;
    }
    *elapsed_ms = (double)(monotonic_ns() - start) / 1000000.0;
    return 1;
}

static int samples_initialize(latency_samples *samples, size_t capacity) {
    samples->values = calloc(capacity, sizeof(*samples->values));
    samples->length = 0;
    samples->capacity = samples->values == NULL ? 0 : capacity;
    samples->total = 0;
    return samples->values != NULL;
}

static void samples_record(latency_samples *samples, uint64_t nanoseconds) {
    if (samples->length < samples->capacity) {
        samples->values[samples->length++] = nanoseconds;
    } else if (samples->capacity > 0) {
        samples->values[samples->total % samples->capacity] = nanoseconds;
    }
    ++samples->total;
}

static int compare_u64(const void *left, const void *right) {
    uint64_t a = *(const uint64_t *)left;
    uint64_t b = *(const uint64_t *)right;
    return (a > b) - (a < b);
}

static double samples_percentile(latency_samples *samples, double percentile) {
    if (samples->length == 0) {
        return 0.0;
    }
    qsort(samples->values, samples->length, sizeof(*samples->values), compare_u64);
    size_t index = (size_t)(percentile * (double)(samples->length - 1U));
    return (double)samples->values[index] / 1000.0;
}

static void samples_free(latency_samples *samples) {
    free(samples->values);
    samples->values = NULL;
    samples->length = 0;
    samples->capacity = 0;
    samples->total = 0;
}

static void *reader_main(void *raw_context) {
    reader_context *context = raw_context;
    statement_handle statement;
    if (statement_prepare(
            &context->database,
            "SELECT score,payload FROM bench WHERE id=?",
            &statement
        ) != SQLITE_OK_LOCAL) {
        ++context->errors;
        return NULL;
    }
    while (!atomic_load_explicit(&context->control->start, memory_order_acquire)) {
    }
    while (!atomic_load_explicit(&context->control->stop, memory_order_acquire)) {
        int64_t id = (int64_t)(xorshift64(&context->seed) % context->row_count + 1U);
        uint64_t start = monotonic_ns();
        int result = statement_bind_i64(&statement, 1, id);
        if (result == SQLITE_OK_LOCAL) {
            result = statement_step(&statement);
        }
        if (result == SQLITE_ROW_LOCAL) {
            context->checksum +=
                (uint64_t)(statement_column_double(&statement, 0) * 1000000.0);
            context->checksum += (uint64_t)statement_column_bytes(&statement, 1);
            result = statement_step(&statement);
        }
        if (result == SQLITE_DONE_LOCAL && statement_reset(&statement) == SQLITE_OK_LOCAL) {
            ++context->operations;
            samples_record(&context->samples, monotonic_ns() - start);
        } else {
            ++context->errors;
            statement_reset(&statement);
        }
    }
    if (statement_finalize(&statement) != SQLITE_OK_LOCAL) {
        ++context->errors;
    }
    return NULL;
}

static void *writer_main(void *raw_context) {
    writer_context *context = raw_context;
    statement_handle statement;
    if (statement_prepare(
            &context->database,
            "UPDATE bench SET score=score+0.000001 WHERE id=?",
            &statement
        ) != SQLITE_OK_LOCAL) {
        ++context->errors;
        return NULL;
    }
    while (!atomic_load_explicit(&context->control->start, memory_order_acquire)) {
    }
    while (!atomic_load_explicit(&context->control->stop, memory_order_acquire)) {
        uint64_t start = monotonic_ns();
        if (database_exec(&context->database, "BEGIN IMMEDIATE") != SQLITE_OK_LOCAL) {
            ++context->errors;
            continue;
        }
        int failed = 0;
        for (uint64_t row = 0; row < WRITE_BATCH; ++row) {
            int64_t id = (int64_t)(xorshift64(&context->seed) % context->row_count + 1U);
            if (statement_bind_i64(&statement, 1, id) != SQLITE_OK_LOCAL ||
                statement_step(&statement) != SQLITE_DONE_LOCAL ||
                statement_reset(&statement) != SQLITE_OK_LOCAL) {
                failed = 1;
                break;
            }
        }
        if (failed || database_exec(&context->database, "COMMIT") != SQLITE_OK_LOCAL) {
            database_exec(&context->database, "ROLLBACK");
            ++context->errors;
            statement_reset(&statement);
            continue;
        }
        context->rows += WRITE_BATCH;
        ++context->commits;
        samples_record(&context->samples, monotonic_ns() - start);
    }
    if (statement_finalize(&statement) != SQLITE_OK_LOCAL) {
        ++context->errors;
    }
    return NULL;
}

static int run_stress(
    engine_kind engine,
    const char *path,
    uint64_t row_count,
    unsigned seconds,
    benchmark_result *result
) {
    reader_context readers[READER_THREADS];
    writer_context writer;
    pthread_t reader_threads[READER_THREADS];
    pthread_t writer_thread;
    stress_control control;
    atomic_init(&control.start, 0);
    atomic_init(&control.stop, 0);
    memset(readers, 0, sizeof(readers));
    memset(&writer, 0, sizeof(writer));

    for (size_t index = 0; index < READER_THREADS; ++index) {
        readers[index].control = &control;
        readers[index].seed = 0x243f6a8885a308d3ULL ^ ((uint64_t)index << 32);
        readers[index].row_count = row_count;
        if (database_open(&readers[index].database, engine, path) != SQLITE_OK_LOCAL ||
            !samples_initialize(&readers[index].samples, READER_SAMPLE_CAPACITY)) {
            fprintf(stderr, "%s: could not initialize stress reader\n", engine_name(engine));
            return 0;
        }
    }
    writer.control = &control;
    writer.seed = 0x13198a2e03707344ULL;
    writer.row_count = row_count;
    if (database_open(&writer.database, engine, path) != SQLITE_OK_LOCAL ||
        !samples_initialize(&writer.samples, WRITER_SAMPLE_CAPACITY)) {
        fprintf(stderr, "%s: could not initialize stress writer\n", engine_name(engine));
        return 0;
    }

    for (size_t index = 0; index < READER_THREADS; ++index) {
        if (pthread_create(&reader_threads[index], NULL, reader_main, &readers[index]) != 0) {
            fprintf(stderr, "%s: could not create reader thread\n", engine_name(engine));
            return 0;
        }
    }
    if (pthread_create(&writer_thread, NULL, writer_main, &writer) != 0) {
        fprintf(stderr, "%s: could not create writer thread\n", engine_name(engine));
        return 0;
    }

    uint64_t start = monotonic_ns();
    atomic_store_explicit(&control.start, 1, memory_order_release);
    struct timespec duration = {.tv_sec = (time_t)seconds, .tv_nsec = 0};
    while (nanosleep(&duration, &duration) != 0) {
    }
    atomic_store_explicit(&control.stop, 1, memory_order_release);
    for (size_t index = 0; index < READER_THREADS; ++index) {
        pthread_join(reader_threads[index], NULL);
    }
    pthread_join(writer_thread, NULL);
    result->stress_seconds = (double)(monotonic_ns() - start) / 1000000000.0;

    size_t combined_capacity = 0;
    for (size_t index = 0; index < READER_THREADS; ++index) {
        combined_capacity += readers[index].samples.length;
    }
    latency_samples combined;
    if (!samples_initialize(&combined, combined_capacity)) {
        return 0;
    }
    for (size_t index = 0; index < READER_THREADS; ++index) {
        memcpy(
            combined.values + combined.length,
            readers[index].samples.values,
            readers[index].samples.length * sizeof(*combined.values)
        );
        combined.length += readers[index].samples.length;
        result->stress_reads += readers[index].operations;
        result->stress_errors += readers[index].errors;
        result->checksum += readers[index].checksum;
    }
    result->stress_writes = writer.rows;
    result->stress_commits = writer.commits;
    result->stress_errors += writer.errors;
    result->read_p50_us = samples_percentile(&combined, 0.50);
    result->read_p95_us = samples_percentile(&combined, 0.95);
    result->read_p99_us = samples_percentile(&combined, 0.99);
    result->commit_p50_us = samples_percentile(&writer.samples, 0.50);
    result->commit_p95_us = samples_percentile(&writer.samples, 0.95);
    result->commit_p99_us = samples_percentile(&writer.samples, 0.99);

    samples_free(&combined);
    for (size_t index = 0; index < READER_THREADS; ++index) {
        samples_free(&readers[index].samples);
        database_close(&readers[index].database);
    }
    samples_free(&writer.samples);
    database_close(&writer.database);
    return 1;
}

static uint64_t database_size(const char *path) {
    struct stat details;
    if (stat(path, &details) != 0 || details.st_size < 0) {
        return 0;
    }
    return (uint64_t)details.st_size;
}

static int database_files_are_absent(const char *path) {
    struct stat details;
    char sidecar[4096];
    if (stat(path, &details) == 0) {
        return 0;
    }
    int length = snprintf(sidecar, sizeof(sidecar), "%s-wal", path);
    if (length < 0 || (size_t)length >= sizeof(sidecar) || stat(sidecar, &details) == 0) {
        return 0;
    }
    length = snprintf(sidecar, sizeof(sidecar), "%s-shm", path);
    return length >= 0 && (size_t)length < sizeof(sidecar) && stat(sidecar, &details) != 0;
}

static int parse_engine(const char *value, engine_kind *engine) {
    if (strcmp(value, "sqlite") == 0) {
        *engine = ENGINE_SQLITE;
        return 1;
    }
    if (strcmp(value, "csgdb-plaintext") == 0) {
        *engine = ENGINE_CSGDB_PLAINTEXT;
        return 1;
    }
    if (strcmp(value, "csgdb-encrypted") == 0) {
        *engine = ENGINE_CSGDB_ENCRYPTED;
        return 1;
    }
    return 0;
}

static void print_result(
    engine_kind engine,
    uint64_t row_count,
    uint64_t point_operations,
    uint64_t range_operations,
    uint64_t update_operations,
    uint64_t durable_operations,
    const benchmark_result *result
) {
    printf(
        "{\"engine\":\"%s\",\"engine_sqlite_version\":\"%s\","
        "\"system_sqlite_version\":\"%s\",\"csgdb_version\":\"%s\","
        "\"cache_bytes_per_connection\":%llu,\"rows\":%llu,"
        "\"point_operations\":%llu,\"range_operations\":%llu,"
        "\"update_operations\":%llu,\"durable_operations\":%llu,"
        "\"open_ms\":%.3f,\"bulk_insert_ms\":%.3f,"
        "\"durable_autocommit_ms\":%.3f,\"point_read_ms\":%.3f,"
        "\"range_scan_ms\":%.3f,\"range_rows\":%llu,"
        "\"update_ms\":%.3f,\"stress_seconds\":%.3f,"
        "\"stress_reads\":%llu,\"stress_writes\":%llu,"
        "\"stress_commits\":%llu,\"stress_errors\":%llu,"
        "\"read_p50_us\":%.3f,\"read_p95_us\":%.3f,\"read_p99_us\":%.3f,"
        "\"commit_p50_us\":%.3f,\"commit_p95_us\":%.3f,"
        "\"commit_p99_us\":%.3f,\"database_bytes\":%llu,"
        "\"max_rss_kib\":%llu,\"integrity_ok\":%s,\"checksum\":%llu}\n",
        engine_name(engine),
        result->engine_sqlite_version,
        sqlite3_libversion(),
        csgdb_libversion(),
        (unsigned long long)CACHE_SIZE_BYTES,
        (unsigned long long)row_count,
        (unsigned long long)point_operations,
        (unsigned long long)range_operations,
        (unsigned long long)update_operations,
        (unsigned long long)durable_operations,
        result->open_ms,
        result->bulk_insert_ms,
        result->durable_autocommit_ms,
        result->point_read_ms,
        result->range_scan_ms,
        (unsigned long long)result->range_rows,
        result->update_ms,
        result->stress_seconds,
        (unsigned long long)result->stress_reads,
        (unsigned long long)result->stress_writes,
        (unsigned long long)result->stress_commits,
        (unsigned long long)result->stress_errors,
        result->read_p50_us,
        result->read_p95_us,
        result->read_p99_us,
        result->commit_p50_us,
        result->commit_p95_us,
        result->commit_p99_us,
        (unsigned long long)result->database_bytes,
        (unsigned long long)result->max_rss_kib,
        result->integrity_ok ? "true" : "false",
        (unsigned long long)result->checksum
    );
}

int main(int argc, char **argv) {
    if (argc == 4 && strcmp(argv[1], "verify") == 0) {
        engine_kind verify_engine;
        struct stat details;
        if (!parse_engine(argv[2], &verify_engine) || stat(argv[3], &details) != 0) {
            fprintf(stderr, "usage: %s verify ENGINE EXISTING_DATABASE.db\n", argv[0]);
            return 2;
        }
        database_handle verify_database;
        if (database_open(&verify_database, verify_engine, argv[3]) != SQLITE_OK_LOCAL) {
            fprintf(stderr, "%s: verify open failed\n", engine_name(verify_engine));
            return 1;
        }
        int ok = verify_integrity(&verify_database);
        if (database_close(&verify_database) != SQLITE_OK_LOCAL) {
            ok = 0;
        }
        printf(
            "{\"engine\":\"%s\",\"integrity_ok\":%s}\n",
            engine_name(verify_engine),
            ok ? "true" : "false"
        );
        return ok ? 0 : 1;
    }
    if (argc != 9) {
        fprintf(
            stderr,
            "usage: %s ENGINE DATABASE.db ROWS POINT_READS RANGE_SCANS UPDATES "
            "DURABLE_WRITES STRESS_SECONDS\n"
            "       %s verify ENGINE EXISTING_DATABASE.db\n",
            argv[0],
            argv[0]
        );
        return 2;
    }
    engine_kind engine;
    if (!parse_engine(argv[1], &engine)) {
        fprintf(stderr, "unknown engine: %s\n", argv[1]);
        return 2;
    }
    const char *path = argv[2];
    uint64_t rows = strtoull(argv[3], NULL, 10);
    uint64_t point_operations = strtoull(argv[4], NULL, 10);
    uint64_t range_operations = strtoull(argv[5], NULL, 10);
    uint64_t update_operations = strtoull(argv[6], NULL, 10);
    uint64_t durable_operations = strtoull(argv[7], NULL, 10);
    unsigned stress_seconds = (unsigned)strtoul(argv[8], NULL, 10);
    if (rows < 1000 || rows > 10000000 || point_operations == 0 ||
        range_operations == 0 || update_operations == 0 ||
        durable_operations == 0 || stress_seconds == 0) {
        fprintf(stderr, "invalid benchmark scale\n");
        return 2;
    }

    if (!database_files_are_absent(path)) {
        fprintf(stderr, "refusing to overwrite existing database or sidecar: %s\n", path);
        return 2;
    }
    benchmark_result result;
    memset(&result, 0, sizeof(result));
    database_handle database;
    uint64_t open_start = monotonic_ns();
    if (database_open(&database, engine, path) != SQLITE_OK_LOCAL) {
        fprintf(stderr, "%s: open failed: %s\n", engine_name(engine), database_error(&database));
        return 1;
    }
    result.open_ms = (double)(monotonic_ns() - open_start) / 1000000.0;
    if (!read_engine_version(&database, result.engine_sqlite_version) ||
        !initialize_schema(&database) ||
        !run_bulk_insert(&database, rows, &result.bulk_insert_ms) ||
        !run_durable_autocommit(&database, durable_operations, &result.durable_autocommit_ms) ||
        !run_point_reads(
            &database,
            rows,
            point_operations,
            &result.point_read_ms,
            &result.checksum
        ) ||
        !run_range_scans(
            &database,
            rows,
            range_operations,
            &result.range_scan_ms,
            &result.range_rows,
            &result.checksum
        ) ||
        !run_updates(&database, rows, update_operations, &result.update_ms) ||
        !expect_ok(&database, database_close(&database), "close before stress") ||
        !run_stress(engine, path, rows, stress_seconds, &result) ||
        database_open(&database, engine, path) != SQLITE_OK_LOCAL ||
        !verify_integrity(&database) ||
        !expect_ok(&database, database_close(&database), "close after integrity check")) {
        return 1;
    }
    result.integrity_ok = 1;
    result.database_bytes = database_size(path);
    struct rusage usage;
    if (getrusage(RUSAGE_SELF, &usage) == 0 && usage.ru_maxrss > 0) {
        result.max_rss_kib = (uint64_t)usage.ru_maxrss;
    }
    print_result(
        engine,
        rows,
        point_operations,
        range_operations,
        update_operations,
        durable_operations,
        &result
    );
    return 0;
}
