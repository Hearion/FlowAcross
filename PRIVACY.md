# 流界 FlowAcross 隐私说明 / Privacy policy

更新日期：2026-11-19。适用对象：本仓库提供的 FlowAcross 桌面应用。网站、GitHub 和操作系统自身的数据处理另受各自政策约束。

## 中文

### 应用处理什么数据

FlowAcross 用于在你选择并配对的电脑之间共享键鼠和剪贴板，无需注册 FlowAcross 账号。当前应用代码未集成广告、使用分析或自动崩溃上报服务；维护者不运营用于中转键鼠、剪贴板或文件的服务器。

| 场景 | 数据与接收方 | 控制方式 |
|---|---|---|
| 局域网自动发现 | 向同一局域网广播设备名称、证书指纹形成的设备 ID、系统类型、服务端口和网络地址。这些发现信息不是加密的，尚未配对的局域网设备也可能看到 | 修改设备名称可以避免广播个人姓名；退出应用停止发现。暂停共享不等于停止发现 |
| 配对与连接 | 与目标电脑交换身份验证信息、设备名称、软件版本和显示器布局；配对成功后保存可信设备记录 | 只与自己信任的电脑核对并确认配对；可以移除已配对设备 |
| 键鼠共享 | 通过加密的设备间连接向当前目标电脑发送键盘、鼠标及控制状态 | 暂停共享、锁定在本机或退出应用；不要将敏感输入发送到不可信设备 |
| 剪贴板共享 | 启用时在已配对连接之间发送剪贴板文本；启用文件共享时还会发送文件清单、图片及文件内容，具体传输由共享状态和跨屏操作触发 | 在“设置 → 剪贴板”关闭共享，或暂停全部共享。剪贴板可能包含密码、截图和其他敏感信息 |
| 检查和下载更新 | 正式构建默认开启自动检查：启动约 15 秒后及此后每 6 小时先访问下载域名的阿里云 OSS 更新清单，失败时回退 GitHub；手动检查也使用这些地址。请求接收方可见 IP 地址、请求路径及常规 HTTP 信息，云平台可能产生访问或安全日志。应用不会在更新请求中主动附加剪贴板、文件或键鼠内容；Linux 上用 deb 或 Arch 安装包安装的版本，更新时由系统的包管理器安装，需要在系统的授权窗口里输入密码 | 在“设置 → 软件更新”关闭自动检查；关闭后仍可手动检查。新版本需要用户选择安装；下载页及手动下载同样会产生网络请求 |

更新清单先请求 <https://dl.bapihou.com/FlowAcross/latest.json>，失败时回退 <https://github.com/Hearion/FlowAcross-releases/releases/latest/download/latest.json>；安装包从清单中指定的地址下载。`dl.bapihou.com` 是维护者管理、由阿里云 OSS 提供文件分发的下载域名。服务商的说明见 [阿里云隐私政策](https://help.aliyun.com/zh/document_detail/2705226.html) 和 [GitHub 隐私声明](https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement)。这里不声称云端没有访问日志。应用字体随前端一起打包，正常显示文字不需要向字体网站请求字体。

