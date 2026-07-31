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

当前已经实现 `csgdb_exec`、Statement、Bind、Step、Column、Reset 和 Finalize。接口保持逐行读取，不会把完整结果集无界物化到内存。

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

csgdb_bind_text(stmt, 1, agent_id, -1);

while (csgdb_step(stmt) == CSGDB_ROW) {
    int64_t id = csgdb_column_int64(stmt, 0);
    const unsigned char *text = csgdb_column_text(stmt, 1);
}

csgdb_finalize(stmt);
```

当前已实现：

```text
csgdb_exec
csgdb_prepare_v2 / csgdb_prepare_v3

csgdb_bind_parameter_count / index / name
csgdb_bind_null / int / int64 / double / text / blob

csgdb_step / reset / clear_bindings / finalize

csgdb_column_count / name / type
csgdb_column_int / int64 / double / text / blob / bytes

csgdb_changes64 / total_changes64 / last_insert_rowid
csgdb_get_autocommit / db_readonly / txn_state
csgdb_busy_timeout / interrupt / release_memory
csgdb_wal_autocheckpoint / wal_checkpoint_v2

csgdb_errcode / errmsg / errstr
csgdb_close
```

后续兼容切片：

```text
csgdb_extended_errcode
csgdb_close_v2
```

参数从 1 开始编号，结果列从 0 开始编号。UTF-8 是主文本接口；需要 UTF-16 时通过独立兼容层提供。

`csgdb_bind_text` 和 `csgdb_bind_blob` 在返回前复制调用方数据。Column 返回的文本和 Blob 指针仅在当前 Row 有效；下一次 `step`、`reset` 或 `finalize` 会使其失效。Statement 到达 `DONE` 或错误状态后必须先 `reset` 才能再次执行。

关闭数据库句柄时，如果还有 Statement 存活，连接关闭会延后到最后一个 Statement 完成 `finalize`。调用方仍必须对同一 Statement 的访问进行串行化。

连接状态接口遵循以下约定：

- `csgdb_changes64` 返回最近一次完成的写操作所影响的行数；
- `csgdb_total_changes64` 返回当前连接的累计变更行数；
- `csgdb_txn_state(db, NULL)` 返回所有已附加数据库中的最高事务活动，状态为 `CSGDB_TXN_NONE`、`CSGDB_TXN_READ` 或 `CSGDB_TXN_WRITE`；
- `csgdb_db_readonly` 返回 `1` 或 `0`，无效数据库名和检查失败返回 `-1`；
- `csgdb_busy_timeout` 接受非负毫秒值，后一次调用替换前一次配置；
- `csgdb_wal_autocheckpoint` 设置连接本地的 PASSIVE 自动 Checkpoint 帧阈值，零表示禁用；
- `csgdb_wal_checkpoint_v2` 接受 PASSIVE、FULL、RESTART 或 TRUNCATE 模式，并分别输出 WAL 总帧数和已经复制的帧数；
- `csgdb_interrupt` 可以从另一个线程调用，不等待正在执行语句持有的连接锁；
- `csgdb_release_memory` 主动释放连接本地的可回收缓存。

`csgdb_interrupt` 与其他连接操作并发调用是安全的，但调用方必须保证数据库句柄在中断调用返回前仍然存活；不得令 `close` 与 `interrupt` 竞争。被取消的 `step` 返回 `CSGDB_INTERRUPT`。连接本身仍可继续使用，Statement 应先 `finalize`，或按其状态执行 `reset` 后再复用。

Checkpoint 的 C 用法：

```c
int32_t wal_frames = -1;
int32_t checkpointed_frames = -1;

csgdb_wal_autocheckpoint(db, 0);

int rc = csgdb_wal_checkpoint_v2(
    db,
    "main",
    CSGDB_CHECKPOINT_PASSIVE,
    &wal_frames,
    &checkpointed_frames
);
```

数据库名为 `NULL` 时检查所有附加库，输出指针可以为 `NULL`。帧数不可用时写入 `-1`。PASSIVE 模式不等待读写锁；它可能返回 `CSGDB_OK`，同时 `checkpointed_frames < wal_frames`，这表示长快照等因素仍阻碍部分帧回收。FULL、RESTART 或 TRUNCATE 无法取得所需锁时返回 `CSGDB_BUSY`，但仍保留引擎给出的帧数。

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
tx.execute(
    "INSERT INTO event(kind, payload) VALUES (?, ?)",
    &[
        ValueRef::Text("tool"),
        ValueRef::Blob(&[1, 2]),
    ],
)?;
tx.commit()?;
```

参数绑定和流式行读取：

```rust
use csgdb::{Database, ValueRef, ValueType};

let db = Database::open_with_passphrase("agent.db", secret)?;

db.execute(
    "INSERT INTO memory(agent_id, text) VALUES (?, ?)",
    &[
        ValueRef::Text(agent_id),
        ValueRef::Text("first memory"),
    ],
)?;

let mut statement = db.prepare_cached(
    "SELECT id, text FROM memory WHERE agent_id = ? ORDER BY id",
)?;
let mut rows = statement.query(&[ValueRef::Text(agent_id)])?;

while let Some(row) = rows.next_row()? {
    assert_eq!(row.value_type(0)?, ValueType::Integer);
    let id = row.get_i64(0)?;
    let text = row.get_text(1)?;
    consume(id, text);
}
```

