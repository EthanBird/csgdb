#ifndef CSGDB_H
#define CSGDB_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(CSGDB_BUILD_SHARED)
#define CSGDB_API __declspec(dllexport)
#elif defined(_WIN32)
#define CSGDB_API __declspec(dllimport)
#elif defined(__GNUC__) || defined(__clang__)
#define CSGDB_API __attribute__((visibility("default")))
#else
#define CSGDB_API
#endif

#define CSGDB_ABI_VERSION 1u

#define CSGDB_OK 0
#define CSGDB_INVALID_ARGUMENT 1
#define CSGDB_INVALID_OPEN_FLAGS 2
#define CSGDB_INVALID_KEY 3
#define CSGDB_KEY_REQUIRED 4
#define CSGDB_KEYSTORE_UNAVAILABLE 5
#define CSGDB_BUSY 6
#define CSGDB_READONLY 7
#define CSGDB_CONSTRAINT 8
#define CSGDB_CORRUPT 9
#define CSGDB_STORAGE 10
#define CSGDB_MISUSE 11
#define CSGDB_RANGE 12
#define CSGDB_INTERRUPT 13

#define CSGDB_TXN_NONE 0
#define CSGDB_TXN_READ 1
#define CSGDB_TXN_WRITE 2

#define CSGDB_CHECKPOINT_PASSIVE 0
#define CSGDB_CHECKPOINT_FULL 1
#define CSGDB_CHECKPOINT_RESTART 2
#define CSGDB_CHECKPOINT_TRUNCATE 3

#define CSGDB_INTEGER 1
#define CSGDB_FLOAT 2
#define CSGDB_TEXT 3
#define CSGDB_BLOB 4
#define CSGDB_NULL 5
#define CSGDB_ROW 100
#define CSGDB_DONE 101

#define CSGDB_PREPARE_PERSISTENT 0x01u

#define CSGDB_OPEN_READONLY  0x0001u
#define CSGDB_OPEN_READWRITE 0x0002u
#define CSGDB_OPEN_CREATE    0x0004u
#define CSGDB_OPEN_URI       0x0008u
#define CSGDB_OPEN_MEMORY    0x0010u
#define CSGDB_OPEN_ENCRYPTED 0x0020u
#define CSGDB_OPEN_PLAINTEXT 0x0040u
#define CSGDB_OPEN_FULLMUTEX 0x0080u
#define CSGDB_OPEN_NOMUTEX   0x0100u
#define CSGDB_OPEN_NOFOLLOW  0x0200u

#define CSGDB_KEY_AUTO       0u
#define CSGDB_KEY_RAW        1u
#define CSGDB_KEY_PASSPHRASE 2u
#define CSGDB_KEY_PROVIDER   3u

typedef struct csgdb csgdb;
typedef struct csgdb_stmt csgdb_stmt;
typedef struct csgdb_value csgdb_value;
typedef struct csgdb_backup csgdb_backup;

typedef struct csgdb_key_source {
    uint32_t struct_size;
    uint32_t kind;
    const void *data;
    size_t data_len;
    const char *provider_id;
} csgdb_key_source;

typedef struct csgdb_open_options {
    uint32_t struct_size;
    uint32_t abi_version;
    uint32_t flags;
    uint32_t busy_timeout_ms;
    uint64_t cache_size_bytes;
    uint64_t memory_budget_bytes;
    /* NULL for the platform default, otherwise a registered UTF-8 VFS name. */
    const char *vfs;
    const char *device_profile;
    csgdb_key_source key;
} csgdb_open_options;

