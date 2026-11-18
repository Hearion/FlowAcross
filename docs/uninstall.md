# 卸载与清理 / Uninstallation and cleanup

## Windows

1. 如果启用了“设置 → 通用 → 开机启动”，先关闭它。通过托盘菜单“退出流界”结束应用，释放共享中的按键和连接。
2. 打开 Windows“设置 → 应用 → 已安装的应用”，找到 FlowAcross 并卸载。便携运行的副本在退出后手动删除其程序目录。
3. 如需清除本地身份、配对、设置和日志，在卸载前通过“设置 → 关于 → 数据目录”记录实际路径；退出后删除该目录。删除身份后需要重新配对。不要直接照抄其他电脑的路径。
4. 如需删除收到的文件，单独清理系统“下载/流界”目录；确认不再需要后再删除。按需清理系统临时目录内的 `flowacross-clipboard`。
5. 在其他已配对电脑上移除这台设备。若防火墙仍保留指向 FlowAcross 程序的规则，可在 Windows 防火墙管理界面核对路径后删除对应规则。

## macOS

1. 先在“设置 → 通用”关闭开机启动。如果启用过“设置 → 跨屏与键鼠 → 低延迟模式”，使用该区域的“移除后台服务”操作，并按系统要求输入管理员授权。单纯把应用拖到废纸篓不会卸载此服务。
2. 通过菜单栏“退出流界”，再把“应用程序”中的 FlowAcross.app 移到废纸篓。
3. 如需清除数据，在退出后删除实际应用数据目录。默认位置为 `~/Library/Application Support/app.flowacross.desktop/`，也可事先通过“设置 → 关于”核对。收到的文件和临时剪贴板缓存需分别清理。
4. 在“系统设置 → 隐私与安全性”中移除 FlowAcross 的辅助功能/输入监控授权，在其他电脑上移除配对。按需检查“系统设置 → 通用 → 登录项”是否仍有对应项。

可选低延迟服务在当前代码中使用 `/Library/LaunchDaemons/app.flowacross.awdl-guard.plist` 和 `/Library/Application Support/FlowAcross/awdl-guard.sh`。优先使用应用内移除功能，避免只删除一部分文件而留下加载中的服务。

## Linux

1. 先关闭应用的开机启动并从托盘退出流界。通过安装时的软件包管理器卸载对应 FlowAcross 包；Arch 包名为 `flowacross-bin`。如果使用 AppImage，退出后删除该 AppImage 文件及自己创建的启动入口。
2. 要清除设置、配对身份和日志，卸载前从“设置 → 关于 → 数据目录”记录实际路径，退出后再清理。收到的文件和系统临时目录中的 `flowacross-clipboard` 需分别检查。
3. 如曾通过 Wayland portal 授权，在桌面环境的权限管理中按需撤销对应授权，并在其他电脑上移除配对。

## English

Disable launch at login, remove the optional macOS low-latency helper through the application's settings if installed, then quit FlowAcross before uninstalling. On Windows, use Settings → Apps → Installed apps. On macOS, remove FlowAcross.app after removing the optional helper. On Linux, uninstall the installed package using its package manager (Arch package: `flowacross-bin`), or delete the AppImage and any manually created launcher. Revoke Wayland portal permissions through the desktop's permission controls if applicable.

Uninstallation does not guarantee removal of received files, temporary clipboard images, logs or user configuration. Before uninstalling, find the actual data directory under Settings → About. Remove it after quitting if you want to erase the identity and pairings. Separately review Downloads/流界 and the temporary `flowacross-clipboard` directory. Removing identity data requires pairing again. Revoke OS permissions and remove the old peer on the other computers as needed. Do not delete unrelated files or firewall rules.
