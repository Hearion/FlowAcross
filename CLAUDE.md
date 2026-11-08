# 流界 FlowAcross 项目约定

- 产品目标、架构和里程碑分别见 `docs/architecture.md`、`docs/roadmap.md`。架构有变化时同步更新文档，完成里程碑条目时在 roadmap 里勾选。
- `crates/flow-core` 只放纯逻辑，不引入 IO、异步运行时或系统 API；新增逻辑必须带单元测试。时间一律由调用方以毫秒传入，不在 core 里读时钟。
- 系统 API 只出现在 `crates/flow-platform`（按 `cfg(target_os)` 分模块），网络只出现在 `crates/flow-net`（M2 新建）。
- 网络上的键码一律用 USB HID Usage（`flow_core::keys::KeyCode`），坐标一律用布局空间逻辑点。
- 任何失败路径都要先释放远端按键、恢复本机输入，再提示用户（见架构文档第 9 节）。
- 面向用户的文案用简洁中文，报错要给出原因和处理建议。
- 提交前：`cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、`cd app && npm run build` 全部通过。
