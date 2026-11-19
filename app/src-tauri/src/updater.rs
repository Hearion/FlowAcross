//! 应用内更新：读取 `latest.json`，校验签名后下载安装。
//!
//! 清单地址在 `tauri.conf.json` 的 `plugins.updater.endpoints` 里，按顺序尝试：先读阿里云 OSS 上的
//! （国内快），读不到（连不上、出错）再读 GitHub 发布仓库的。安装包从清单里写的地址下载，
//! OSS 的清单就从 OSS 下载。两边的文件相同，签名都用同一把私钥校验，放在哪里都一样安全。
//!
//! 安装前先关闭引擎（释放远端按键、恢复本机输入、断开连接），再替换程序并重新打开，
//! 和托盘里「退出流界」走同一条路径，避免安装过程中卡键。
//!
//! Linux 上按"这份流界是怎么装上的"（[`InstallKind`]）分三种装法，清单里各有一项：
//! AppImage 直接替换文件；deb 和 Arch 包下载验签后交给系统的包管理器（`pkexec` 弹出系统的
//! 授权窗口）。包管理器装的是 root 的文件，用户可能取消授权，所以这三种都是先装好、再关引擎
//! 重新打开，装不成时流界照常运行。认不出装法的（自己编译的）只提示有新版本，不自动安装。

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::engine::Engine;

/// 启动后等一会儿再检查，避开启动时的连接和权限检测。
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(15);
/// 之后每隔多久检查一次。
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// 下载进度事件的最小间隔。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// 官网的下载页：认不出安装方式、或自动安装不成时让用户去这里下载。
const DOWNLOAD_PAGE: &str = "https://flow.bapihou.com";

/// 这份流界是怎么装上的，决定更新怎么装。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    /// Windows 安装包、macOS 应用：交给更新插件。
    Bundled,
    /// Linux 的 AppImage：替换文件本身。
    AppImage,
    /// Linux 的 deb：下载后 `dpkg -i`。
    Deb,
    /// Arch 系的安装包：下载后 `pacman -U`。
    Pacman,
    /// Linux 上认不出来（自己编译的、别人重新打包的）：只提示有新版本，请用户自己安装。
    Manual,
}

/// Linux 上程序文件归哪个包管理器管。`owned_by(工具)`：用这个工具查，程序文件是不是它装的。
///
/// 不看程序里记的打包类型：Arch 的安装包是把 deb 里的文件原样重新打包的，程序里记的仍是 deb，
/// 照着去跑 `dpkg -i` 一定失败。
fn linux_package_kind(owned_by: impl Fn(&str) -> bool) -> InstallKind {
    if owned_by("dpkg") {
        InstallKind::Deb
    } else if owned_by("pacman") {
        InstallKind::Pacman
    } else {
        InstallKind::Manual
    }
}

/// 是不是从 AppImage 里运行的。只看 `APPIMAGE` 环境变量不够：从 AppImage 打包的终端或编辑器里
/// 启动别的程序时，子进程会继承它——那样的流界会把"更新"写到父程序的 AppImage 文件上。
/// AppImage 运行时还会把挂载点放进 `APPDIR`，程序文件在它下面才算数。
fn runs_from_appimage(
    appimage_set: bool,
    appdir: Option<&std::path::Path>,
    exe: &std::path::Path,
) -> bool {
    appimage_set && appdir.is_some_and(|dir| exe.starts_with(dir))
}

/// 这份流界的安装方式。只查一次：要启动 `dpkg-query` / `pacman` 子进程，
/// 第一次调用放在阻塞线程里（见 `check`）。
pub fn install_kind() -> InstallKind {
    static KIND: OnceLock<InstallKind> = OnceLock::new();
    *KIND.get_or_init(|| {
        if !cfg!(target_os = "linux") {
            return InstallKind::Bundled;
        }
        let Ok(exe) = std::env::current_exe() else {
            return InstallKind::Manual;
        };
        if runs_from_appimage(
            std::env::var_os("APPIMAGE").is_some(),
            std::env::var_os("APPDIR")
                .as_deref()
                .map(std::path::Path::new),
            &exe,
        ) {
            return InstallKind::AppImage;
        }
        let kind = linux_package_kind(|tool| {
            let (program, flag) = match tool {
                "dpkg" => ("dpkg-query", "-S"),
                _ => ("pacman", "-Qo"),
            };
            Command::new(program)
                .arg(flag)
                .arg(&exe)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        });
        tracing::info!(?kind, exe = %exe.display(), "安装方式");
        kind
    })
}

