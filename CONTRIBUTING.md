# Contributing to CSGDB

感谢你参与 CSGDB。项目当前优先建立可靠的存储与查询基础，任何新增能力都应说明其正确性边界、性能成本、设备影响和恢复策略。

## 开发约定

1. 在 Issue 或设计讨论中说明问题、使用场景和预期行为。
2. 对文件格式、事务、加密、查询 IR 或公共 API 的修改先提交 ADR。
3. 每个 Pull Request 尽量只处理一个清晰目标。
4. 新功能必须同时提交测试和必要文档。
5. 性能优化必须提供可复现基准、测试设备和前后对比。
6. 不允许用跳过同步、关闭校验或降低加密强度的方式制造默认性能优势。

## 建议的提交前检查

项目代码建立后，统一使用以下检查：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --workspace --doc
```

涉及存储、事务、迁移或加密的变更还应运行：

- 故障注入和断电恢复测试；
- 并发模型测试；
- 模糊测试与属性测试；
- 旧文件格式兼容性测试；
- 加密与错误密钥负向测试；
- ARM64 设备基准测试。

## 分支与提交

- 功能分支：`feature/<short-name>`
- 修复分支：`fix/<short-name>`
- 文档分支：`docs/<short-name>`
- 提交信息使用简短祈使句，并能独立解释变更。

## 设计决策

重大决策应在 `docs/decisions/` 中使用 ADR 记录，至少包含：

- 背景与问题；
- 决策；
- 被拒绝的方案；
- 正确性与安全影响；
- 性能与兼容性影响；
- 回滚方式。

## 安全问题

安全漏洞不要直接公开细节。请按照 [SECURITY.md](SECURITY.md) 使用 GitHub 的私密漏洞报告渠道。

## License

提交代码即表示你同意相关贡献按照仓库的 MIT License 发布。
