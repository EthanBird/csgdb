# 构建基线

## 2026-07-30：M1 首个事务内核切片

环境：

- Linux x86_64；
- Rust 1.97.1，另用 Rust 1.82.0 验证 MSRV；
- SQLCipher 与 OpenSSL 均从源码静态构建；
- `cargo build --workspace --release --locked`；
- 未使用 LTO、`panic = abort` 或面向体积的 profile 调优。

结果：

| 项目 | 结果 |
| --- | ---: |
| 首次完整 release 构建墙钟时间 | 2 分 15 秒 |
| `libcsgdb.so` | 7.4 MiB |
| stripped `libcsgdb.so` | 6.7 MiB |
| `libcsgdb.a` 静态归档 | 42 MiB |
| `csg` CLI | 444 KiB |
| Rust 1.82 全 workspace check | 1 分 40 秒 |

静态归档包含尚未经过最终链接裁剪的目标文件，不能直接等同于应用增量体积。后续基线应同时记录 Android ARM64、嵌入式 Linux ARM64、LTO、符号裁剪和不同密码学提供器的结果。

这一轮只记录构建成本和构造物大小，不代表运行时事务性能。事务吞吐、提交延迟、打开耗时、WAL 增长和加密开销将在 Statement API 稳定后使用固定数据集单独测量。

## 2026-07-30：M1 Statement 与参数绑定切片

在同一环境使用 `cargo build --workspace --release --locked` 重新验证：

| 项目 | 结果 |
| --- | ---: |
| `libcsgdb.so` | 7.5 MiB |
| stripped `libcsgdb.so` | 6.7 MiB |
| `libcsgdb.a` 静态归档 | 42 MiB |
| `csg` CLI | 444 KiB |
| Rust 单元与 API 测试 | 28 个通过 |
| Rust 1.82 全 workspace check | 1 分 54 秒 |

动态库新增并实际导出了 Prepare、Bind、Step、Column、Reset、Clear Bindings 和 Finalize 函数族。C11 烟雾测试通过动态链接完成了加密 `.db` 的参数化插入及逐行读取；头文件同时通过 C11 与 C++17 严格编译。
