//! 开机启动与单实例。
//!
//! - 开机启动用官方插件 `tauri-plugin-autostart`：Windows 写当前用户的 `Run` 注册表项，
//!   macOS 写 `~/Library/LaunchAgents/FlowAcross.plist`。启动项带 [`AUTOSTART_ARG`]，
//!   这样开机自启时只在托盘 / 菜单栏运行，不弹出窗口。
//! - 单实例用 `tauri-plugin-single-instance`：再次打开流界时第二个进程直接退出，
//!   已经在运行的流界把窗口显示到前台。
//! - 开关状态不存配置文件，每次都读系统里实际的注册状态（用户可能在任务管理器或系统设置里改过）。

use std::path::PathBuf;

use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;

/// 开机自启时传给流界的参数。
pub const AUTOSTART_ARG: &str = "--autostart";

/// 本次启动是否由开机启动项触发。
pub fn launched_by_autostart() -> bool {
    std::env::args_os().skip(1).any(|a| a == AUTOSTART_ARG)
}

/// 更新后重新打开时会沿用原来的启动参数。开机自启的流界更新后不应该"消失"，
/// 所以重新打开前留一个标记，下次启动看到标记就照常显示窗口。
fn show_marker(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("show-window-once"))
}

/// 让下一次启动（即使带着 [`AUTOSTART_ARG`]）照常显示窗口。
pub fn show_window_on_next_start(app: &AppHandle) {
    if let Some(p) = show_marker(app) {
        let _ = std::fs::write(p, b"");
    }
}

/// 启动时是否应该隐藏窗口：开机自启且没有"下次显示窗口"的标记。标记读一次就删掉。
pub fn start_hidden(app: &AppHandle) -> bool {
    let forced_show = show_marker(app).is_some_and(|p| std::fs::remove_file(p).is_ok());
    launched_by_autostart() && !forced_show
}

fn read_state(app: &AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| {
        tracing::warn!(error = %e, "读取开机启动状态失败");
        format!("无法读取开机启动状态（{e}）。请重新打开流界后再试。")
    })
}

/// 系统里是否已注册开机启动。
#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<bool, String> {
    read_state(&app)
}

/// 开启 / 关闭开机启动，返回操作后系统里实际的注册状态。
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, enabled, "修改开机启动失败");
        let action = if enabled { "开启" } else { "关闭" };
        return Err(format!("{action}开机启动失败（{e}）。{}", fix_hint()));
    }
    let actual = read_state(&app)?;
    tracing::info!(enabled = actual, "开机启动已更新");
    if actual != enabled {
        return Err(format!(
            "系统没有接受这次修改，开机启动仍为{}。{}",
            if actual { "开启" } else { "关闭" },
            fix_hint()
        ));
    }
    Ok(actual)
}

#[cfg(target_os = "windows")]
fn fix_hint() -> &'static str {
    "可能是安全软件拦截了修改启动项，请在安全软件里放行流界，或打开「任务管理器 › 启动应用」手动设置 FlowAcross。"
}

#[cfg(target_os = "macos")]
fn fix_hint() -> &'static str {
    "请确认「~/Library/LaunchAgents」文件夹可以写入，或在「系统设置 › 通用 › 登录项」里手动设置流界。"
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn fix_hint() -> &'static str {
    "请在系统的开机启动设置里手动设置流界。"
}
