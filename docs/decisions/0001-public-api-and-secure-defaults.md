# ADR 0001：公共接口与安全默认值

- Status: Accepted
- Date: 2026-07-30

## Context

CSGDB 需要兼顾普通嵌入式 SQL 使用方式、Rust 类型安全、多语言绑定和 Agent 扩展。加密应默认开启，但数据库文件仍使用常见 `.db` 扩展名。

## Decision

1. 主 C ABI 使用 `csgdb_*`，opaque handle 和版本化配置结构体。
2. Rust API 提供安全 Builder，不暴露底层 FFI 指针。
3. 所有主数据库统一使用 `.db`，扩展名不表达加密状态。
4. 未指定安全模式时自动选择加密。
5. 明文必须显式选择。
6. 缺少密钥或 KeyProvider 时安全失败，禁止固定默认密钥和明文降级。
7. AgentPlan、CSG-Q、全文、向量和图查询建立在基础 SQL 接口之上。
8. `sqlite3_*` 符号兼容层与主库分离。

## Consequences

- 普通使用路径保持熟悉。
- 现有数据库工具无法仅通过扩展名判断文件是否加密。
- 应用在无系统密钥库的嵌入式平台必须提供 KeyProvider。
- 加密主库不保证被普通未配置密钥的数据库工具读取。
- 每个新增 C ABI 结构体必须使用 `struct_size` 和 `abi_version`。
