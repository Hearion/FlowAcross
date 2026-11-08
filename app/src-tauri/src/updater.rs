//! 应用内更新：从公开的发布仓库读取 `latest.json`，校验签名后下载安装。
//!
//! 安装前先关闭引擎（释放远端按键、恢复本机输入、断开连接），再替换程序并重新打开，
//! 和托盘里「退出流界」走同一条路径，避免安装过程中卡键。

use std::sync::Mutex;
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

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
    pub date: Option<String>,
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
    }
}

async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    let updater = app
        .updater()
        .map_err(|e| format!("更新功能初始化失败：{e}"))?;
    let found = updater.check().await.map_err(|e| {
        tracing::warn!(error = %e, "检查更新失败");
        "检查更新失败，请确认网络可以访问 GitHub 后重试。".to_string()
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
    tracing::info!(version = %update.version, "开始下载更新");

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

    // 先释放远端按键、恢复本机输入、断开连接，再替换程序。
    tracing::info!(version = %update.version, "下载完成，关闭引擎后安装");
    engine.shutdown();
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

/// 安装失败后重新打开流界。
#[tauri::command]
pub fn restart_app(app: AppHandle) {
    app.restart();
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
