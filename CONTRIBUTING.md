# 参与 FlowAcross

提交 Issue 或 Pull Request 前，请先阅读 README 和相关架构文档。外部贡献由维护者审查后合并，尤其注意键鼠释放、配对、文件传输、依赖和构建流程的变更。

## 许可证

除已有文件明确保留的第三方许可证外，你有权提交的项目原创贡献按 `GPL-3.0-only` 提供，与项目许可一致。保留现有版权及第三方声明。提交第三方代码时必须说明来源、原许可证及修改情况；不要加入无权再许可的代码或资源。

## 验证

按 [CLAUDE.md](CLAUDE.md) 运行格式、Clippy、Rust 测试和前端构建；改了 `scripts/` 下的发版脚本时再跑 `node --test "scripts/*.test.mjs"`（CI 也会跑）。依赖或锁文件变化后运行 `node scripts/generate-third-party-notices.mjs`，检查第三方声明和源码获取方式。

## 仓库与签名权限

拥有仓库写权限或 SignPath 访问权限的成员必须开启多因素认证。外部贡献不得直接取得生产签名凭据；生产发布需要维护者人工批准。完整角色和发布控制见 [Code signing policy](docs/code-signing-policy.md)。

安全或隐私问题请联系 xiaoyan00110@outlook.com，不要在公开 Issue 中粘贴私钥、密码或敏感日志。