/// 在 `latest.json` 里找哪一项。`None`：用插件默认的（Windows、macOS）。
///
/// Linux 的 deb 和 Arch 包用流界自己起的名字（按安装它的工具叫 `dpkg`、`pacman`），不用插件
/// 默认会找的 `linux-<架构>-deb` 和 `linux-<架构>`：0.2.3 及更早的 Linux 版不会分辨 Arch，
/// 清单里一旦有那两项，装在 Arch 上的旧版本就会去下 deb、跑 `dpkg -i`，然后报安装失败。
/// AppImage 没有这个问题，沿用插件的名字，旧版本的 AppImage 也能收到更新。
fn update_target(kind: InstallKind, arch: &str) -> Option<String> {
    match kind {
        InstallKind::Bundled => None,
        // 认不出装法时借 AppImage 那一项看版本号，不会去安装它。
        InstallKind::AppImage | InstallKind::Manual => Some(format!("linux-{arch}-appimage")),
        InstallKind::Deb => Some(format!("linux-{arch}-dpkg")),
        InstallKind::Pacman => Some(format!("linux-{arch}-pacman")),
    }
}

/// 交给包管理器安装时，包文件的扩展名和安装命令。
fn package_install(kind: InstallKind) -> Option<(&'static str, &'static str)> {
    match kind {
        InstallKind::Deb => Some(("deb", "dpkg -i")),
        InstallKind::Pacman => Some(("pkg.tar.zst", "pacman -U --noconfirm")),
        _ => None,
    }
}

/// 以 root 运行的安装脚本：安装包从标准输入读进来，写到只有 root 能进的临时目录里再安装。
///
/// 不把验过签名的包先放到用户可写的临时文件再让 root 去装：授权窗口停留期间，同一用户下的
/// 其他进程可以把文件换掉，换来以 root 身份安装任意的包。从内存直接经管道交给 root 就没有
/// 这个空子。
fn root_install_script(ext: &str, install: &str) -> String {
    format!(
        "set -e; d=$(mktemp -d /var/tmp/flowacross-update-XXXXXXXX); trap 'rm -rf \"$d\"' EXIT; \
         cat > \"$d/flowacross.{ext}\"; {install} \"$d/flowacross.{ext}\""
    )
}

/// 把已经验过签名的安装包交给系统的包管理器。`pkexec` 会弹出系统的授权窗口，阻塞到用户操作完。
/// 失败时返回给用户看的原因。
fn install_with_package_manager(kind: InstallKind, bytes: &[u8]) -> Result<(), String> {
    let Some((ext, install)) = package_install(kind) else {
        return Err("这种安装方式不支持自动安装".into());
    };
    let mut child = Command::new("pkexec")
        .args(["/bin/sh", "-c", &root_install_script(ext, install)])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("系统里没有 pkexec，无法请求管理员授权（{e}）"))?;
    // 用户取消授权时对方不读，写入会失败：以退出码为准。
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(bytes);
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("等待安装结束时出错（{e}）"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    tracing::warn!(code = ?out.status.code(), %stderr, "包管理器安装更新失败");
    Err(match out.status.code() {
        Some(126) => "没有授权，已取消安装".into(),
        Some(127) => {
            "没有拿到管理员授权：这个桌面可能没有运行 polkit 认证代理（Sway、Hyprland 上要自己启动一个）".into()
        }
        _ => {
            let last = stderr.lines().rev().find(|l| !l.trim().is_empty());
            match last {
                Some(l) => format!("包管理器报错：{}", l.trim()),
                None => "包管理器没有装上".into(),
            }
        }
    })
}

/// 手动安装的办法（自动安装不成时附在提示后面）。
fn manual_hint(kind: InstallKind) -> &'static str {
    match kind {
        InstallKind::Deb => "也可以到下载页下载 .deb，用 sudo apt install ./文件名.deb 安装。",
        InstallKind::Pacman => "也可以到下载页下载 .pkg.tar.zst，用 sudo pacman -U 文件名 安装。",
        InstallKind::AppImage => {
            "请确认 AppImage 文件所在的文件夹可以写入；也可以到下载页下载新的 AppImage 替换它。"
        }
        _ => "可以到下载页下载安装包手动安装。",
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
    pub date: Option<String>,
    /// 这份流界的安装方式。界面据此说明安装时会发生什么。
    pub kind: InstallKind,
    /// 不能自动安装（认不出安装方式）：界面改为显示「打开下载页」。
    pub manual: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// 更新失败的原因。`restart` 为 true 表示引擎已经关闭，需要重新打开流界才能继续使用。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateError {
    pub message: String,
    pub restart: bool,
}

impl UpdateError {
    fn new(message: String) -> Self {
        Self {
            message,
            restart: false,
        }
    }
}

/// 最近一次检查到的新版本，安装时直接使用。
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

