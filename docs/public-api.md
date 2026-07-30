# 公共接口与兼容性

## 1. 目标

CSGDB 的普通使用体验应接近成熟的嵌入式 SQL 数据库：

```text
open → prepare → bind → step → column → finalize → close
```

结构化 SQL 是基础接口。AgentPlan、CSG-Q、全文、向量和图能力作为增量扩展提供，普通应用不需要理解这些扩展也能使用事务、表、索引和查询。

兼容性分为四个层面：

| 层面 | 目标 |
| --- | --- |
| SQL | 与内置事务内核所支持的 SQL 方言保持一致 |
| 使用模型 | 保留连接、预编译语句、参数绑定和逐行读取 |
| C 源码 | 提供稳定 `csgdb_*` C ABI 和可选兼容头 |
| 文件 | 明文模式兼容普通 `.db`；加密模式兼容所选加密格式 |

普通数据库文件格式无法在保持完全明文兼容的同时提供全库加密。因此，默认加密文件仍命名为 `.db`，但普通未配置密钥的数据库工具不能读取其中内容。

## 2. 文件命名

所有主数据库统一使用 `.db`：

```text
agent.db
memory.db
workspace.db
```

加密与明文不通过扩展名区分。运行期间允许产生：

```text
agent.db-wal
agent.db-shm
agent.db-tmp
```

大型 Artifact 可以选择外部加密 Pack：

```text
agent.db.blobs/
```

扩展名不是密码学安全措施。真正的安全边界包括密钥来源、加密算法、认证标签、临时文件保护、错误密钥处理和明文降级策略。

## 3. 默认打开行为

当前开发版本已经实现默认加密策略，但尚未内置各平台的系统密钥库适配。因此：

- `Database::open` / `csgdb_open` 在已配置 KeyProvider 时可直接使用；
- 没有 KeyProvider 时返回 `KEYSTORE_UNAVAILABLE`，不会创建明文文件；
- 当前可立即使用 `open_with_key`、`open_with_passphrase` 或显式明文接口；
- 后续平台 SDK 将把默认系统密钥库适配注入 `KEY_AUTO`。

最简单的 C 接口：

```c
csgdb *db = NULL;

int rc = csgdb_open("agent.db", &db);
if (rc != CSGDB_OK) {
    fprintf(stderr, "%s\n", csgdb_errmsg(db));
    csgdb_close(db);
    return rc;
}

/* 数据库已经以加密模式打开。 */

csgdb_close(db);
```

最简单的 Rust 接口：

```rust
let db = Database::open("agent.db")?;
```

默认规则：

1. 新建数据库时生成随机数据库密钥。
2. 存在系统密钥库或 KeyProvider 时，安全保存或包装密钥。
3. 打开已有数据库时，根据数据库身份解析对应密钥。
4. 无法取得密钥时返回 `KEY_REQUIRED` 或 `KEYSTORE_UNAVAILABLE`。
5. 不允许写死的默认密钥。
6. 不允许因为密钥缺失而静默降级为明文。
7. 检测到明文数据库时，默认返回 `PLAINTEXT_REQUIRES_OPT_IN`。

## 4. C ABI

公共 C 头文件使用 opaque handle：

```c
typedef struct csgdb csgdb;
typedef struct csgdb_stmt csgdb_stmt;
typedef struct csgdb_value csgdb_value;
typedef struct csgdb_backup csgdb_backup;
```

主要打开接口：

```c
int csgdb_open(
    const char *path,
    csgdb **out_db
);

int csgdb_open_v2(
    const char *path,
    csgdb **out_db,
    uint32_t flags,
    const char *vfs
);

int csgdb_open_with_key(
    const char *path,
    const void *key,
    size_t key_len,
    csgdb **out_db
);

int csgdb_open_v3(
    const char *path,
    csgdb **out_db,
    const csgdb_open_options *options
);
```

公共结构体必须带有大小和 ABI 版本：

```c
typedef struct csgdb_open_options {
    uint32_t struct_size;
    uint32_t abi_version;

    uint32_t flags;
    uint32_t busy_timeout_ms;

    uint64_t cache_size_bytes;
    uint64_t memory_budget_bytes;

    const char *vfs;
    const char *device_profile;

    csgdb_key_source key;
} csgdb_open_options;
```

