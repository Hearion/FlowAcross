# 在 Windows 上调试流界

目标：在 Windows 电脑上跑起流界，和 Mac 互相发现、配对、自动重连，并测试键鼠跨屏。

## 1. 把源码拷到 Windows

在 Mac 的项目根目录生成源码压缩包（不含依赖和构建产物，几 MB）：

```bash
scripts/pack-source.sh
```

把生成的 `share/flowacross-src.zip` 用 U 盘、隔空投送到手机再转、网盘等方式拷到 Windows，解压到一个**不含中文和空格的路径**，例如 `C:\dev\FlowAcross`（Rust 的部分工具对中文路径支持不好）。

以后项目推到 Git 仓库后，可以直接在 Windows 上 `git clone`。

## 2. 一键配置开发环境

在解压后的目录打开 PowerShell，运行：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1
```

脚本会按需安装 Visual Studio 2022 生成工具（C++）、WebView2、Rust、Node.js，并执行 `npm install`。已装好的会跳过。Visual Studio 生成工具体积较大，第一次可能要几十分钟。

完成后**关闭并重新打开 PowerShell**，让新的 PATH 生效。

## 3. 启动

```powershell
cd app
npm run tauri dev
```

第一次编译 Rust 依赖需要几分钟。窗口出来后：

1. Windows 防火墙会弹窗询问是否允许访问网络：勾选**专用网络**，点**允许访问**。
2. 首次引导里给这台电脑起个名字。Windows 不需要额外的系统权限。
3. 在 Mac 上也打开流界，两边的「添加设备」里应该能看到对方。

## 4. 测试清单

| 项目 | 怎么测 | 期望 |
|---|---|---|
| 显示器识别 | 看布局图里 Windows 的屏幕 | 数量、位置、分辨率（物理像素）和缩放比例与「设置 › 系统 › 屏幕」一致 |
| 互相发现 | 两边打开「添加设备」 | 几秒内出现对方的名字 |
| 配对 | 一边点「配对」 | 两边弹出相同的 6 位码，都点确认后显示「在线」 |
| 拒绝配对 | 一边点「拒绝」 | 另一边提示「拒绝了配对」，两边都不保存 |
| 布局 | 拖动 Windows 的屏幕贴到 Mac 屏幕旁边 | 自动吸附，出现发光的跨屏边，重启后位置不变 |
| 断线重连 | 关掉一边的流界再打开 / 断开 Wi‑Fi 十几秒再连上 | 自动恢复「在线」 |
| 停止自动连接 | 关掉一边的流界，等 1 分钟 | 另一边提示已停止自动连接，设备显示「已断开」；打开流界后它会主动连回来，也可在设备详情点「连接」 |
| 改名 | 设置 › 通用 改设备名称 | 另一边立刻显示新名字 |
| 移除 | 设备详情 › 移除设备 | 两边都删除配对记录 |

### 键鼠跨屏

先在布局图里把两台电脑的屏幕拖到真实摆放的位置，**让屏幕边相贴**（出现发光的边），鼠标只会从相贴的那段边穿过去。

Mac 上需要授予「辅助功能」和「输入监控」：开发模式下要授权给启动流界的**终端**（或编辑器），授权后**退出并重新运行** `npm run tauri dev`。缺权限时界面会提示。Windows 不需要额外权限。

| 项目 | 怎么测 | 期望 |
|---|---|---|
| 边缘切换 | 把 Mac 的鼠标推过相贴的边 | 光标出现在 Windows 屏幕上，顶部显示「键鼠在 ×××」；Mac 光标隐藏 |
| 切回 | 在 Windows 上把鼠标推回那条边 | 回到 Mac，光标出现在跨回来的位置 |
| 点击 / 双击 / 右键 / 拖拽 | 在对方电脑上操作文件、窗口 | 和本机操作一致 |
| 滚动 | 鼠标滚轮、触控板双指滚动 | 方向正确、速度合理 |
| 打字 | 在记事本里输入英文、中文（对方的输入法） | 字符正确，输入法正常候选 |
| 组合键 | Mac 控制 Windows 时按 ⌘C / ⌘V | Windows 收到 Ctrl+C / Ctrl+V |
| 快捷键切换 | Ctrl + Alt + ←/→（Mac 上 ⌃⌥←/→） | 直接切到左/右边的电脑 |
| 防误触 | 设置里开启「按住按键才跨屏」「贴边停留」 | 不满足条件时不切换 |
| 锁定 / 暂停 | 顶部「锁定在本机」「暂停共享」 | 鼠标推到边上不切换 |
| 剪贴板 | 在一台电脑复制一段文字（含中文），切到另一台粘贴 | 1 秒内可以粘贴出相同内容；设置里关掉「共享剪贴板」后不再同步 |
| 流界窗口在前台 | Windows 当前窗口是流界时，用 Windows 键盘控制 Mac 打字 | 和其他窗口在前台时一样能打字 |
| 断网不卡键 | 在对方电脑上按住 Shift，同时断开 Wi‑Fi | 本机立即恢复可用；对方 3 秒内自动松开 Shift |

已知限制：
- 控制 Mac 期间，Windows 的光标会停在离开的屏幕边上（暂不隐藏），回到 Windows 后恢复。
- Windows 上无法操作「以管理员身份运行」的窗口和 UAC 弹窗，除非流界也以管理员身份运行；Ctrl+Alt+Del 无法传递。
- macOS 上输入密码时（安全输入模式）系统会屏蔽键盘捕获。

## 5. 编译报错

### `error: linker 'link.exe' not found`

Rust 找不到微软的 C++ 链接器，说明 Visual Studio 的 C++ 生成工具没装上（常见于电脑上原本就有 Visual Studio / Build Tools，但没勾选 C++）。任选一种方式修复：

- **重新运行配置脚本**（已修复：会检测并补装缺少的 C++ 组件，最后确认 `link.exe` 存在）：

  ```powershell
  powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1
  ```

- **手动安装**：开始菜单打开「Visual Studio Installer」→ 在 Build Tools 2022（或你的 Visual Studio）上点「修改」→ 勾选「使用 C++ 的桌面开发」（右侧确认包含 MSVC 生成工具和 Windows 11 SDK）→ 安装。

装好后**重新打开 PowerShell** 再运行 `npm run tauri dev`。如果安装器提示需要重启，先重启。

### 其他编译错误

把完整报错（尤其是第一条 `error` 及上下几行）发给开发者。

## 6. 连不上时

- **两台电脑必须在同一个局域网**。公司网络、酒店和公共 Wi‑Fi 常会屏蔽设备发现，可以先用家里的网络或手机热点测试。
- **Windows 网络要设为「专用网络」**：设置 › 网络和 Internet › 当前网络 › 属性 › 网络配置文件类型。公用网络下 Windows 会拦截局域网发现。
- **防火墙**：如果第一次弹窗时点了取消，在「Windows 安全中心 › 防火墙和网络保护 › 允许应用通过防火墙」里找到 flowacross，勾选「专用」。
- **Mac 端**：开发模式下第一次运行时 macOS 会询问「本地网络」权限，要选允许；如果点了拒绝，到「系统设置 › 隐私与安全性 › 本地网络」里打开终端（或 FlowAcross）的开关。

## 7. 日志

两边都会写日志，排查问题时把两边的日志一起发过来：

- 托盘 / 菜单栏图标 › 打开日志文件夹，或「设置 › 关于 › 打开日志文件夹」
- Windows：`%APPDATA%\app.flowacross.desktop\logs\`
- macOS：`~/Library/Application Support/app.flowacross.desktop/logs/`

需要更详细的日志时，启动前设置环境变量：

```powershell
$env:RUST_LOG="debug"; npm run tauri dev
```

## 8. 打包安装程序（可选）

在 Windows 上：

```powershell
cd app
npm run build:win
```

安装程序在 `target\release\bundle\nsis\` 下。目前没有代码签名，安装时 SmartScreen 会提示「未知发布者」，点「更多信息 › 仍要运行」即可。

也可以在 GitHub 仓库的 Actions 页面手动运行「Windows 测试包」工作流，构建好后从运行结果的附件下载安装程序。正式发版见 [release.md](release.md)。
