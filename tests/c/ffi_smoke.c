#include "csgdb.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int fail(csgdb *db, const char *operation, int32_t code) {
    fprintf(
        stderr,
        "%s failed (%d): %s\n",
        operation,
        code,
        db == NULL ? csgdb_errstr(code) : csgdb_errmsg(db)
    );
    csgdb_close(db);
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s DATABASE.db\n", argv[0]);
        return 2;
    }

    uint8_t key[32] = {0};
    csgdb *db = NULL;
    int32_t rc = csgdb_open_with_key(argv[1], key, sizeof(key), &db);
    if (rc != CSGDB_OK) {
        return fail(db, "open", rc);
    }

    rc = csgdb_exec(
        db,
        "CREATE TABLE smoke("
        "id INTEGER PRIMARY KEY,"
        "value TEXT NOT NULL,"
        "payload BLOB NOT NULL"
        ");"
    );
    if (rc != CSGDB_OK) {
        return fail(db, "exec", rc);
    }

    const char *insert_sql =
        "INSERT INTO smoke(id, value, payload) VALUES (?, ?, ?)";
    csgdb_stmt *statement = NULL;
    rc = csgdb_prepare_v2(db, insert_sql, -1, &statement, NULL);
    if (rc != CSGDB_OK) {
        return fail(db, "prepare insert", rc);
    }

    const unsigned char payload[] = {0, 1, 2, 255};
    if (csgdb_bind_int64(statement, 1, 7) != CSGDB_OK ||
        csgdb_bind_text(statement, 2, "ffi-ok", -1) != CSGDB_OK ||
        csgdb_bind_blob(statement, 3, payload, sizeof(payload)) != CSGDB_OK ||
        csgdb_step(statement) != CSGDB_DONE) {
        csgdb_finalize(statement);
        return fail(db, "bind/insert", csgdb_errcode(db));
    }
    rc = csgdb_finalize(statement);
    if (rc != CSGDB_OK) {
        return fail(db, "finalize insert", rc);
    }
    if (csgdb_changes64(db) != 1 ||
        csgdb_total_changes64(db) < 1 ||
        csgdb_last_insert_rowid(db) != 7 ||
        csgdb_get_autocommit(db) == 0 ||
        csgdb_db_readonly(db, "main") != 0 ||
        csgdb_txn_state(db, NULL) != CSGDB_TXN_NONE) {
        return fail(db, "connection state", csgdb_errcode(db));
    }
    rc = csgdb_busy_timeout(db, 20);
    if (rc != CSGDB_OK) {
        return fail(db, "busy timeout", rc);
    }
    rc = csgdb_wal_autocheckpoint(db, 0);
    if (rc != CSGDB_OK) {
        return fail(db, "wal autocheckpoint", rc);
    }

    const char *select_sql =
        "SELECT id, value, payload FROM smoke WHERE id = ?";
    statement = NULL;
    rc = csgdb_prepare_v3(
        db,
        select_sql,
        -1,
        CSGDB_PREPARE_PERSISTENT,
        &statement,
        NULL
    );
    if (rc != CSGDB_OK || csgdb_bind_int(statement, 1, 7) != CSGDB_OK) {
        csgdb_finalize(statement);
        return fail(db, "prepare select", csgdb_errcode(db));
    }
    if (csgdb_step(statement) != CSGDB_ROW) {
        csgdb_finalize(statement);
        return fail(db, "step select", csgdb_errcode(db));
    }
    const void *blob_view = (const void *)(uintptr_t)1;
    size_t blob_view_len = SIZE_MAX;
    rc = csgdb_column_blob_view(statement, 3, &blob_view, &blob_view_len);
    if (rc != CSGDB_RANGE || blob_view != NULL || blob_view_len != 0) {
        csgdb_finalize(statement);
        return fail(db, "invalid blob view", rc);
    }
    rc = csgdb_column_blob_view(statement, 2, &blob_view, &blob_view_len);
    if (rc != CSGDB_OK || blob_view_len != sizeof(payload) ||
        memcmp(blob_view, payload, sizeof(payload)) != 0 ||
        csgdb_column_count(statement) != 3 ||
        csgdb_column_type(statement, 0) != CSGDB_INTEGER ||
        csgdb_column_int64(statement, 0) != 7 ||
        csgdb_column_type(statement, 1) != CSGDB_TEXT ||
        csgdb_column_bytes(statement, 1) != 6 ||
        memcmp(csgdb_column_text(statement, 1), "ffi-ok", 6) != 0 ||
        csgdb_column_type(statement, 2) != CSGDB_BLOB ||
        csgdb_column_bytes(statement, 2) != (int32_t)sizeof(payload) ||
        memcmp(csgdb_column_blob(statement, 2), payload, sizeof(payload)) != 0 ||
        csgdb_step(statement) != CSGDB_DONE) {
        csgdb_finalize(statement);
        return fail(db, "select", csgdb_errcode(db));
    }
    rc = csgdb_reset_and_clear_bindings(statement);
    if (rc != CSGDB_OK) {
        csgdb_finalize(statement);
        return fail(db, "fused reset", rc);
    }
    rc = csgdb_step(statement);
    if (rc != CSGDB_DONE) {
        csgdb_finalize(statement);
        return fail(db, "cleared binding", rc);
    }
    rc = csgdb_reset_and_clear_bindings(statement);
    if (rc != CSGDB_OK) {
        csgdb_finalize(statement);
        return fail(db, "second fused reset", rc);
    }
    rc = csgdb_finalize(statement);
    if (rc != CSGDB_OK) {
        return fail(db, "finalize select", rc);
    }
    rc = csgdb_release_memory(db);
    if (rc != CSGDB_OK) {
        return fail(db, "release memory", rc);
    }
    int32_t wal_frames = -1;
    int32_t checkpointed_frames = -1;
    rc = csgdb_wal_checkpoint_v2(
        db,
        "main",
        CSGDB_CHECKPOINT_TRUNCATE,
        &wal_frames,
        &checkpointed_frames
    );
    if (rc != CSGDB_OK ||
        wal_frames < 0 ||
        wal_frames != checkpointed_frames) {
        return fail(db, "wal checkpoint", rc);
    }

    rc = csgdb_close(db);
    if (rc != CSGDB_OK) {
        return fail(NULL, "close", rc);
    }
    return 0;
}