Windows 界面依赖 Microsoft WebView2。当前安装器采用 Tauri 默认的 WebView2 下载引导方式：如果系统缺少所需运行时，安装过程可能访问 Microsoft 的下载服务并安装运行时。Microsoft 运行时的更新和数据处理不由 FlowAcross 的更新开关控制，见 [Microsoft 隐私声明](https://www.microsoft.com/en-us/privacy/privacystatement) 和 [Tauri 安装方式说明](https://v2.tauri.app/distribute/windows-installer/#webview2-installation-options)。

### 本地存储与删除

- 应用数据目录保存设置、可信设备、显示器布局、最近连接地址、本机身份公钥证书及私钥。实际路径见“设置 → 关于 → 数据目录”；调试 profile 使用独立子目录。私钥不要公开或发给他人。
- `logs/` 保存本机运行日志，可能含设备名、网络地址、文件路径、设备 ID 片段和错误详情；应用不自动上传日志。常规配置限制保留最近 14 个每日日志文件；清理失败或回退路径可能保留更久。提交诊断信息前请自行审阅和遮盖敏感字段。
- 收到的文件和图片保存在系统“下载/流界”目录；复制图片时，最近一张会缓存在流界数据目录的 `clipboard-image` 下（0.2.3 及更早的版本放在系统临时目录的 `flowacross-clipboard` 下）。清空传输列表或卸载程序不代表这些文件都被删除。
- 移除设备会删除本机的对应配对记录，但不会删除远端已经收到的数据。若要撤销双方信任，请在两台电脑上都移除配对。
- 退出后可手动删除数据目录、接收文件和临时缓存。删除身份会导致以后需要重新配对。完整操作见 [卸载与清理](docs/uninstall.md)。

### 系统权限与可选修改

应用需要系统允许的键鼠捕获和输入注入能力；macOS 需要辅助功能/输入监控等授权，Linux Wayland 的部分桌面环境会通过系统 portal 请求远程桌面等授权。开机自启由用户在设置中开启或关闭。macOS 的可选低延迟模式会在管理员授权后安装后台服务并调整 AWDL 网络接口状态；可在设置中关闭并移除服务。详细清理步骤见卸载文档。

### 联系

一般问题可提交 [GitHub Issue](https://github.com/Hearion/FlowAcross/issues)。敏感隐私或安全问题可联系维护者：xiaoyan00110@outlook.com。邮件和 Issue 是你主动交给对应服务及维护者的信息，请不要附带私钥、密码或无关的个人数据。

## English

FlowAcross shares keyboard, mouse and clipboard data directly between computers chosen and paired by the user. No FlowAcross account is required. The application currently includes no advertising, usage analytics or automatic crash reporting service. The maintainer does not operate a relay for shared input, clipboard contents or files.

- **Discovery:** mDNS advertises the device name, certificate-derived device ID, OS type, port and addresses to the local network. Discovery metadata is not encrypted and can be visible before pairing. Renaming the device reduces identifying information. Quitting stops discovery; pausing sharing does not stop discovery.
- **Paired connections:** authentication information, device names, application versions and display layouts are exchanged. Shared input and clipboard/file contents travel over encrypted direct connections. Users can pause sharing, disable clipboard/file sharing, remove peers, or quit. Share only with trusted computers.
- **Updates and downloads:** Production builds check automatically by default, approximately 15 seconds after startup and every six hours. They first contact the maintainer-controlled `dl.bapihou.com` domain backed by Alibaba Cloud OSS, then fall back to GitHub if the primary manifest is unavailable. Packages are downloaded from the URLs in the manifest. These services receive IP addresses, request paths and normal HTTP information, and may retain access/security logs. FlowAcross does not deliberately attach shared input, clipboard or file contents to these requests. Disable automatic checks in Settings → Software updates; manual requests remain available. Installation requires user action. On Linux, versions installed from the deb or Arch package are updated through the system package manager, which asks for administrator authorization in a system dialog. See the linked Alibaba Cloud and GitHub privacy policies.
- **Windows runtime:** the installer uses Tauri's default WebView2 download bootstrapper mode and may contact Microsoft to install a missing runtime. WebView2's own updates and data processing are separate from FlowAcross's update setting; see the linked Microsoft privacy statement.
- **Local data:** settings, trusted peers, recent addresses, display layouts and identity keys remain in the application data directory, available through Settings → About. Local logs may contain names, addresses, paths, identifiers and error details; they are not automatically uploaded. Normal logging retains up to 14 daily files, with possible longer retention on cleanup/fallback failures. Review logs before sharing them.
- **Received files and cached images:** these can remain under Downloads/流界 and the `clipboard-image` folder inside the FlowAcross data directory (versions up to 0.2.3 used a `flowacross-clipboard` folder in the system temporary directory) until manually removed. Uninstalling or clearing a transfer list does not guarantee their removal. Removing a peer does not erase files already received on that peer.
- **System changes:** startup registration is user-controlled. macOS permissions and Linux Wayland portal requests are handled through the OS. The optional macOS low-latency feature installs a privileged service after administrator authorization; remove it in settings before uninstalling.

See [uninstallation and cleanup](docs/uninstall.md) for local deletion steps. The bundled font is loaded locally. This policy covers the desktop application, not independent behavior of the website, GitHub, the OS or modified builds. Contact: xiaoyan00110@outlook.com, or the repository Issues page for non-sensitive questions.
