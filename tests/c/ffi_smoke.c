#include "csgdb.h"

#include <stdint.h>
#include <stdio.h>

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
        "CREATE TABLE smoke(id INTEGER PRIMARY KEY, value TEXT NOT NULL);"
        "INSERT INTO smoke(value) VALUES ('ffi-ok');"
    );
    if (rc != CSGDB_OK) {
        return fail(db, "exec", rc);
    }

    rc = csgdb_close(db);
    if (rc != CSGDB_OK) {
        return fail(NULL, "close", rc);
    }
    return 0;
}