fn info_of(u: &Update) -> UpdateInfo {
    UpdateInfo {
        version: u.version.clone(),
        current: u.current_version.clone(),
        notes: u.body.clone().filter(|s| !s.trim().is_empty()),
        date: u.date.map(|d| d.date().to_string()),
        kind: install_kind(),
        manual: install_kind() == InstallKind::Manual,
    }
}

async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    // 第一次要启动子进程查安装方式，别占着异步运行时的线程。
    let kind = tauri::async_runtime::spawn_blocking(install_kind)
        .await
        .unwrap_or(InstallKind::Manual);
    let mut builder = app.updater_builder();
    if let Some(target) = update_target(kind, std::env::consts::ARCH) {
        builder = builder.target(target);
    }
    let updater = builder
        .build()
        .map_err(|e| format!("更新功能初始化失败：{e}"))?;
    let found = updater.check().await.map_err(|e| {
        tracing::warn!(error = %e, "检查更新失败");
        use tauri_plugin_updater::Error;
        match e {
            // 清单读到了，只是里面没有这台电脑能用的安装包（比如这种处理器架构没有发布）：不是网络问题。
            Error::TargetNotFound(_) | Error::TargetsNotFound(_) => {
                "最新版本没有提供适用于这台电脑的更新包，请到下载页查看。".to_string()
            }
            _ => "检查更新失败，请检查网络后重试。".to_string(),
        }
    })?;
    let info = found.as_ref().map(info_of);
    *app.state::<PendingUpdate>()
        .0
        .lock()
        .expect("pending update") = found;
    Ok(info)
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    check(&app).await
}

