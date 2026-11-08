# 流界 FlowAcross

**一套键鼠，自由跨屏。**

把几台电脑放在桌上，鼠标从一台的屏幕边缘移出，就出现在相邻电脑的屏幕上，键盘随之切换。支持 Windows 与 macOS 互相控制，局域网直连、端到端加密、无需注册账号。

> 当前进度：界面、真实显示器、系统权限检测、局域网发现、加密连接、配对和自动重连已完成；键鼠捕获/注入与跨屏切换（M3）已实现，正在真机验证。详见 [docs/roadmap.md](docs/roadmap.md)。

## 文档

- [技术架构](docs/architecture.md)：选型、坐标空间、切换判定、键盘处理、网络与配对、失败处理
- [开发路线图](docs/roadmap.md)：M0–M5 里程碑与验收标准
- [在 Windows 上调试](docs/windows.md)：拷贝源码、一键配置环境、测试清单、连不上时怎么查

## 目录

```
crates/flow-core       纯逻辑：布局、跨边映射、切换判定、按键路由、协议、配对码、剪贴板防回环
crates/flow-platform   平台层：显示器枚举、系统权限、键鼠捕获/注入
crates/flow-net        网络层：设备身份、mDNS 发现、QUIC 双向认证连接
app/src                React 界面（设备布局图）
app/src-tauri          Tauri 2 外壳：引擎（配置、握手、配对、重连）、托盘/菜单栏、Tauri 命令
docs/                  架构与路线图
```

## 开发环境

- Rust stable（≥ 1.85）：<https://rustup.rs>
- Node.js ≥ 20
- macOS：Xcode Command Line Tools；Windows：Visual Studio C++ 生成工具 + WebView2

## 常用命令

安装前端依赖（首次）：

```bash
cd app && npm install
```

运行全部 Rust 测试：

```bash
cargo test --workspace
```

代码检查：

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
```

前端类型检查与构建：

```bash
cd app && npm run build
```

启动桌面应用开发模式（会同时启动 Vite 开发服务器）：

```bash
cd app && npm run tauri dev
```

打包 macOS 安装包（release 版 .app + .dmg，复制到 `share/`）：

```bash
scripts/build-mac.sh
```

发布正式版本（自动打包 Windows + macOS 并推送应用内更新）：改版本号、打 `v*` 标签推送即可，一次性配置和步骤见 [docs/release.md](docs/release.md)。

## macOS 系统权限（开发时必读）

macOS 按"负责启动的应用"记录辅助功能、输入监控权限：

- **`npm run tauri dev` 开发模式**：权限算在你运行命令的终端上（系统设置列表里显示为「终端」、iTerm 或编辑器），列表里不会出现流界。在「隐私与安全性 › 辅助功能」和「输入监控」里打开那个终端的开关，再重新运行 `npm run tauri dev`。
- **打包成独立 App 测试**：列表里会显示 FlowAcross。

  ```bash
  cd app && npm run build:mac-app
  ```

  ```bash
  open ../target/debug/bundle/macos/FlowAcross.app
  ```

  **先创建本地签名证书（每台 Mac 只需一次）**，否则每次重新打包签名都会变，系统设置里的旧授权随之失效，得删掉 FlowAcross 条目再重新授权：

  ```bash
  scripts/setup-mac-signing.sh
  ```

  它在登录钥匙串里创建自签名证书「FlowAcross Dev」，`scripts/build-mac.sh` 会自动用它签名。之后系统按"证书 + 包名"识别流界，授权一次长期有效。这个证书只用于本机开发测试，发布给别人需要 Apple Developer ID 签名和公证。

## 在一台 Mac 上测试配对

没有第二台电脑时，可以在同一台电脑上再开一个独立身份的实例（数据保存在单独的 profile 目录，端口随机）。先正常启动开发模式：

```bash
cd app && npm run tauri dev
```

开发模式启动完成后，在另一个终端启动第二个实例（复用同一个开发服务器）：

```bash
FLOWACROSS_PROFILE=b ./target/debug/flowacross
```

两个窗口会互相出现在"添加设备"里，可以完整走一遍配对、断开重连和移除流程。

应用数据目录（macOS）：`~/Library/Application Support/app.flowacross.desktop/`，其中 `config.json` 是设置与已配对设备，`identity.*.der` 是设备身份。删除整个目录即可回到首次启动状态。
