# 流界 FlowAcross 开发路线图

每个里程碑结束时都应当"能跑、能测、能演示"。勾选表示已完成。

## M0 工程基础

- [x] 技术选型与架构文档
- [x] Rust workspace + Tauri 2 + React 骨架
- [x] `flow-core`：布局空间、跨边映射、切换判定、按键状态跟踪、协议编解码、配对码
- [x] `flow-core` 单元测试
- [x] `flow-platform` trait 定义
- [x] CI：macOS + Windows 上跑 fmt / clippy / test（`.github/workflows/ci.yml`，已在 GitHub 上通过）
- [x] 界面视觉设计（布局图、跨屏边、设置面板）

## M1 本机输入（平台层）

- [x] macOS：CGEventTap 捕获并可拦截键鼠；CGEventPost 注入；虚拟键码 ↔ HID
- [x] Windows：低级钩子捕获；SendInput 注入；扫描码 ↔ HID；Per-Monitor DPI v2（仅通过交叉编译检查，待真机验证；Raw Input 视需要再加）
- [x] 枚举显示器（位置、逻辑尺寸、缩放）：macOS 已实现；Windows 已实现、仅通过交叉编译检查，待真机验证
- [x] 权限检测与跳转系统设置（macOS：辅助功能、输入监控）
- [ ] `flow-cli` 调试工具：录制本机输入 / 回放注入

**验收**：单机上用 `flow-cli` 回放一段录制的键鼠操作，结果和手动操作一致。

## M2 网络、发现与配对

- [x] 设备身份：密钥 + 自签名证书，本地持久化
- [x] mDNS 广播与发现
- [x] QUIC 连接：双向证书认证 + 控制流（数据报、剪贴板流随 M3/M5 加入）
- [x] 配对流程：双方显示 6 位码、双方确认、保存可信设备；移除时双方同步删除
- [x] 自动重连（退避、mDNS 重新发现立即重试、上次地址兜底；约 30 秒内重试 5 次仍失败就停止，改为手动连接）
- [x] 两台真机互相发现与配对的手动验证（Mac + Windows）

**验收**：两台电脑互相发现，完成配对，重启应用后自动重连。

## M3 跨屏控制（端到端）

- [x] Engine 状态机：本机 / 控制远端 / 被控制（`app/src-tauri/src/input.rs` + engine）
- [x] 边缘切换 + 快捷键切换（Ctrl+Alt+←/→）+ 锁定
- [x] 防误触：修饰键、停留时间、角落死区
- [x] 源侧拦截本机输入、隐藏并锁定本机光标（Windows 把光标停在离开的那个边缘点，暂不隐藏）
- [x] 心跳、超时释放、断网恢复本机
- [x] Mac ⌘ ↔ Win Ctrl 互换（Mac 与 Windows 之间自动启用）
- [ ] Mac ↔ Windows 真机互控验证（点击、滚动、拖拽、打字、中文输入法、断网不卡键）

**验收**：Mac 和 Windows 互控，能正常点击、滚动、拖拽、打字（含中文输入法），拔网线后两边都不卡键。

## M4 界面与引导

- [x] 首次引导：设备名称 → 权限 → 发现附近设备 → 配对
- [x] 设备布局图：拖动卡片、吸附、多显示器、当前控制高亮、连接状态（布局保存在本地配置）
- [ ] 托盘 / 菜单栏：暂停共享、锁定（已完成）、切换到指定设备（待 M3）
- [ ] 设置页：防误触、剪贴板、开机启动已可保存；快捷键自定义待做
- [ ] 连接失败的原因与建议文案

## M5 剪贴板与发布

- [x] 跨设备文本复制粘贴，防回环，可独立关闭（每 0.5 秒检查剪贴板变化；待真机验证）
- [x] 跨设备复制文件：复制后键鼠切过去自动传输并写入对方剪贴板，带进度、取消、失败提示，大于 1 GB 手动接收（两台引擎回环测试通过，macOS 代码仅通过交叉编译检查；待 Mac ↔ Windows 真机验证）
- [x] 跨设备复制图片：截图或复制的图片转成 PNG 走文件通道，对方收完后把图片写入剪贴板（两台引擎回环测试和位图转换单元测试通过，Windows 代码仅通过交叉编译检查；待 Mac ↔ Windows 真机验证）
- [x] 边缘浮光：键鼠来到本机时在进来的那条屏幕边上闪一道光，可在设置里关闭（形状和节奏的计算有单元测试；macOS 用 `cargo run -p flow-platform --example edge_glow` 在本机跑通，Windows 已通过四条屏幕边的原生合成像素、透明背景、焦点和鼠标穿透验证；待 Mac ↔ Windows 真机验证）
- [ ] 休眠 / 唤醒 / 锁屏处理
- [x] 开机启动、单实例：开机自启时只在托盘 / 菜单栏运行，再次打开流界时显示已有窗口（Windows 已本机验证隐藏启动和单实例，开关写入启动项待手动验证；macOS 未编译、未在真机验证）
- [ ] 打包：macOS 签名 + 公证，Windows 签名安装包（推标签自动打包发布已完成，v0.1.0 已发布，见 `docs/release.md`；macOS 目前用自签名证书，Windows 未签名）
- [x] 自动更新：发布到公开仓库 FlowAcross-releases，App 定期检查、提示后下载验签安装（待下一个版本验证从 v0.1.0 升级）