#[tauri::command]
pub async fn install_update(app: AppHandle, engine: State<'_, Engine>) -> Result<(), UpdateError> {
    let pending = app
        .state::<PendingUpdate>()
        .0
        .lock()
        .expect("pending update")
        .clone();
    let update =
        pending.ok_or_else(|| UpdateError::new("没有可安装的更新，请先检查更新。".into()))?;
    let kind = install_kind();
    if kind == InstallKind::Manual {
        return Err(UpdateError::new(
            "这份流界不是用官方安装包装的，不能自动更新。请到下载页下载新版本后手动安装。".into(),
        ));
    }
    tracing::info!(version = %update.version, ?kind, "开始下载更新");

    // 下载期间引擎照常工作，下载失败不影响使用。
    let mut downloaded = 0u64;
    let mut last = Instant::now() - PROGRESS_INTERVAL;
    let progress_app = app.clone();
    let bytes = update
        .download(
            move |chunk, total| {
                downloaded += chunk as u64;
                if last.elapsed() >= PROGRESS_INTERVAL || total == Some(downloaded) {
                    last = Instant::now();
                    let _ =
                        progress_app.emit("update-progress", UpdateProgress { downloaded, total });
                }
            },
            || {},
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "下载更新失败");
            UpdateError::new("下载更新失败，请检查网络后重试。".into())
        })?;

    if kind != InstallKind::Bundled {
        // Linux：先装好再关引擎。授权窗口可能被取消、包管理器可能正忙，装不成时流界照常运行。
        tracing::info!(version = %update.version, ?kind, "下载完成，开始安装");
        let installer = update.clone();
        let installed = tauri::async_runtime::spawn_blocking(move || match kind {
            InstallKind::AppImage => installer.install(bytes).map_err(|e| e.to_string()),
            _ => install_with_package_manager(kind, &bytes),
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        if let Err(e) = installed {
            tracing::error!(error = %e, "安装更新失败");
            return Err(UpdateError::new(format!(
                "安装更新没有成功：{e}。流界仍在运行，不影响使用。{}",
                manual_hint(kind)
            )));
        }
        tracing::info!(version = %update.version, "安装完成，重新打开");
        engine.shutdown();
        crate::startup::show_window_on_next_start(&app);
        app.restart();
    }

    // 先释放远端按键、恢复本机输入、断开连接，再替换程序。
    tracing::info!(version = %update.version, "下载完成，关闭引擎后安装");
    engine.shutdown();
    // 重新打开时会沿用这次的启动参数，开机自启的流界也要照常显示窗口。
    crate::startup::show_window_on_next_start(&app);
    // Windows 上安装程序启动后当前进程会直接退出；macOS 替换完程序包后由这里重新打开。
    if let Err(e) = update.install(bytes) {
        tracing::error!(error = %e, "安装更新失败");
        return Err(UpdateError {
            message: format!(
                "安装更新失败（{e}）。键鼠共享已停止，请重新打开流界；如果仍然失败，可以从发布页下载安装包手动安装。"
            ),
            restart: true,
        });
    }
    app.restart();
}

/// 用系统浏览器打开官网的下载页。
#[tauri::command]
pub fn open_download_page() {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = Command::new(program).arg(DOWNLOAD_PAGE).spawn() {
        tracing::warn!(error = %e, "打开下载页失败");
    }
}

/// 安装失败后重新打开流界。
///
/// 同步命令在主线程上执行，这里不能用 `restart()`：它在主线程上会跳过退出事件，
/// 单实例插件来不及释放锁，新进程会以为流界还在运行而直接退出。
/// `request_restart()` 走正常的退出流程（先关闭引擎、释放单实例锁）再重新打开。
#[tauri::command]
pub fn restart_app(app: AppHandle) {
    crate::startup::show_window_on_next_start(&app);
    app.request_restart();
}

/// 后台定期检查更新，发现新版本时推送 `update-available` 事件（每个版本只提示一次）。
/// 开发版不自动检查，避免调试时被打扰；设置页仍可手动检查。
pub fn spawn_auto_check(app: AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        let mut notified: Option<String> = None;
        loop {
            let enabled = app.state::<Engine>().settings().auto_check_updates;
            if enabled {
                match check(&app).await {
                    Ok(Some(info)) if notified.as_deref() != Some(info.version.as_str()) => {
                        tracing::info!(version = %info.version, "发现新版本");
                        notified = Some(info.version.clone());
                        let _ = app.emit("update-available", info);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(%e, "自动检查更新失败"),
                }
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_install_kind_follows_the_package_manager_that_owns_the_binary() {
        assert_eq!(linux_package_kind(|t| t == "dpkg"), InstallKind::Deb);
        // Arch 的安装包里是 deb 的文件，但归 pacman 管。
        assert_eq!(linux_package_kind(|t| t == "pacman"), InstallKind::Pacman);
        // 两个都不认：自己编译的，或别人重新打包的。
        assert_eq!(linux_package_kind(|_| false), InstallKind::Manual);
    }

    #[test]
    fn appimage_needs_the_binary_inside_its_mount_point() {
        use std::path::Path;
        let mount = Path::new("/tmp/.mount_FlowAcXYZ");
        let inside = Path::new("/tmp/.mount_FlowAcXYZ/usr/bin/flowacross");
        assert!(runs_from_appimage(true, Some(mount), inside));
        // 从别的 AppImage（比如终端）里启动的流界：继承了环境变量，但程序不在那个挂载点里。
        assert!(!runs_from_appimage(
            true,
            Some(mount),
            Path::new("/usr/bin/flowacross")
        ));
        assert!(!runs_from_appimage(true, None, inside));
        assert!(!runs_from_appimage(false, Some(mount), inside));
    }

    #[test]
    fn update_targets_keep_old_linux_versions_away_from_packages() {
        assert_eq!(update_target(InstallKind::Bundled, "x86_64"), None);
        let target = |k| update_target(k, "x86_64").unwrap();
        assert_eq!(target(InstallKind::AppImage), "linux-x86_64-appimage");
        assert_eq!(target(InstallKind::Deb), "linux-x86_64-dpkg");
        assert_eq!(target(InstallKind::Pacman), "linux-x86_64-pacman");
        // 认不出装法时只借 AppImage 那一项看版本号。
        assert_eq!(target(InstallKind::Manual), "linux-x86_64-appimage");
        // 旧版本会找的两项（见 `update_target` 的说明）不能出现。
        for k in [InstallKind::Deb, InstallKind::Pacman] {
            assert!(!["linux-x86_64-deb", "linux-x86_64"].contains(&target(k).as_str()));
        }
    }

    #[test]
    fn root_install_script_reads_the_package_from_stdin() {
        let (ext, install) = package_install(InstallKind::Pacman).unwrap();
        let script = root_install_script(ext, install);
        assert!(script.contains("mktemp -d /var/tmp/flowacross-update-"));
        assert!(script.contains(r#"cat > "$d/flowacross.pkg.tar.zst""#));
        assert!(script.contains(r#"pacman -U --noconfirm "$d/flowacross.pkg.tar.zst""#));
        let (ext, install) = package_install(InstallKind::Deb).unwrap();
        assert!(root_install_script(ext, install).contains(r#"dpkg -i "$d/flowacross.deb""#));
        assert_eq!(package_install(InstallKind::AppImage), None);
    }

    /// 脚本本身能跑：用一个假的"安装命令"代替包管理器，确认包的内容原样到了它手里，
    /// 临时目录用完就删。
    #[cfg(unix)]
    #[test]
    fn root_install_script_runs_under_a_plain_shell() {
        let out = std::env::temp_dir().join(format!("flowacross-script-{}", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let install = format!("cp -- \"$d/flowacross.deb\" {} #", out.display());
        let mut child = Command::new("/bin/sh")
            .args(["-c", &root_install_script("deb", &install)])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"package bytes")
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(std::fs::read(&out).unwrap(), b"package bytes");
        std::fs::remove_file(&out).unwrap();
    }
}
