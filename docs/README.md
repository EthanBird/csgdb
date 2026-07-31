# CSGDB 开发文档

本文档集描述 CSGDB 的目标架构与实现边界。所有语法、文件格式和模块名称在第一个稳定版本发布前都可能调整。

## 阅读顺序

1. [系统架构](architecture.md)：分层、组件、事务与执行路径。
2. [Agent 数据模型](data-model.md)：事件、状态、记忆、事实和来源。
3. [CSG-Q 查询语言](query-language.md)：Rust DSL、AgentPlan、IR 和优化器。
4. [公共接口与兼容性](public-api.md)：Rust API、稳定 C ABI、打开流程和密钥规则。
5. [存储、安全与恢复](storage-security.md)：存储内核、加密、迁移、备份和修复。
6. [嵌入式与性能设计](embedded-performance.md)：设备档位、缓存、索引和基准。
7. [测试与发布门槛](testing.md)：正确性、安全、兼容性和性能验证。
8. [构建基线](build-baseline.md)：已验证工具链、构建时间和产物大小。
9. [开发路线](development-plan.md)：里程碑、依赖和完成定义。
10. [架构决策](decisions/README.md)：已经接受的公共接口、连接、Group Commit、WAL 维护和类型化 Schema 约束。

## 文档规范

- `已决定`：已成为当前实现约束，修改需要 ADR。
- `候选`：需要原型或基准测试验证。
- `探索`：尚未进入实现承诺。
- 示例 API 默认是设计草案，除非显式标记为稳定。

文档中的性能数字应区分目标、实测结果和理论上限，并附带设备、数据规模、编译选项和测试方法。
