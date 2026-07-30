# CSGDB

> A Rust-native embedded database for terminal AI agents.

CSGDB（Context-State-Graph Database）是一套面向终端智能体的本地嵌入式数据库。它以可靠事务存储为基础，将结构化状态、不可变事件、语义记忆、关系图、全文检索和向量召回统一在同一套数据模型与查询系统中。

项目同时面向桌面、移动端和具备中端手机级性能的嵌入式设备，重点关注：

- 数据可靠性与断电恢复；
- Rust 类型安全和低运行时开销；
- Agent 原生的记忆、事实、来源与状态模型；
- 关系、全文、向量、图和时间条件的混合检索；
- 数据库、索引、Blob 与备份的全链路加密；
- 可预测的 CPU、内存、I/O 和结果规模；
- 相同文件格式在不同设备档位之间迁移。

## 当前状态

CSGDB 处于架构设计和基础实现准备阶段。当前仓库首先固定边界、数据模型、查询 IR、安全基线和测试门槛，避免在存储格式稳定前过早承诺兼容性。

尚未发布可用于生产环境的版本，也不应将当前设计文档视为已经实现的安全保证。

## 核心能力

| 能力 | 设计目标 |
| --- | --- |
| 事务存储 | ACID、WAL、快照读取、批量提交、可控 Checkpoint |
| 类型化 Schema | Rust derive、字段约束、索引声明、版本化迁移 |
| CSG-Q | 编译期 Rust DSL 与运行时 AgentPlan 共用同一查询 IR |
| Agent 数据模型 | Event、State、Memory、Fact、Edge、Artifact、Embedding、Provenance |
| 混合检索 | 结构过滤、全文、向量、图遍历、时间衰减与可解释融合 |
| 数据安全 | 数据库级加密、字段级加密、密钥分层、备份加密 |
| 数据治理 | 来源追踪、可信度、有效时间、过期、遗忘和压缩策略 |
| 恢复能力 | 完整性检查、派生索引重建、增量备份、数据抢救 |
| 嵌入式适配 | 有界缓存、量化向量、设备校准、可裁剪功能 |
| 可观测性 | 查询计划、得分解释、慢查询、锁等待、WAL 和内存统计 |

## 架构概览

```text
Rust API / CSG-Q / AgentPlan
              │
              ▼
   Typed Query IR + Policy
              │
              ▼
 Planner + Budget + Executor
       │       │       │
       ▼       ▼       ▼
 Relational  Search   Graph/Vector
              │
              ▼
 Transaction + Connection Manager
              │
              ▼
 Storage Kernel + WAL + Encrypted I/O
```

第一阶段使用成熟的嵌入式事务内核承担页存储、B-Tree、WAL 和崩溃恢复。Rust 层负责公共 API、Schema、查询 IR、执行计划、Agent 模型、加密密钥管理、向量索引、迁移、压缩和工具链。只有性能数据明确证明事务内核是主要瓶颈时，才评估替换底层 Pager。

## CSG-Q 示例

以下语法用于表达设计方向，尚未构成稳定 API：

```rust
let hits = csg_query! {
    from Memory as m

    filter m.agent == $agent_id
       && m.occurred_at >= now() - 30.days()

    recall $question in (m.text, m.embedding)
    fuse rrf(semantic, lexical, recency, m.importance)

    follow m -[Supports | Contradicts]-> Fact depth 0..2

    budget {
        cpu: 12.ms(),
        heap: 8.mib(),
        visit: 20_000
    }

    select {
        m.id,
        m.text,
        score,
        reason: explain(score)
    }

    take 24
};
```

CSG-Q 不以复刻 SQL 语法为目标。它将结构化查询、混合召回、图遍历、资源预算、权限和解释信息编译到统一的类型化查询计划。

## 目标平台

- Linux、Windows、macOS；
- Android、iOS；
- ARM64、x86_64、RISC-V 64；
- 具备文件系统、线程和动态内存的嵌入式 Linux/类 POSIX 环境。

完整数据库后端依赖操作系统能力，不以裸机 MCU 为首期目标。部分查询 IR、Schema 和数据类型组件可以保持 `no_std + alloc` 兼容。

## 文档

- [文档索引](docs/README.md)
- [系统架构](docs/architecture.md)
- [Agent 数据模型](docs/data-model.md)
- [CSG-Q 查询语言](docs/query-language.md)
- [存储、安全与恢复](docs/storage-security.md)
- [嵌入式与性能设计](docs/embedded-performance.md)
- [测试与发布门槛](docs/testing.md)
- [开发路线](docs/development-plan.md)
- [贡献指南](CONTRIBUTING.md)
- [安全策略](SECURITY.md)

## 设计原则

1. 正确性优先于跑分，数据格式优先于 API 便利性。
2. 原始数据是事实来源，全文和向量索引必须可重建。
3. 所有 Agent 动态查询都必须具有作用域、权限、预算和结果上限。
4. 加密覆盖主库、WAL、临时文件、派生索引、Blob 和备份。
5. 排名结果应包含可检查的得分构成与来源。
6. 不在核心数据库中耦合具体模型服务或网络协议。
7. 通过可重复基准测试决定优化，而不是凭经验固化参数。

## License

CSGDB 使用 [MIT License](LICENSE)。