新增字段只能追加在尾部。实现根据 `struct_size` 判断调用方提供了哪些字段，未知尾部字段必须忽略。

## 5. 打开标志

```c
#define CSGDB_OPEN_READONLY        0x0001u
#define CSGDB_OPEN_READWRITE       0x0002u
#define CSGDB_OPEN_CREATE          0x0004u
#define CSGDB_OPEN_URI             0x0008u
#define CSGDB_OPEN_MEMORY          0x0010u
#define CSGDB_OPEN_ENCRYPTED       0x0020u
#define CSGDB_OPEN_PLAINTEXT       0x0040u
#define CSGDB_OPEN_FULLMUTEX       0x0080u
#define CSGDB_OPEN_NOMUTEX         0x0100u
#define CSGDB_OPEN_NOFOLLOW        0x0200u
```

如果调用方没有指定 `ENCRYPTED` 或 `PLAINTEXT`，实现自动补充 `ENCRYPTED`。二者同时出现时返回 `INVALID_OPEN_FLAGS`。

明文必须显式启用：

```c
csgdb_open_v2(
    "legacy.db",
    &db,
    CSGDB_OPEN_READWRITE |
    CSGDB_OPEN_CREATE |
    CSGDB_OPEN_PLAINTEXT,
    NULL
);
```

Rust 使用明确命名：

```rust
let db = Database::open_plaintext("legacy.db")?;
```

## 6. 密钥来源

```c
typedef enum csgdb_key_kind {
    CSGDB_KEY_AUTO = 0,
    CSGDB_KEY_RAW = 1,
    CSGDB_KEY_PASSPHRASE = 2,
    CSGDB_KEY_PROVIDER = 3
} csgdb_key_kind;

typedef struct csgdb_key_source {
    uint32_t struct_size;
    uint32_t kind;
    const void *data;
    size_t data_len;
    const char *provider_id;
} csgdb_key_source;
```

解析优先级：

```text
显式 Raw Key
  → 显式 Passphrase
  → 本连接 KeyProvider
  → 全局 KeyProvider
  → 系统安全密钥库
  → 安全失败
```

Rust KeyProvider：

```rust
pub trait KeyProvider: Send + Sync {
    fn load(
        &self,
        identity: &DatabaseIdentity,
    ) -> Result<Option<SecretKey>>;

    fn create(
        &self,
        identity: &DatabaseIdentity,
    ) -> Result<SecretKey>;

    fn remove(
        &self,
        identity: &DatabaseIdentity,
    ) -> Result<()>;
}
```

嵌入式设备可以将该接口适配到安全芯片、TEE、TPM 或宿主应用提供的密钥系统。

禁止在以下位置传递原始密钥：

- SQL 和 PRAGMA；
- URI 查询字符串；
- 日志和错误消息；
- 环境诊断输出；
- panic 文本；
- 数据库文件名。

URI 只能包含不敏感的 KeyProvider 标识：

```text
file:agent.db?mode=ro&csg_key_id=device
```

## 7. SQL 执行接口

当前已经实现 `csgdb_exec`。本节其余 Statement、Bind、Step 和 Column 函数是 M1 下一切片的稳定接口目标。

```c
csgdb_stmt *stmt = NULL;

csgdb_prepare_v3(
    db,
    "SELECT id, text FROM memory WHERE agent_id = ?",
    -1,
    CSGDB_PREPARE_PERSISTENT,
    &stmt,
    NULL
);

csgdb_bind_text(stmt, 1, agent_id, -1, CSGDB_TRANSIENT);

while (csgdb_step(stmt) == CSGDB_ROW) {
    int64_t id = csgdb_column_int64(stmt, 0);
    const char *text = csgdb_column_text(stmt, 1);
}

csgdb_finalize(stmt);
```

首批稳定函数族：

```text
csgdb_exec
csgdb_prepare_v2 / csgdb_prepare_v3

csgdb_bind_null / int / int64 / double / text / blob

csgdb_step / reset / clear_bindings / finalize

csgdb_column_count / name / type
csgdb_column_int64 / double / text / blob / bytes

csgdb_changes / total_changes / last_insert_rowid
csgdb_busy_timeout / interrupt

csgdb_errcode / extended_errcode / errmsg
csgdb_close / close_v2
```

