// 发布版在 Windows 上不弹出控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod config;
mod engine;
mod input;
mod updater;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};

use crate::engine::{Engine, EngineEvent, EngineOptions, MainThread, Notify};

/// 日志同时写到终端和 `<数据目录>/logs/flowacross.log.<日期>`，调试两台电脑联调时把日志文件发过来即可。
/// 用环境变量 RUST_LOG 调整级别，例如 `RUST_LOG=debug`。
fn init_logging(dir: &Path) {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let _ = std::fs::create_dir_all(dir);
    let (file, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(dir, "flowacross.log"));
    // 进程存活期间一直需要这个 guard（负责把缓冲写入文件）。
    std::mem::forget(guard);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,flowacross=debug,flow_net=debug"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .with(fmt::layer().with_ansi(false).with_writer(file))
        .try_init();
}

fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(updater::PendingUpdate::default())
        .setup(|app| {
            // 开发调试：FLOWACROSS_PROFILE=b 可以在同一台电脑上再开一个独立身份的实例。
            let profile = std::env::var("FLOWACROSS_PROFILE")
                .ok()
                .filter(|s| !s.is_empty());
            let mut data_dir = app.path().app_data_dir()?;
            if let Some(p) = &profile {
                data_dir = data_dir.join("profiles").join(p);
            }
            let log_dir: PathBuf = data_dir.join("logs");
            init_logging(&log_dir);
            tracing::info!(version = env!("CARGO_PKG_VERSION"), data_dir = %data_dir.display(), "流界启动");

            let open = MenuItem::with_id(app, "open", "打开流界", true, None::<&str>)?;
            let pause =
                CheckMenuItem::with_id(app, "pause", "暂停共享", true, false, None::<&str>)?;
            let lock =
                CheckMenuItem::with_id(app, "lock", "锁定在当前电脑", true, false, None::<&str>)?;
            let logs = MenuItem::with_id(app, "logs", "打开日志文件夹", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出流界", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open, &sep, &pause, &lock, &sep, &logs, &quit])?;

            let handle = app.handle().clone();
            let (pause_item, lock_item) = (pause.clone(), lock.clone());
            let notify: Notify = Arc::new(move |ev| match ev {
                EngineEvent::State(view) => {
                    let _ = pause_item.set_checked(view.settings.sharing_paused);
                    let _ = lock_item.set_checked(view.locked);
                    let _ = handle.emit("state", view);
                }
                EngineEvent::Notice(msg) => {
                    let _ = handle.emit("notice", msg);
                }
            });
            let port = if profile.is_some() {
                0
            } else {
                flow_net::DEFAULT_PORT
            };
            let main_handle = app.handle().clone();
            let main_thread: MainThread = Arc::new(move |f| {
                let _ = main_handle.run_on_main_thread(f);
            });
            // 在主线程上启动引擎：首次枚举显示器时可以读到系统的显示器名称。
            let engine = tauri::async_runtime::block_on(Engine::start(
                EngineOptions {
                    data_dir,
                    port,
                    discovery: true,
                    input: true,
                    main_thread: Some(main_thread),
                },
                notify,
            ))?;
            app.manage(engine);

            if let (Some(p), Some(w)) = (&profile, app.get_webview_window("main")) {
                let _ = w.set_title(&format!("流界 FlowAcross（{p}）"));
            }

            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("流界 FlowAcross")
                .menu(&menu)
                .on_menu_event(move |app, event| {
                    let engine = app.state::<Engine>();
                    match event.id.as_ref() {
                        "open" => show_main_window(app),
                        "pause" => {
                            let mut s = engine.settings();
                            s.sharing_paused = !s.sharing_paused;
                            engine.update_settings(s);
                        }
                        "lock" => engine.set_locked(!engine.locked()),
                        "logs" => commands::reveal(&log_dir),
                        "quit" => {
                            engine.shutdown();
                            app.exit(0);
                        }
                        _ => {}
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            updater::spawn_auto_check(app.handle().clone());
            Ok(())
        })
        // 关闭窗口时收起到托盘 / 菜单栏，而不是退出。
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::set_device_name,
            commands::finish_onboarding,
            commands::update_settings,
            commands::set_locked,
            commands::set_device_paused,
            commands::move_device,
            commands::pair_with,
            commands::respond_pairing,
            commands::remove_device,
            commands::connect_device,
            commands::request_permission,
            commands::open_permission_settings,
            commands::open_logs,
            commands::open_data_dir,
            updater::check_update,
            updater::install_update,
            updater::restart_app,
        ])
        .build(tauri::generate_context!())
        .expect("流界启动失败");

    app.run(|app, event| match event {
        RunEvent::ExitRequested { .. } => {
            if let Some(engine) = app.try_state::<Engine>() {
                engine.shutdown();
            }
        }
        // macOS：点击程序坞图标时重新显示窗口。
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main_window(app),
        _ => {}
    });
}
