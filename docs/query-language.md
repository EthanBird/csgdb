# CSG-Q 查询语言

## 1. 定位

CSG-Q 是类型化混合查询系统，不是 SQL 字符串生成器。它需要统一表达：

- 结构化过滤、Join 和聚合；
- 全文与向量召回；
- 有界图遍历；
- 时间点和有效区间；
- 多路排序融合；
- 权限、资源预算和取消；
- 结果解释与增量订阅。

## 2. 双前端

### 2.0 已实现的有界类型化查询

`#[derive(Collection)]` 已为每个字段生成类型化句柄：

```rust
let field = Memory::FIELD_NAMESPACE;

assert_eq!(field.id(), "agent.memory.namespace");
assert_eq!(field.column(), "namespace");
```

其具体类型是 `CollectionField<Memory, String>`。因此集合和字段值类型可以直接参与 Rust 泛型检查，而稳定字段 ID 可以进入可序列化查询计划；物理列名只由已注册 Schema 解析，不作为 Agent 输入。字段句柄保持为一个静态元数据指针的大小，不在普通记录访问路径执行哈希或注册表查询。

字段句柄已经可以直接构造谓词和排序：

```rust
let predicate = Memory::FIELD_NAMESPACE
    .eq("session")?
    .and(Memory::FIELD_SCORE.ge(Some(0.7))?)
    .and(Memory::FIELD_DELETED.eq(false)?);

let query = Memory::query()
    .filter(predicate)
    .order_by(Memory::FIELD_SCORE.desc())
    .take(32)?;

let rows = db.query_collection(&query)?;
# Ok::<(), csgdb::Error>(())
```

`QueryDraft` 没有执行接口，只有调用 `take(1..=10_000)` 后才得到 `CollectionQuery`。当前实现支持：

- `eq`、`ne`；
- 对有序字段使用 `lt`、`le`、`gt`、`ge`；
- 可空字段的 `is_null`、`is_not_null`；
- `and`、`or`、`not`；
- 多字段 `order_by`；
- 单连接、显式事务、连接池、只读连接和只读快照执行。

文本和 Blob 可以直接传入 `&str`、`&[u8]`，构建后的查询拥有绑定值。值不会拼入 SQL；调试输出也不会打印值。自定义排序会自动追加主键作为稳定的最终排序键，未指定排序时直接按主键升序。

当前安全上限为 10,000 行、256 个谓词节点、32 层谓词深度和 8 个调用方排序字段。伪造字段元数据、重复排序字段、非有限浮点比较值、对 `NULL` 使用大小比较或超过任一上限都会返回 `ErrorCode::InvalidQuery`。

显式投影、主键游标分页、流式结果和动态 AgentPlan 尚未进入稳定 API。

### 2.1 Rust 编译期 DSL

供应用开发者使用，通过函数式过程宏生成类型化 IR：

```rust
let query = csg_query! {
    from Memory as m
    filter m.namespace == $namespace
       && m.occurred_at >= $from

    recall $text in (m.text, m.embedding)
    fuse rrf(semantic, lexical, recency, m.importance)

    budget {
        cpu: 15.ms(),
        heap: 8.mib(),
        visit: 25_000,
        output: 256.kib()
    }

    select MemoryHit {
        id: m.id,
        text: m.text,
        score,
        why: explain(score)
    }

    take 32
};
```

宏展开后仍由 Rust 编译器检查字段、类型、返回结构和权限能力。

### 2.2 AgentPlan

供 Agent 在运行时构造。AgentPlan 是受限、可验证、可序列化的查询计划，不允许任意 SQL 和自定义代码。

```rust
let plan = AgentPlan::recall::<Memory>(question)
    .scope(namespace)
    .within(Last::days(30))
    .fusion(Fusion::reciprocal_rank())
    .budget(Budget::edge_default())
    .limit(24);
```

动态计划必须满足：

- Schema 中存在目标集合和字段；
- 调用方具有 Namespace 和字段权限；
- 向量维度与模型兼容；
- 图深度、访问量、CPU、内存和输出有界；
- 不包含未注册函数；
- 计划复杂度低于解析上限。

## 3. 类型状态

通过 Type-State 使危险查询无法执行：

```rust
let draft = Memory::scan();
// draft.execute() 不存在

let executable = draft
    .scope(namespace)
    .budget(Budget::edge_default())
    .limit(100);

let rows = executable.execute(&db)?;
```

只有完成作用域、预算和结果上限配置的动态查询才实现 `Executable`。

敏感字段采用能力类型：

```rust
db.with_capability(private_memory_cap)
    .query(...)
```

缺少能力时，敏感字段不出现在可投影字段集合中。

## 4. 查询 IR

逻辑 IR 建议包含：

```rust
enum LogicalOp {
    Scan,
    Filter,
    Join,
    Aggregate,
    FullTextRecall,
    VectorRecall,
    Traverse,
    TemporalSlice,
    Fusion,
    Sort,
    Project,
    Limit,
    Subscribe,
}
```

每个节点保存：

- 输入与输出类型；
- 估算行数；
- 估算 CPU、内存和 I/O；
- 所需权限；
- 确定性；
- 可中断点；
- 来源位置；
- 计划版本。

物理 IR 可以选择 B-Tree 查找、覆盖索引、FTS、Flat SIMD、HNSW、邻接表扫描等具体实现。

## 5. 查询优化

优化器重点解决混合检索中的顺序问题：

- 结构化选择性高时先过滤，再做向量检索；
- 向量候选集小且结构过滤宽泛时先召回，再过滤；
- 图遍历前尽量缩小起点；
- 可覆盖查询避免读取正文；
- 大字段延迟加载；
- 根据设备缓存和向量规模选择 Flat 或近似索引；
- 根据预算降低候选集和重排规模。

统计信息包括行数、基数、直方图、全文文档频率、向量索引规模、图节点度数和缓存热度。

## 6. 排名融合

不同召回信号不直接相加。默认支持：

- Reciprocal Rank Fusion；
- 经标定的加权融合；
- 两阶段召回与重排；
- 明确版本化的自定义 ScoreExpr。

查询结果可以附带：

```rust
struct ScoreExplanation {
    semantic_rank: Option<u32>,
    lexical_rank: Option<u32>,
    recency_score: f32,
    importance_score: f32,
    graph_boost: f32,
    final_score: f32,
    fusion_version: FusionVersion,
}
```

相同数据、相同模型、相同计划版本和相同统计快照应尽可能产生可重复结果。

## 7. 时间与图

示例：

```rust
csg_query! {
    from Fact as f
    as_of $timestamp
    follow f -[Supports | Contradicts]-> Fact depth 1..3
    filter edge.confidence >= 0.7
    budget { visit: 10_000 }
    take 100
}
```

禁止无限深度遍历。运行时还应有最大深度硬限制，即使调用方声明了更大的预算。

## 8. 可取消与流式结果

长查询必须定期检查：

- 调用方取消令牌；
- CPU 时间；
- 已访问记录；
- 已分配内存；
- 输出字节数；
- 数据库关闭信号。

结果默认可流式返回。需要稳定一致视图时，结果流持有只读快照，但执行器必须限制快照最长生命周期，避免长期阻止 Checkpoint。