参数从 1 开始编号，结果列从 0 开始编号。UTF-8 是主文本接口；需要 UTF-16 时通过独立兼容层提供。

## 8. Rust API

当前可运行接口：

```rust
let mut db = Database::open_with_passphrase(
    "agent.db",
    "replace-with-a-secret",
)?;

db.execute_batch(
    "CREATE TABLE IF NOT EXISTS memory (
        id INTEGER PRIMARY KEY,
        text TEXT NOT NULL
    );",
)?;

let tx = db.transaction()?;
tx.execute_batch("INSERT INTO memory(text) VALUES ('first memory');")?;
tx.commit()?;

assert_eq!(db.query_i64("SELECT count(*) FROM memory")?, 1);
```

显式密钥：

```rust
let db = Database::builder("portable.db")
    .key(KeySource::Raw(SecretKey::from_slice(&key)?))
    .open()?;
```

密码：

```rust
let db = Database::open_with_passphrase("portable.db", secret)?;
```

事务：

```rust
let tx = db.transaction()?;
tx.execute_batch(
    "INSERT INTO event(kind, payload) VALUES ('tool', X'0102');
     UPDATE state SET revision = revision + 1 WHERE id = 1;",
)?;
tx.commit()?;
```

参数绑定、通用行读取和 Statement Cache 正在实现。线程和生命周期目标：

```text
Database: Send + Sync
Statement<'db>: Send + !Sync
Transaction<'db>: !Send + !Sync
Row<'stmt>: borrowed
```

异步接口通过专用数据库工作线程实现，不要求底层文件 I/O 伪装成异步操作。

## 9. Agent 扩展

高级能力不修改基础 SQL Parser，通过表值函数、虚拟表或 CSG-Q 提供。

混合召回：

```sql
SELECT m.id, m.text, r.score
FROM csg_recall('memory', ?, 32) AS r
JOIN memory AS m ON m.id = r.rowid
WHERE m.agent_id = ?
ORDER BY r.score DESC;
```

图遍历：

```sql
SELECT *
FROM csg_traverse(
    'edge',
    ?,
    'Supports|Contradicts',
    2
);
```

向量检索：

```sql
SELECT rowid, distance
FROM csg_vector_search(
    'memory_embedding',
    ?,
    20
);
```

CSG-Q 与上述 SQL 扩展最终转换到同一内部查询 IR。

## 10. 可选兼容库

发布物可以包含：

```text
libcsgdb
libcsgdb_sqlite3_compat
```

主库只导出 `csgdb_*`。兼容库按明确矩阵导出部分 `sqlite3_*` 符号，避免主库与系统数据库库发生符号冲突。

兼容库不承诺所有私有结构、编译选项、VFS 或扩展 ABI。已有驱动若不能提供显式密钥，只能使用已注册的全局 KeyProvider 或系统密钥库。

## 11. 错误码

基础错误：

```text
OK
ERROR
BUSY
LOCKED
READONLY
INTERRUPT
IOERR
CORRUPT
CONSTRAINT
NOTADB
ROW
DONE
```

扩展错误：

```text
KEY_REQUIRED
BAD_KEY
KEYSTORE_UNAVAILABLE
PLAINTEXT_REQUIRES_OPT_IN
INVALID_OPEN_FLAGS
AUTH_DENIED
BUDGET_EXCEEDED
VECTOR_MODEL_MISMATCH
INDEX_REBUILD_REQUIRED
MIGRATION_INCOMPLETE
```

基础错误用于通用处理，扩展错误用于精确诊断。C ABI 的错误数字一经稳定发布，不再重排。

## 12. 版本与兼容承诺

提供：

```c
const char *csgdb_libversion(void);
uint32_t csgdb_libversion_number(void);
uint32_t csgdb_abi_version(void);
const char *csgdb_source_id(void);
```

稳定版本遵循：

- 补丁版本不破坏 ABI 和文件格式；
- 次版本可以增加函数、结构体尾部字段和可选能力；
- 主版本才允许移除接口；
- 文件格式升级必须提供显式迁移路径；
- 加密参数升级必须允许旧库读取和受控重加密；
- 每个发布版本保留 Golden Database 兼容测试。