`Value` 是拥有所有权的动态值，`ValueRef` 用于低开销绑定或借用当前行。严格 getter 不做跨类型隐式转换；例如对 TEXT 调用 `get_i64` 会返回 `InvalidColumnType`。

`prepare_cached` 使用连接本地、有容量上限的 LRU Cache，默认最多保留 16 条闲置语句；`set_prepared_statement_cache_capacity` 可调整上限，`flush_prepared_statement_cache` 可立即清空。`prepare` 则总是直接编译。线程和生命周期约束由 Rust 类型系统表达：

```text
Statement<'db>: 不能比 Database 存活更久
Row<'stmt>: borrowed
Rows::next_row: 推进后使上一个 Row 借用失效
```

连接观测与控制：

```rust
use std::time::Duration;
use csgdb::TransactionState;

db.set_busy_timeout(Duration::from_millis(250))?;

assert!(db.is_autocommit());
assert_eq!(db.transaction_state(None)?, TransactionState::None);

let changed = db.changes();
let changed_since_open = db.total_changes();
let last_rowid = db.last_insert_rowid();

db.release_memory()?;
```

`is_readonly("main")` 检查指定数据库的打开模式，`is_busy()` 表示当前连接是否存在尚未完成的 Statement，`is_interrupted()` 表示连接中断当前是否仍在生效。`transaction_state(None)` 汇总所有已附加数据库，传入 `Some("main")` 则只检查主库。

手动 WAL 维护：

```rust
use csgdb::CheckpointMode;

db.set_wal_autocheckpoint(0)?;

let progress = db.checkpoint(CheckpointMode::Passive)?;
if !progress.is_complete() {
    eprintln!(
        "{} WAL frames remain",
        progress.remaining_frames().unwrap_or_default()
    );
}
```

`checkpoint` 只处理主库，`checkpoint_database(Some(name), mode)` 可以指定附加库，传入 `None` 时处理所有附加库。`CheckpointResult` 区分锁竞争与未完整复制，并保留 WAL 总帧数、已复制帧数和剩余帧数。RESTART 和 TRUNCATE 可能等待读连接，通常应放在维护窗口；前台观测优先使用 PASSIVE。

长查询取消通过与数据库借用分离的 `InterruptHandle` 完成：

```rust
let interrupt = db.interrupt_handle();

// 将 db 移交给数据库工作线程执行查询。
let worker = std::thread::spawn(move || {
    db.query_i64(long_running_sql)
});

// 可从控制线程或请求超时处理器调用。
interrupt.interrupt();
let result = worker.join().expect("database worker");
```

中断句柄可以跨线程移动；数据库关闭后再调用会安全地成为空操作。取消只终止当前运行，连接在清理当前 Statement 后仍可继续使用。

### 8.1 有界连接管理

需要并发读取和集中写入调度时，可以选择 `DatabasePool`；普通 `Database` API 不受影响：

```rust
use csgdb::{DatabasePool, KeySource, PoolOptions, Value};

let pool = DatabasePool::builder("agent.db")
    .key(KeySource::Raw(database_key))
    .read_connections(2)
    .write_queue_capacity(64)
    .open()?;

pool.execute_batch(
    "CREATE TABLE IF NOT EXISTS event(
        id INTEGER PRIMARY KEY,
        body TEXT NOT NULL
    );",
)?;

pool.execute(
    "INSERT INTO event(body) VALUES (?)",
    vec![Value::from("tool completed")],
)?;

let count = pool.query_i64("SELECT count(*) FROM event")?;
```

打开时先创建一个可写连接并启用 WAL，再使用同一个已解析密钥打开固定数量的文件级只读连接。默认值为 2 个读连接和 64 个等待写任务。默认 Group Commit 最多合并 8 个相邻逻辑任务，收集窗口为 250 微秒；默认每 32 次写线程提交运行一次 PASSIVE Checkpoint，未回收 WAL 软上限为 4096 帧。内存数据库不支持多连接池，应继续使用单连接 `Database`。

多个参数化写入可以作为单个队列任务和单个事务提交：

```rust
use csgdb::{BatchStatement, Value};

let changes = pool.execute_transaction([
    BatchStatement::new(
        "INSERT INTO event(body) VALUES (?)",
        vec![Value::from("observed")],
    ),
    BatchStatement::new(
        "UPDATE agent_state SET revision = revision + 1 WHERE id = ?",
        vec![Value::Integer(7)],
    ),
])?;
```

返回值按输入顺序给出每条语句的变更行数。任一语句的准备、绑定或执行失败都会回滚整个逻辑任务。相邻的 `execute` 与 `execute_transaction` 调用可以共享一个外层物理事务，但每个调用拥有独立 Savepoint 和响应：

