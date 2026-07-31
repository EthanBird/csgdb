# Architecture Decision Records

重大架构决策使用 ADR 记录。文件命名：

```text
NNNN-short-title.md
```

状态包括 Proposed、Accepted、Superseded 和 Rejected。新 ADR 不修改旧决策的历史内容；替代决策通过新文件引用被替代的 ADR。

当前记录：

- [0001：公共接口与安全默认值](0001-public-api-and-secure-defaults.md)
- [0002：有界连接管理器](0002-bounded-connection-manager.md)
- [0003：可控 WAL 维护与参数化批量事务](0003-controlled-wal-and-batch-transactions.md)
- [0004：有界 Group Commit 与 WAL 压力维护](0004-bounded-group-commit-and-wal-pressure.md)