## 后续版本

- 跨屏拖拽传输（复制文件的传输通道已就绪）
- 设备布局在各台电脑之间同步（用户需求：在一台上拖动屏幕位置，另一台实时跟着变；要定好两边同时拖动时以谁为准）
- 快捷键映射（用户需求，类似 Synergy 的 hotkey：在一台上按 Ctrl+Alt+S，另一台收到 S）
- 文件传输进度显示在系统通知 / 托盘里（窗口隐藏时也能看到）
- Linux 支持
  - [x] 第 0 步：工程在 Linux 上能编译，CI 加 Ubuntu
  - [x] 第 1 步：X11 平台层（`crates/flow-platform/src/linux*.rs`）：XInput2 捕获 + 抓取锁定、XTest 注入、RandR 显示器、X 选区剪贴板（文本、文件、图片）、边缘浮光。在容器里的虚拟 X 屏幕（Xvfb）上测过：注入落点、捕获键鼠和滚轮、抓取与放开、剪贴板和别的程序（xclip）双向互通（含 4 MB 图片分块）、浮光的显示与消失；CI 的 Ubuntu 任务每次都跑这些测试。**没有在真实的 Linux 桌面和真实键鼠上验证**，和 Mac / Windows 的互控也没有测过
  - [ ] X11 真机验证：GNOME / KDE 的 X11 会话下与 Mac、Windows 互控（点击、滚动、拖拽、打字、中文输入法、复制文件和图片、多显示器、高分屏缩放）
  - [ ] 打包（deb / AppImage）与自动更新：测试版（版本号带后缀，如 `v0.2.2-beta.1`）会打 x86_64 的 deb、AppImage 和 Arch 的 pkg.tar.zst 并发布成 Pre-release，见 `docs/release.md`；正式版还不带 Linux 包，应用内自动更新没有接入
  - [x] Wayland：wlroots 系（Hyprland、Sway）：虚拟指针 / 虚拟键盘注入、屏幕边的 layer-shell 窗口捕获、data-control 剪贴板、layer-shell 浮光（`linux_wl*.rs`、`linux_wlr_input.rs`）。在容器里的无头 sway 上测过整条链路（1.7 不肯锁定指针、1.10 会锁定，两种情况都通过），CI 每次都跑（`scripts/test-wayland.sh`）；**没有在 Hyprland 和真实键鼠上验证**。有用户在 Hyprland 上等这个（FlowAcross-releases 的 Issue #1）
  - [x] Wayland：GNOME / KDE：xdg-desktop-portal 的 RemoteDesktop（注入）+ InputCapture（捕获）+ libei，GNOME 的剪贴板走 Clipboard portal（`linux_portal.rs`）。**只做到编译通过和纯计算部分的单元测试，整条链路没有运行过**（授权弹窗要人来点，无头环境里跑不起来）
  - [ ] Wayland 真机验证：Hyprland、Sway、GNOME（45 以上）、KDE Plasma（6.1 以上）下与 Mac、Windows 互控，尤其是 portal 那一套
  - 已知不足：Wayland 上「按住修饰键才切换」和本机的 Ctrl+Alt+←/→ 不起作用（看不到全局键盘）；GNOME / KDE 上「停留一会儿才切换」不起作用，捕获的授权每次启动都要点；GNOME 上没有边缘浮光。这些设置在界面上不显示，引擎也会忽略（`flow_platform::limits()`）
  - Hyprland 真机反馈（v0.2.2-beta.2，CachyOS，一横一竖两块屏，Linux 做主控端控制 Mac）：立即跨屏、贴边停留、剪贴板文字和图片双向正常。待查：
    - [ ] 「对齐」方式下不是按相贴的部分穿越（读代码没找到原因，等用户补充 `hyprctl monitors` 和具体位置；多显示器加竖屏的组合在无头 sway 里还没测过）
    - [ ] 「屏幕角落不切换」感觉不生效（死区只有 10 个逻辑点，可能是太小，也可能真有问题）
  - 上面的"做不到"是指现有实现，其中几项换个做法可以实现（都还没做，做好后恢复显示对应的设置）：
    - [ ] 按住修饰键才切换：光标碰到屏幕边时让边缘窗口临时拿一下键盘焦点，合成器会同时报告修饰键状态，没按着就还回去（副作用：前台程序短暂失去焦点）；portal 上撞到屏障后同样能拿到修饰键状态
    - [ ] 本机快捷键切换：提供一条命令（如 `flowacross --switch right`）让用户在合成器里自己绑快捷键；或接入全局快捷键 portal（Hyprland、KDE 等支持），后者还能为"快捷键映射"打基础
    - [ ] GNOME / KDE 的贴边停留：撞上屏障后先不放开，在停留时间内看用户是否继续往外推
    - GNOME 的边缘浮光确实难做：没有在屏幕边上放置顶层窗口的协议，只能做成 Shell 扩展
