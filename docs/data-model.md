# Agent 数据模型

## 1. 设计目标

数据模型必须同时支持可重放历史、快速状态读取、可信事实、语义召回和多 Agent 隔离。数据库不应将所有内容压入一个通用 JSON 表，也不应要求所有扩展都修改核心 Schema。

建议采用“稳定热字段 + 可扩展属性”的混合模型：

- 高频过滤、排序和关联字段使用类型化列；
- 低频扩展属性使用版本化 Map；
- 大对象使用 Artifact Pack；
- 向量与生成模型信息分离保存。

### 1.1 当前类型化映射

当前 `derive(Collection)` 已经提供关系表的稳定映射边界。每个核心对象在落盘前都必须显式确定集合 ID、表名、版本、字段 ID、列名和主键；这些磁盘标识不从 Rust 类型名推导。字段声明顺序仅决定当前语句的绑定和解码顺序，不影响按稳定字段 ID 排序生成的 Schema 指纹。

首批字段类型覆盖整数、浮点、布尔、UTF-8 文本、Blob 和可空值。强类型 ID、枚举、时间戳、版本化 Map 和向量仍需要后续 Codec/newtype 支持；在这些 Codec 固定前，以下核心对象结构仍属于逻辑模型，不代表已经冻结的物理格式。

## 2. 核心对象

### 2.1 Event

不可变事件，是可审计历史和重放的基础。

```rust
struct Event {
    id: EventId,
    namespace: NamespaceId,
    session: Option<SessionId>,
    sequence: u64,
    kind: EventKind,
    occurred_at: Timestamp,
    recorded_at: Timestamp,
    payload: Value,
    provenance: ProvenanceId,
}
```

典型类型包括用户输入、模型响应、工具请求、工具结果、环境观察、状态变更和错误。

### 2.2 State

用于保存当前状态，而不是完整历史：

```rust
struct State {
    namespace: NamespaceId,
    key: StateKey,
    revision: u64,
    value: Value,
    expires_at: Option<Timestamp>,
    updated_by: EventId,
}
```

State 更新使用乐观版本检查，防止多个 Agent 工作流覆盖彼此结果。

### 2.3 Memory

```rust
struct Memory {
    id: MemoryId,
    namespace: NamespaceId,
    kind: MemoryKind,
    text: Text,
    confidence: f32,
    importance: f32,
    occurred_at: Timestamp,
    last_accessed_at: Option<Timestamp>,
    expires_at: Option<Timestamp>,
    provenance: ProvenanceId,
    supersedes: Option<MemoryId>,
}
```

`MemoryKind` 至少区分：

- Episodic：发生过的具体经历；
- Semantic：经过整理的事实或知识；
- Procedural：技能、流程和使用习惯；
- Working：短期工作记忆；
- Preference：用户或 Agent 偏好。

### 2.4 Fact

Fact 表达可被支持、反驳、失效和替代的结构化命题：

```rust
struct Fact {
    id: FactId,
    namespace: NamespaceId,
    subject: EntityId,
    predicate: PredicateId,
    object: FactValue,
    confidence: f32,
    valid_from: Option<Timestamp>,
    valid_to: Option<Timestamp>,
    observed_at: Timestamp,
    provenance: ProvenanceId,
}
```

同一事实可以存在多个来源。数据库不自动决定哪个来源绝对正确，而是保存证据和冲突关系供查询层处理。

### 2.5 Edge

```rust
struct Edge {
    id: EdgeId,
    namespace: NamespaceId,
    source: NodeId,
    kind: EdgeKind,
    target: NodeId,
    weight: f32,
    valid_from: Option<Timestamp>,
    valid_to: Option<Timestamp>,
    provenance: ProvenanceId,
}
```

首批关系建议包含：

- `Supports`
- `Contradicts`
- `DerivedFrom`
- `CausedBy`
- `RelatedTo`
- `PartOf`
- `Supersedes`
- `CreatedBy`

图遍历默认要求深度和访问节点上限。

### 2.6 Artifact

Artifact 保存文件、图像、音频、代码、网页快照等大型内容：

```rust
struct Artifact {
    id: ArtifactId,
    namespace: NamespaceId,
    media_type: MediaType,
    content_hash: ContentHash,
    logical_size: u64,
    stored_size: u64,
    codec: CodecId,
    encryption_scope: EncryptionScope,
    created_at: Timestamp,
    provenance: ProvenanceId,
}
```

正文按内容哈希存入加密 Pack。去重仅在相同加密作用域内进行，避免跨租户内容存在性泄露。

### 2.7 Embedding

```rust
struct Embedding<const D: usize, M: EmbeddingModel> {
    owner: NodeId,
    model_revision: ModelRevision,
    value: QuantizedVector<D>,
    source_hash: ContentHash,
    created_at: Timestamp,
}
```

必须保存模型标识、模型版本、维度、距离度量、量化方式和源内容哈希。内容改变或模型升级后，旧向量可以继续读取，但不能与不兼容的向量索引混用。

### 2.8 Provenance

来源是所有知识型对象的必要组成：

```rust
struct Provenance {
    id: ProvenanceId,
    source_kind: SourceKind,
    source_uri: Option<Uri>,
    source_hash: Option<ContentHash>,
    producer: ProducerId,
    tool: Option<ToolIdentity>,
    recorded_at: Timestamp,
    signature: Option<Signature>,
}
```

## 3. 命名空间与权限

每条权威记录必须属于一个 Namespace。Namespace 可以表示用户、Agent、工作区或任务。

访问策略至少支持：

- 读取私有数据；
- 写入状态；
- 追加事件；
- 读取敏感字段；
- 导出 Artifact；
- 执行维护操作；
- 跨 Namespace 聚合。

策略在查询进入优化器前注入逻辑 IR，不能只依赖应用层过滤。

## 4. 时间语义

区分：

- `occurred_at`：现实事件发生时间；
- `recorded_at`：进入数据库时间；
- `valid_from/valid_to`：事实有效区间；
- `transaction_time`：数据库版本时间。

查询系统应支持当前状态和 `as_of` 历史状态。所有时间以 UTC 存储，同时允许保存原始时区元数据。

## 5. 遗忘与压缩

删除不是唯一的遗忘方式。可配置策略包括：

- TTL 过期；
- 低重要度事件聚合为摘要；
- 原文迁入冷存储；
- 向量降精度；
- 删除派生索引但保留权威记录；
- 按 Namespace 进行密码学删除；
- 保留 Provenance 但移除敏感正文。

任何自动整理操作都应产生维护事件，并记录被替代对象和所用策略版本。
