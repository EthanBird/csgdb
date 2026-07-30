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

## 2026-07-30：M1 连接控制与取消切片

在相同工具链约束下重新执行全 workspace 测试和严格静态检查：

| 项目 | 结果 |
| --- | ---: |
| Rust 单元与 API 测试 | 31 个通过 |
| Rust/C 长查询取消测试 | 2 个通过 |
| C ABI 加密数据库烟雾测试 | 通过 |
| C11 与 C++17 头文件严格编译 | 通过 |

本切片新增连接级变更计数、最后插入 Rowid、自动提交、只读与事务状态、忙等待配置、缓存回收和独立中断句柄。Rust 与 C 测试均在线程间取消真实递归查询，并验证取消后连接可以继续执行。

## 2026-07-30：M1 有界连接管理切片

本切片继续使用相同的加密后端和工具链约束：

| 项目 | 结果 |
| --- | ---: |
| Rust 单元与 API 测试 | 39 个通过 |
| 连接池与调度器专项测试 | 8 个通过 |
| 严格 Clippy | 通过 |
| Rust 1.82 全 workspace check | 通过 |

专项测试覆盖单写互斥、真实只读连接、池耗尽、WAL 快照、队列立即拒绝与超时、写回调 panic、递归写保护、写连接中断和显式关闭所有权。当前数据只证明正确性与资源边界，不作为吞吐或延迟基准。

## 2026-07-30：M1 Checkpoint 与批量事务切片

本切片在相同工具链约束下增加 WAL 维护和原子批量接口：

| 项目 | 结果 |
| --- | ---: |
| Rust 单元与 API 测试 | 43 个通过 |
| 参数化批量提交与整体回滚 | 通过 |
| 长快照干扰与释放后 WAL 截断 | 通过 |
| Rust/C 四种 Checkpoint 模式 | 通过 |
| Checkpoint 帧进度和累计统计 | 通过 |

这里的“批量事务”表示调用方显式提交拥有所有权的参数化语句列表，由单写线程在一个事务中顺序执行；它不等同于自动合并多个调用方任务的 Group Commit。长快照测试关闭连接级自动 Checkpoint，先建立读快照，再提交 128 条写入，验证 PASSIVE 模式留下非零未回收帧，快照结束后 TRUNCATE 完成回收。