CSGDB_API const char *csgdb_libversion(void);
CSGDB_API uint32_t csgdb_libversion_number(void);
CSGDB_API uint32_t csgdb_abi_version(void);
CSGDB_API const char *csgdb_source_id(void);
CSGDB_API uint32_t csgdb_default_open_flags(void);
CSGDB_API int32_t csgdb_open_options_init(csgdb_open_options *out_options);
CSGDB_API int32_t csgdb_open(const char *path, csgdb **out_db);
CSGDB_API int32_t csgdb_open_v2(
    const char *path,
    csgdb **out_db,
    uint32_t flags,
    /* NULL for the platform default, otherwise a registered UTF-8 VFS name. */
    const char *vfs
);
CSGDB_API int32_t csgdb_open_with_key(
    const char *path,
    const void *key,
    size_t key_len,
    csgdb **out_db
);
CSGDB_API int32_t csgdb_open_v3(
    const char *path,
    csgdb **out_db,
    const csgdb_open_options *options
);
CSGDB_API int32_t csgdb_exec(csgdb *db, const char *sql);
CSGDB_API int64_t csgdb_changes64(const csgdb *db);
CSGDB_API int64_t csgdb_total_changes64(const csgdb *db);
CSGDB_API int64_t csgdb_last_insert_rowid(const csgdb *db);
CSGDB_API int32_t csgdb_get_autocommit(const csgdb *db);
CSGDB_API int32_t csgdb_db_readonly(
    const csgdb *db,
    const char *database_name
);
CSGDB_API int32_t csgdb_txn_state(
    const csgdb *db,
    const char *database_name
);
CSGDB_API int32_t csgdb_busy_timeout(csgdb *db, int32_t timeout_ms);
CSGDB_API int32_t csgdb_wal_autocheckpoint(
    csgdb *db,
    int32_t frames
);
CSGDB_API int32_t csgdb_wal_checkpoint_v2(
    csgdb *db,
    const char *database_name,
    int32_t mode,
    int32_t *out_wal_frames,
    int32_t *out_checkpointed_frames
);
CSGDB_API void csgdb_interrupt(csgdb *db);
CSGDB_API int32_t csgdb_release_memory(csgdb *db);
CSGDB_API int32_t csgdb_prepare_v2(
    csgdb *db,
    const char *sql,
    int32_t sql_len,
    csgdb_stmt **out_statement,
    const char **out_tail
);
CSGDB_API int32_t csgdb_prepare_v3(
    csgdb *db,
    const char *sql,
    int32_t sql_len,
    uint32_t flags,
    csgdb_stmt **out_statement,
    const char **out_tail
);
CSGDB_API int32_t csgdb_bind_parameter_count(const csgdb_stmt *statement);
CSGDB_API int32_t csgdb_bind_parameter_index(
    const csgdb_stmt *statement,
    const char *name
);
CSGDB_API const char *csgdb_bind_parameter_name(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API int32_t csgdb_bind_null(csgdb_stmt *statement, int32_t index);
CSGDB_API int32_t csgdb_bind_int(
    csgdb_stmt *statement,
    int32_t index,
    int32_t value
);
CSGDB_API int32_t csgdb_bind_int64(
    csgdb_stmt *statement,
    int32_t index,
    int64_t value
);
CSGDB_API int32_t csgdb_bind_double(
    csgdb_stmt *statement,
    int32_t index,
    double value
);
CSGDB_API int32_t csgdb_bind_text(
    csgdb_stmt *statement,
    int32_t index,
    const char *value,
    int64_t value_len
);
CSGDB_API int32_t csgdb_bind_blob(
    csgdb_stmt *statement,
    int32_t index,
    const void *value,
    size_t value_len
);
CSGDB_API int32_t csgdb_step(csgdb_stmt *statement);
CSGDB_API int32_t csgdb_reset(csgdb_stmt *statement);
CSGDB_API int32_t csgdb_clear_bindings(csgdb_stmt *statement);
CSGDB_API int32_t csgdb_column_count(const csgdb_stmt *statement);
CSGDB_API const char *csgdb_column_name(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API int32_t csgdb_column_type(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API int32_t csgdb_column_int(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API int64_t csgdb_column_int64(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API double csgdb_column_double(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API const unsigned char *csgdb_column_text(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API const void *csgdb_column_blob(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API int32_t csgdb_column_bytes(
    const csgdb_stmt *statement,
    int32_t index
);
CSGDB_API int32_t csgdb_finalize(csgdb_stmt *statement);
CSGDB_API int32_t csgdb_close(csgdb *db);
CSGDB_API int32_t csgdb_errcode(const csgdb *db);
CSGDB_API const char *csgdb_errmsg(const csgdb *db);
CSGDB_API const char *csgdb_errstr(int32_t code);

#ifdef __cplusplus
}
#endif

#endif
