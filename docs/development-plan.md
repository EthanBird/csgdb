# 开发路线

## 总体策略

先建立正确性与测试基础，再叠加 Agent 数据模型和混合检索。每个里程碑都必须产生可运行代码、测试、基准和更新后的文档。

## 当前进度

当前处于 M1。已经完成：

- Rust Workspace 和 `csgdb`、`csgdb-core`、`csgdb-ffi`、`csgdb-cli` crate；
- 默认加密、显式明文、冲突标志拒绝和失败关闭策略；
- 原始密钥、口令和平台密钥提供器的公共抽象；
- SQLCipher 事务内核适配和真实 `.db` 文件读写；
- Rust `open/open_with_key/open_with_passphrase/open_plaintext/close`；
- SQL 批量执行、单整数查询以及显式提交和回滚；
- C `open/open_v2/open_with_key/open_v3/exec/close/error` 函数族；
- Rust `Value/ValueRef/Statement/Rows/Row`、严格类型读取和精确参数数量校验；
- Rust 连接本地有界 LRU Prepared Statement Cache；
- C `prepare/bind/step/column/reset/clear_bindings/finalize` 函数族；
- C Statement 状态机、越界检查和延迟连接关闭；
- Rust/C 连接变更计数、最后插入 Rowid、自动提交、只读和事务状态；
- Rust/C 忙等待配置、连接缓存回收和跨线程查询取消；
- 查询取消后的稳定错误分类和连接恢复验证；
- 加密文件头、正确密钥重开、错误密钥拒绝和明文兼容测试；
- 经实际构建验证的 Rust 1.82 最低工具链；
- 格式、Clippy、测试、文档和 C 头文件检查的 CI。

当前仍是开发版本。尚未完成读连接池、写入调度器、平台密钥库适配、Checkpoint 管理和断电故障注入。下一切片优先完成有界读连接池与单写调度，再基于这些原语加入并发快照和背压测试。

## M0：规格与工程骨架

交付：

- Cargo Workspace；
- crate 边界；
- 错误类型和日志规范；
- CI：格式、Lint、测试、文档；
- ADR 模板；
- 基准结果格式；
- 支持平台矩阵；
- 文件格式版本策略。

完成定义：

- 空数据库能在参考桌面和 ARM64 设备构建、打开和关闭；
- CI 不依赖未固定的系统状态；
- 不安全代码集中并具有审计说明。

## M1：事务与类型化 Schema

交付：

- 存储内核封装；
- 数据库与事务 API；
- 单写入调度器和读取连接池；
- `derive(Collection)`；
- 基础 CRUD；
- Prepared Statement Cache；
- WAL 与 Checkpoint 管理；
- 基础监控。

完成定义：

- 支持批量写入、快照读取和事务回滚；
- 断电注入不破坏已提交数据；
- Schema 标识不依赖 Rust 符号重命名。

## M2：查询 IR 与 CSG-Q

交付：

- Rust 查询 DSL；
- AgentPlan；
- 逻辑与物理 IR；
- 结构化索引选择；
- Budget、Capability、取消和流式结果；
- `EXPLAIN` 输出。

完成定义：

- AgentPlan 无法执行无作用域、无预算或无上限查询；
- 查询改写具备属性测试；
- 计划可序列化、版本化和缓存。

## M3：Agent 核心模型

交付：

- Event、State、Memory、Fact、Edge、Artifact、Provenance；
- Namespace 与权限；
- 乐观 State 更新；
- 事实版本与有效时间；
- Artifact Pack；
- 过期和遗忘策略框架。

完成定义：

- Agent 会话可以重放；
- 并发状态覆盖能够检测；
- 所有知识型对象均可追踪来源；
- Namespace 隔离测试通过。

## M4：全文、向量与图

交付：

- 全文索引与分词接口；
- Flat SIMD 向量检索；
- 量化向量；
- HNSW；
- 有界图遍历；
- RRF 和校准融合；
- Score Explanation；
- 派生索引代数与重建。

完成定义：

- 索引删除后可以从权威数据完整重建；
- 模型和维度不匹配会被拒绝；
- ARM64 设备达到已公布性能基线；
- 混合查询结果能够解释。

## M5：加密、备份与恢复

交付：

- 数据库级加密后端；
- 操作系统密钥库适配；
- 字段级加密；
- Blob 和备份加密；
- 完整快照与增量备份；
- 完整性检查；
- 索引恢复和记录抢救工具。

完成定义：

- 明文扫描覆盖所有落盘文件；
- 错误密钥和损坏备份被可靠拒绝；
- 密钥轮换中断后可以继续或回滚；
- 安全审查完成。

## M6：在线迁移与压缩

交付：

- Schema Diff；
- 影子表迁移；
- 双写和原子切换；
- 字段 Codec；
- 后台重新压缩；
- 自动 Vacuum 与空间回收；
- 维护任务调度。

完成定义：

- 每个迁移状态可故障恢复；
- 新旧 Codec 数据可以同时读取；
- 后台任务不突破前台查询预算。

## M7：SDK、工具和嵌入式发布

交付：

- 稳定 Rust API；
- C ABI；
- Kotlin/Swift 绑定；
- `csg` CLI；
- 数据库检查与性能分析工具；
- Full、Edge、Core 构建档；
- ARM64 与 RISC-V 参考构建；
- 使用手册和示例 Agent。

CLI 至少包含：

```text
csg inspect
csg query
csg explain
csg verify
csg backup
csg restore
csg repair
csg compact
csg migrate
csg benchmark
```

完成定义：

- 相同权威数据可在不同设备档位读取；
- 发布包具有可复现构建记录；
- 性能、安全和兼容性报告公开；
- 达到首个稳定文件格式门槛。

## 后续探索

以下项目不进入首个稳定版本承诺：

- 纯 Rust Pager 与 B-Tree；
- 多进程并发写入；
- 端到端增量同步协议；
- 磁盘向量索引；
- 基于设备统计的自动物化视图；
- 面向多 Agent 的本地共享服务模式。

所有探索功能必须以 feature flag 隔离，不能影响稳定文件格式和默认安全边界。
