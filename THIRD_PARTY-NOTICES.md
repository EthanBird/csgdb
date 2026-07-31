# Third-Party Notices

CSGDB 自有源代码使用 MIT License。构建产物可能静态链接以下第三方组件，它们继续适用各自许可证：

| 组件 | 用途 | 许可证 |
| --- | --- | --- |
| rusqlite / libsqlite3-sys | Rust 数据库绑定 | MIT |
| SQLCipher | 事务内核与数据库页加密 | BSD-style |
| OpenSSL | 默认加密提供器 | Apache License 2.0 |
| SQLite source incorporated by SQLCipher | SQL 事务内核基础 | Public Domain |
| syn / quote / proc-macro2 | Rust derive 宏解析与生成 | MIT / Apache-2.0 |
| sha2 及其 RustCrypto 依赖 | 编译期 Schema SHA-256 指纹 | MIT / Apache-2.0 |

正式发布源码包和二进制包时，应同时包含锁定版本对应的完整第三方许可证文本。该文件是归属摘要，不替代第三方许可证。