- 单个逻辑任务失败时只回滚自己的 Savepoint，同组其他任务仍可提交；
- 外层提交失败时，所有原本成功的逻辑任务都收到提交错误；
- 任务数和等待时间同时受 `GroupCommitOptions` 约束；
- `write`、`write_with_policy`、`execute_batch`、Checkpoint 和配置变更是严格屏障；
- 队列背压和逻辑任务的提交、完成、拒绝统计不因物理合并而改变。

显式批量事务仍然是调用方定义的原子边界；Group Commit 只减少相邻边界的持久化提交次数，不把两个调用方的失败域合并。

资源与维护参数可以在打开前配置：

```rust
use csgdb::{GroupCommitOptions, WalMaintenanceOptions};
use std::time::Duration;

let pool = DatabasePool::builder("agent.db")
    .key(KeySource::Raw(database_key))
    .group_commit(GroupCommitOptions::new(
        16,
        Duration::from_micros(500),
    ))
    .wal_maintenance(WalMaintenanceOptions::new(
        16,
        2_048,
    ))
    .open()?;
```

Group Commit 硬上限为 256 个逻辑任务和 20 毫秒等待时间。自动维护只运行 PASSIVE 模式，不等待读者。一次维护发现未回收帧超过软上限后，写线程会在每次后续写入后重试 PASSIVE，直到长快照释放或压力下降。

需要流式行或多语句快照时使用读回调：

```rust
let result = pool.read(|connection| {
    let transaction = connection.transaction()?;
    let revision = transaction.query_i64(
        "SELECT revision FROM agent_state WHERE id = 1",
    )?;
    let event_count = transaction.query_i64(
        "SELECT count(*) FROM event",
    )?;
    transaction.commit()?;
    Ok((revision, event_count))
})?;
```

读回调结束时连接自动归还。`try_read` 不等待，池耗尽时返回 `ReadPoolExhausted`。回调不能返回借用连接或 Row 的值。

所有写任务由单个 `csgdb-writer` 线程串行执行。`write` 等待队列空间，`try_write` 立即拒绝，`write_with_policy` 可以指定最长等待时间：

```rust
use std::time::Duration;
use csgdb::WriteBackpressure;

let result = pool.write_with_policy(
    WriteBackpressure::Timeout(Duration::from_millis(50)),
    move |database| {
        let transaction = database.transaction()?;
        transaction.execute_batch(write_batch)?;
        transaction.commit()
    },
);
```

写回调及返回值必须满足 `Send + 'static`，因此排队参数使用拥有所有权的 `Value`，而不是借用型 `ValueRef`。同一池的递归读返回 `ReentrantRead`，读回调或写回调中的同步写提交返回 `ReentrantWrite`，从结构上阻止自等待死锁。回调 panic 返回 `WriteTaskPanicked`，不会停止写线程。

`checkpoint`、`checkpoint_database`、`set_wal_autocheckpoint` 和 `set_automatic_wal_maintenance` 也通过单写线程串行执行，避免与普通写任务同时操作维护状态。调用 `set_wal_autocheckpoint` 会切换到引擎本地策略并关闭管理器自动维护；`set_automatic_wal_maintenance` 会关闭引擎本地阈值并重新启用或替换管理器策略。

`stats()` 返回读连接使用量、当前排队任务、写线程活动状态、提交/完成/拒绝计数、物理 Group Commit 次数、实际合并任务数、最大批次、自动 Checkpoint 次数、维护失败、最近 WAL 帧数、未回收帧数和压力状态。`interrupt_writer()` 可从控制线程取消当前写任务。`close()` 只有在其他池克隆全部释放后才成功，否则返回 `ConnectionManagerInUse`。

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

当前 C ABI：

```text
OK                         0
INVALID_ARGUMENT           1
INVALID_OPEN_FLAGS         2
INVALID_KEY                3
KEY_REQUIRED               4
KEYSTORE_UNAVAILABLE       5
BUSY                       6
READONLY                   7
CONSTRAINT                 8
CORRUPT                    9
STORAGE                   10
MISUSE                    11
RANGE                     12
INTERRUPT                 13
ROW                      100
DONE                     101
```

后续扩展错误候选：

```text
PLAINTEXT_REQUIRES_OPT_IN
AUTH_DENIED
BUDGET_EXCEEDED
VECTOR_MODEL_MISMATCH
INDEX_REBUILD_REQUIRED
MIGRATION_INCOMPLETE
```

`MISUSE` 表示语句状态不允许当前操作，`RANGE` 表示参数或结果列索引越界，`INTERRUPT` 表示运行中的数据库操作被显式取消。C ABI 的错误数字一经稳定发布，不再重排。

Rust 连接管理器另外使用以下 `ErrorCode`：

```text
InvalidPoolConfiguration
ReadPoolExhausted
WriteQueueFull
WriteQueueTimeout
ConnectionManagerClosed
ConnectionManagerInUse
WriteTaskPanicked
ReentrantRead
ReentrantWrite
```

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
