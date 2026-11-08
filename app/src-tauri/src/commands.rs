//! 暴露给界面的 Tauri 命令，都是对 [`Engine`] 的薄封装。状态变化通过 `state` 事件推送。

use std::path::Path;

use flow_core::layout::DeviceId;
use flow_platform::PermissionState;
use tauri::State;

use crate::config::Settings;
use crate::engine::{Engine, StateView, parse_permission};

#[tauri::command]
pub fn get_state(engine: State<'_, Engine>) -> StateView {
    engine.view()
}

#[tauri::command]
pub fn set_device_name(engine: State<'_, Engine>, name: String) {
    engine.set_device_name(&name);
}

#[tauri::command]
pub fn finish_onboarding(engine: State<'_, Engine>) {
    engine.finish_onboarding();
}

#[tauri::command]
pub fn update_settings(engine: State<'_, Engine>, settings: Settings) {
    engine.update_settings(settings);
}

#[tauri::command]
pub fn set_locked(engine: State<'_, Engine>, locked: bool) {
    engine.set_locked(locked);
}

#[tauri::command]
pub fn set_device_paused(engine: State<'_, Engine>, device: DeviceId, paused: bool) {
    engine.set_device_paused(&device, paused);
}

/// 拖动设备结束：位移单位为布局空间逻辑点。失败时返回给用户看的原因。
#[tauri::command]
pub fn move_device(
    engine: State<'_, Engine>,
    device: DeviceId,
    dx: f64,
    dy: f64,
    snap: Option<f64>,
) -> Result<(), String> {
    engine.move_device(&device, dx, dy, snap)
}

#[tauri::command]
pub fn pair_with(engine: State<'_, Engine>, device: DeviceId) {
    engine.pair_with(&device);
}

#[tauri::command]
pub fn respond_pairing(engine: State<'_, Engine>, device: DeviceId, accept: bool) {
    engine.respond_pairing(&device, accept);
}

/// 手动连接一台已配对但离线的设备。
#[tauri::command]
pub fn connect_device(engine: State<'_, Engine>, device: DeviceId) {
    engine.connect_device(&device);
}

#[tauri::command]
pub fn remove_device(engine: State<'_, Engine>, device: DeviceId) {
    engine.remove_device(&device);
}

/// 申请权限。macOS 的授权弹窗只会出现一次（拒绝过或问过之后再调用不会有任何反应），
/// 所以申请后仍未授权时，直接打开系统设置里对应的页面。
#[tauri::command]
pub fn request_permission(engine: State<'_, Engine>, kind: String) -> StateView {
    if let Some(p) = parse_permission(&kind) {
        let perms = flow_platform::permissions();
        perms.request(p);
        if perms.state(p) != PermissionState::Granted {
            let _ = perms.open_settings(p);
        }
    }
    engine.view()
}

/// 用系统文件管理器打开目录。
pub fn reveal(dir: &Path) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(dir).spawn();
}

#[tauri::command]
pub fn open_logs(engine: State<'_, Engine>) {
    reveal(&engine.data_dir().join("logs"));
}

#[tauri::command]
pub fn open_data_dir(engine: State<'_, Engine>) {
    reveal(engine.data_dir());
}

/// 开启或关闭低延迟模式（仅 macOS）。第一次开启会弹出管理员授权窗口，放到阻塞线程里等。
#[tauri::command]
pub async fn set_low_latency(engine: State<'_, Engine>, enabled: bool) -> Result<(), String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.set_low_latency(enabled))
        .await
        .map_err(|e| e.to_string())?
}

/// 关闭低延迟模式并移除后台服务（弹出管理员授权窗口）。
#[tauri::command]
pub async fn remove_low_latency(engine: State<'_, Engine>) -> Result<(), String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.remove_low_latency())
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn open_permission_settings(kind: String) -> Result<(), String> {
    let p = parse_permission(&kind).ok_or("未知的权限类型")?;
    flow_platform::permissions()
        .open_settings(p)
        .map_err(|e| e.to_string())
}
