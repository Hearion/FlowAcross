// 发布版在 Windows 上不弹出控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod config;
mod engine;
mod input;
mod startup;
mod transfer;
mod updater;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};

use flow_core::autolock::{LatencyLevel, latency_level};
use flow_core::layout::DeviceId;

use crate::engine::{
    DeviceStatus, DeviceView, Engine, EngineEvent, EngineOptions, MainThread, Notify,
};

const LOG_KEEP_DAYS: usize = 14;

/// Linux 托盘菜单里「暂停共享」「锁定在本机」两项的文字：点下去会发生什么。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn tray_labels(paused: bool, locked: bool) -> (&'static str, &'static str) {
    (
        if paused {
            "恢复共享"
        } else {
            "暂停共享"
        },
        if locked {
            "取消锁定"
        } else {
            "锁定在本机"
        },
    )
}

/// 托盘菜单里带勾选的几项：（暂停共享、锁定在本机、在菜单栏显示网络状态）。
type TrayFlags = (bool, bool, bool);

/// 托盘菜单「把键鼠切到」里各设备的菜单项编号前缀，后面跟设备 ID。
const SWITCH_PREFIX: &str = "switch:";

/// 托盘菜单「把键鼠切到」里的一项。
#[derive(Debug, Clone, PartialEq)]
struct SwitchEntry {
    id: String,
    label: String,
    enabled: bool,
}

/// 「把键鼠切到」子菜单的内容：本机和每台已配对的设备，各带一句现在的状态。
/// 键鼠所在的那台、没连着的、被暂停的不能点。托盘菜单的勾选在 Linux 上常常不显示，
/// 所以状态一律写成文字。
fn switch_entries(devices: &[DeviceView]) -> Vec<SwitchEntry> {
    devices
        .iter()
        .map(|d| {
            let note = if d.has_input {
                "键鼠在这里"
            } else if d.status == DeviceStatus::Local {
                "本机"
            } else if d.paused {
                "已暂停"
            } else if d.status != DeviceStatus::Connected {
                "没有连着"
            } else if d.remote_paused {
                "对方已暂停"
            } else if d.locked {
                "已锁屏"
            } else {
                ""
            };
            SwitchEntry {
                id: format!("{SWITCH_PREFIX}{}", d.id.0),
                label: if note.is_empty() {
                    d.name.clone()
                } else {
                    format!("{}（{note}）", d.name)
                },
                enabled: d.can_switch && !d.has_input,
            }
        })
        .collect()
}

/// 菜单栏上流界图标按网络状况变的颜色（macOS，设置「在菜单栏显示网络状态」打开时）：
/// 返回（鼠标停在图标上时的提示文字，颜色档）。`None` 是默认颜色：设置关着、暂停了共享、
/// 没有连着的电脑，或者不知道该看哪一台。
///
/// 显示哪一台：本机正在控制的那台；没有的话是正在控制本机的那台；都没有、而只连着一台时显示它。
/// 连着好几台、键鼠又在本机时不知道该显示哪一台，就不显示。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn menu_bar_title(
    enabled: bool,
    paused: bool,
    devices: &[DeviceView],
    controlled_by: Option<&DeviceId>,
) -> Option<(String, LatencyLevel)> {
    if !enabled || paused {
        return None;
    }
    let measured = || {
        devices
            .iter()
            .filter(|d| d.status == DeviceStatus::Connected && d.latency_ms.is_some())
    };
    let only = || {
        let mut it = measured();
        match (it.next(), it.next()) {
            (Some(d), None) => Some(d),
            _ => None,
        }
    };
    let d = measured()
        .find(|d| d.active)
        .or_else(|| controlled_by.and_then(|id| measured().find(|d| &d.id == id)))
        .or_else(only)?;
    let ms = d.latency_ms?;
    let level = d.latency_level.unwrap_or_else(|| latency_level(ms));
    let ms_text = if ms < 1 {
        "<1 ms".to_owned()
    } else {
        format!("{ms} ms")
    };
    let quality = match level {
        LatencyLevel::Good => "流畅",
        LatencyLevel::Fair => "一般",
        LatencyLevel::Poor => "偏高",
    };
    Some((
        format!(
            "{}\n到「{}」的延迟 {ms_text}，{quality}",
            flow_platform::menu_bar::DEFAULT_TOOLTIP,
            d.name
        ),
        level,
    ))
}

/// macOS 菜单栏用的单色模板图标。
#[cfg(target_os = "macos")]
const TRAY_TEMPLATE_PNG: &[u8] = include_bytes!("../icons/tray-template.png");

/// 菜单栏图标颜色的状态（macOS）：状态在会话线程里推送，图标只能在主线程上改。
#[cfg(target_os = "macos")]
struct MenuBarTitle {
    /// 流界的 `NSStatusItem` 的地址；托盘建好之前是 0。
    item: usize,
    /// 应该显示的状态；`None` 是默认颜色。
    want: Option<(String, LatencyLevel)>,
    /// 已经显示的状态。
    shown: Option<(String, LatencyLevel)>,
}

#[cfg(target_os = "macos")]
impl Default for MenuBarTitle {
    /// 托盘建好时图标就是默认颜色：设置关着、没连着时一直不碰它。
    fn default() -> Self {
        Self {
            item: 0,
            want: None,
            shown: None,
        }
    }
}

/// 在主线程上把菜单栏图标改成 `want`（已经是的话什么都不做）。只排队、不等它执行。
#[cfg(target_os = "macos")]
fn apply_menu_bar_title(handle: &AppHandle, title: &Arc<Mutex<MenuBarTitle>>) {
    let title = title.clone();
    let _ = handle.run_on_main_thread(move || {
        let mut t = title.lock().unwrap_or_else(|e| e.into_inner());
        if t.item == 0 || t.shown == t.want {
            return;
        }
        let want = t.want.clone();
        // SAFETY: 在主线程上；`item` 是托盘建好后取到的 NSStatusItem，托盘一直存在到退出。
        unsafe {
            flow_platform::menu_bar::set_status(
                t.item,
                want.as_ref().map(|(text, level)| (text.as_str(), *level)),
                TRAY_TEMPLATE_PNG,
            )
        };
        t.shown = want;
    });
}

/// 日志同时写到终端和 `<数据目录>/logs/flowacross.log.<日期>`，调试两台电脑联调时把日志文件发过来即可。
/// 只保留最近 [`LOG_KEEP_DAYS`] 天，避免长期运行（或被人反复连接刷日志）把磁盘写满。
/// 用环境变量 RUST_LOG 调整级别，例如 `RUST_LOG=debug`。
fn init_logging(dir: &Path) {
    use tracing_appender::rolling::{RollingFileAppender, Rotation};
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let _ = std::fs::create_dir_all(dir);
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("flowacross.log")
        .max_log_files(LOG_KEEP_DAYS)
        .build(dir)
        .unwrap_or_else(|_| tracing_appender::rolling::daily(dir, "flowacross.log"));
    let (file, guard) = tracing_appender::non_blocking(appender);
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

/// 开发调试：FLOWACROSS_PROFILE=b 可以在同一台电脑上再开一个独立身份的实例。
fn debug_profile() -> Option<String> {
    std::env::var("FLOWACROSS_PROFILE")
        .ok()
        .filter(|s| !s.is_empty())
}

fn main() {
    let mut builder = tauri::Builder::default();
    // 单实例必须最先注册：第二个进程在这里就退出，不会走到后面的初始化（不会抢端口、不会装键盘钩子）。
    // 调试用的独立身份实例不受限制。
    if debug_profile().is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // `flowacross --switch <目标>`：让正在运行的流界把键鼠切走，不打扰窗口。
            if let Some(target) = startup::switch_target(&args) {
                tracing::info!(%target, "收到切换命令");
                let result = app
                    .try_state::<Engine>()
                    .ok_or_else(|| "流界还在启动，稍后再试。".to_string())
                    .and_then(|engine| engine.switch_command(target));
                if let Err(reason) = result {
                    tracing::warn!(%target, %reason, "切换命令没有执行");
                    let _ = app.emit("notice", reason);
                }
                return;
            }
            tracing::info!("再次打开流界，显示已有窗口");
            show_main_window(app);
        }));
    }
    let app = builder
        // 不注册设备级输入（Raw Input）。Windows 上进程注册了键盘 Raw Input 后，只要流界窗口在前台，
        // 系统就不再调用本进程的低级键盘钩子，跨屏时键盘就到不了远端。流界不需要这些设备事件。
        .device_event_filter(tauri::DeviceEventFilter::Always)
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .arg(startup::AUTOSTART_ARG)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(updater::PendingUpdate::default())
        .setup(|app| {
            let profile = debug_profile();
            let mut data_dir = app.path().app_data_dir()?;
            if let Some(p) = &profile {
                data_dir = data_dir.join("profiles").join(p);
            }
            let log_dir: PathBuf = data_dir.join("logs");
            init_logging(&log_dir);
            tracing::info!(version = env!("CARGO_PKG_VERSION"), data_dir = %data_dir.display(), "流界启动");
            // 后台运行时不被系统降速，否则跨屏会卡顿。
            flow_platform::keep_responsive();

            let open = MenuItem::with_id(app, "open", "打开流界", true, None::<&str>)?;
            let pause =
                CheckMenuItem::with_id(app, "pause", "暂停共享", true, false, None::<&str>)?;
            let lock =
                CheckMenuItem::with_id(app, "lock", "锁定在本机", true, false, None::<&str>)?;
            // 只有 macOS 的菜单栏能在图标旁边显示文字。
            #[cfg(target_os = "macos")]
            let latency = CheckMenuItem::with_id(
                app,
                "menu_latency",
                "在菜单栏显示网络状态",
                true,
                false,
                None::<&str>,
            )?;
            let logs = MenuItem::with_id(app, "logs", "打开日志文件夹", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出流界", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            // 「把键鼠切到」：键鼠在别的电脑上、鼠标又不方便推过去时用。内容随设备状态变化。
            let switch = Submenu::with_id(app, "switch", "把键鼠切到", true)?;
            let menu = Menu::with_items(
                app,
                &[&open, &sep, &switch, &pause, &lock, &sep, &logs, &quit],
            )?;
            #[cfg(target_os = "macos")]
            menu.insert(&latency, 5)?;
            #[cfg(target_os = "macos")]
            let (latency_item, menu_title) = (
                latency.clone(),
                Arc::new(Mutex::new(MenuBarTitle::default())),
            );
            #[cfg(target_os = "macos")]
            let title_for_tray = menu_title.clone();

            let handle = app.handle().clone();
            let (pause_item, lock_item) = (pause.clone(), lock.clone());
            let switch_menu = switch.clone();
            // 上一次填进子菜单的内容：状态每秒可能推好几次（传文件的进度），没变就不重建菜单。
            let switch_shown: Mutex<Option<Vec<SwitchEntry>>> = Mutex::new(None);
            // 上一次设到托盘上的（序号、（暂停、锁定、菜单栏显示延迟））。改菜单项要等主线程执行完才返回，而状态在
            // 会话收消息的循环里推送：每次都改的话，主线程一忙，键鼠消息就跟着排队。
            let tray_shown: Mutex<(u64, Option<TrayFlags>)> = Mutex::new((0, None));
            let notify: Notify = Arc::new(move |ev| match ev {
                EngineEvent::State(view) => {
                    let flags = (
                        view.settings.sharing_paused,
                        view.locked,
                        view.settings.menu_bar_latency,
                    );
                    let update_flags = {
                        let mut shown = tray_shown.lock().unwrap_or_else(|e| e.into_inner());
                        // 并发推送时先算出来的可能后到：比已经显示的旧就整份不要。
                        if view.seq < shown.0 {
                            return;
                        }
                        shown.0 = view.seq;
                        let changed = shown.1 != Some(flags);
                        shown.1 = Some(flags);
                        changed
                    };
                    #[cfg(target_os = "macos")]
                    {
                        let want = menu_bar_title(
                            view.settings.menu_bar_latency,
                            view.settings.sharing_paused,
                            &view.devices,
                            view.controlled_by.as_ref(),
                        );
                        let mut t = menu_title.lock().unwrap_or_else(|e| e.into_inner());
                        if t.want != want {
                            t.want = want;
                            drop(t);
                            apply_menu_bar_title(&handle, &menu_title);
                        }
                    }
                    let entries = switch_entries(&view.devices);
                    let mut shown = switch_shown.lock().unwrap_or_else(|e| e.into_inner());
                    if shown.as_ref() != Some(&entries) {
                        *shown = Some(entries.clone());
                        // 菜单在主线程上重建：这里只是排队（不等它执行），所以可以拿着锁排，
                        // 保证排队的顺序就是状态变化的顺序，几次重建也不会交叉在一起。
                        let (menu, app) = (switch_menu.clone(), handle.clone());
                        let _ = handle.run_on_main_thread(move || {
                            if let Ok(old) = menu.items() {
                                for item in &old {
                                    let _ = menu.remove(item);
                                }
                            }
                            for e in &entries {
                                if let Ok(item) =
                                    MenuItem::with_id(&app, &e.id, &e.label, e.enabled, None::<&str>)
                                {
                                    let _ = menu.append(&item);
                                }
                            }
                            // 只有本机一台时没有可切的地方。
                            let _ = menu.set_enabled(entries.len() > 1);
                        });
                    }
                    drop(shown);
                    if update_flags {
                        let _ = pause_item.set_checked(flags.0);
                        let _ = lock_item.set_checked(flags.1);
                        #[cfg(target_os = "macos")]
                        let _ = latency_item.set_checked(flags.2);
                        // Linux 的托盘菜单常常不显示勾选状态（用户反馈：点了「暂停共享」再打开还是「暂停共享」），
                        // 改用文字表示当前状态。macOS 和 Windows 的勾选是正常的，保持原样。
                        #[cfg(target_os = "linux")]
                        {
                            let (pause, lock) = tray_labels(flags.0, flags.1);
                            let _ = pause_item.set_text(pause);
                            let _ = lock_item.set_text(lock);
                        }
                    }
                    let _ = handle.emit("state", view);
                }
                EngineEvent::Notice(msg) => {
                    let _ = handle.emit("notice", msg);
                }
                EngineEvent::Transfers(list) => {
                    let _ = handle.emit("transfers", list);
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
            // 从其他电脑复制来的文件放在「下载/流界」（调试用的第二个实例放在「流界-<profile>」）。
            let receive_dir = app.path().download_dir().ok().map(|d| match &profile {
                Some(p) => d.join(format!("流界-{p}")),
                None => d.join("流界"),
            });
            // 在主线程上启动引擎：首次枚举显示器时可以读到系统的显示器名称。
            let engine = tauri::async_runtime::block_on(Engine::start(
                EngineOptions {
                    data_dir,
                    port,
                    discovery: true,
                    input: true,
                    main_thread: Some(main_thread),
                    receive_dir,
                },
                notify,
            ))?;
            app.manage(engine);

            if let (Some(p), Some(w)) = (&profile, app.get_webview_window("main")) {
                let _ = w.set_title(&format!("流界 FlowAcross（{p}）"));
            }

            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip(flow_platform::menu_bar::DEFAULT_TOOLTIP)
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
                        // 和设置页里的开关是同一项设置，改完界面和菜单的勾选一起更新。
                        "menu_latency" => {
                            let mut s = engine.settings();
                            s.menu_bar_latency = !s.menu_bar_latency;
                            engine.update_settings(s);
                        }
                        "logs" => commands::reveal(&log_dir),
                        "quit" => {
                            engine.shutdown();
                            app.exit(0);
                        }
                        id => {
                            if let Some(device) = id.strip_prefix(SWITCH_PREFIX) {
                                let device = flow_core::layout::DeviceId(device.to_owned());
                                if let Err(reason) = engine.switch_to(&device) {
                                    // 托盘菜单没有地方显示原因：打开窗口，用界面的提示说明。
                                    show_main_window(app);
                                    let _ = app.emit("notice", reason);
                                }
                            }
                        }
                    }
                });
            // macOS 菜单栏用单色模板图标（只有黑色和透明），系统按菜单栏的深浅自动反色，
            // 和其他菜单栏图标风格一致；Windows 和 Linux 的托盘照常用彩色的应用图标。
            #[cfg(target_os = "macos")]
            {
                let icon = tauri::image::Image::from_bytes(TRAY_TEMPLATE_PNG)?;
                tray = tray.icon(icon).icon_as_template(true);
            }
            #[cfg(not(target_os = "macos"))]
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            let tray = tray.build(app)?;
            // 菜单栏的文字要改在 NSStatusItem 上。取它要等主线程执行，而这里就在主线程上，
            // 所以放到别的线程去等；取到之后把已经算好的文字补上。
            #[cfg(target_os = "macos")]
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let item = tray.with_inner_tray_icon(|t| {
                        t.ns_status_item().map(|i| {
                            let p = &*i as *const _ as usize;
                            // 托盘一直存在到退出：留着这份引用，地址始终有效。
                            std::mem::forget(i);
                            p
                        })
                    });
                    if let Ok(Some(item)) = item {
                        title_for_tray.lock().unwrap_or_else(|e| e.into_inner()).item = item;
                        apply_menu_bar_title(&handle, &title_for_tray);
                    }
                });
            }
            #[cfg(not(target_os = "macos"))]
            drop(tray);
            // 窗口在配置里默认隐藏：开机自启时只在托盘 / 菜单栏运行，其余情况启动后显示。
            if startup::start_hidden(app.handle()) {
                tracing::info!("开机自启，窗口保持隐藏");
            } else {
                show_main_window(app.handle());
            }
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
            commands::wake_device,
            commands::switch_to_device,
            commands::set_hotkeys_suspended,
            commands::retry_authorization,
            commands::move_device,
            commands::pair_with,
            commands::respond_pairing,
            commands::remove_device,
            commands::connect_device,
            commands::request_permission,
            commands::open_permission_settings,
            commands::open_logs,
            commands::open_data_dir,
            commands::set_low_latency,
            commands::remove_low_latency,
            commands::accept_files,
            commands::cancel_transfer,
            commands::dismiss_transfer,
            commands::open_received_files,
            commands::dismiss_config_warning,
            updater::check_update,
            updater::install_update,
            updater::restart_app,
            updater::open_download_page,
            startup::get_autostart,
            startup::set_autostart,
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

#[cfg(test)]
mod tests {
    use super::*;
    use flow_core::protocol::Os;

    fn dev(id: &str, name: &str, status: DeviceStatus) -> DeviceView {
        DeviceView {
            id: DeviceId(id.into()),
            name: name.into(),
            os: Os::Windows,
            status,
            manual: false,
            active: false,
            has_input: false,
            paused: false,
            remote_paused: false,
            can_wake: false,
            locked: false,
            can_switch: false,
            can_send_keys: false,
            latency_ms: None,
            latency_level: None,
            problem: None,
        }
    }

    #[test]
    fn tray_switch_entries_describe_each_device() {
        let local = DeviceView {
            has_input: true,
            ..dev("me", "我的 Mac", DeviceStatus::Local)
        };
        let online = DeviceView {
            can_switch: true,
            ..dev("pc", "书房台式机", DeviceStatus::Connected)
        };
        let locked = DeviceView {
            can_switch: true,
            locked: true,
            ..dev("nb", "笔记本", DeviceStatus::Connected)
        };
        let offline = dev("old", "旧电脑", DeviceStatus::Offline);
        let entries = switch_entries(&[local, online, locked, offline]);
        let pairs: Vec<(&str, &str, bool)> = entries
            .iter()
            .map(|e| (e.id.as_str(), e.label.as_str(), e.enabled))
            .collect();
        assert_eq!(
            pairs,
            vec![
                // 键鼠已经在本机：不能点。
                ("switch:me", "我的 Mac（键鼠在这里）", false),
                ("switch:pc", "书房台式机", true),
                // 锁着屏也能切过去（至少能点亮屏幕、看到它），只是标出来。
                ("switch:nb", "笔记本（已锁屏）", true),
                ("switch:old", "旧电脑（没有连着）", false),
            ]
        );
    }

    #[test]
    fn menu_bar_icon_color_follows_the_computer_in_use() {
        let with = |id: &str, ms: u32, active: bool| DeviceView {
            latency_ms: Some(ms),
            latency_level: Some(latency_level(ms)),
            active,
            ..dev(id, id, DeviceStatus::Connected)
        };
        let tip = |name: &str, ms: &str, q: &str| {
            format!("流界 FlowAcross\n到「{name}」的延迟 {ms}，{q}")
        };
        let local = dev("me", "me", DeviceStatus::Local);
        let pc = with("pc", 3, false);
        let nb = with("nb", 45, false);
        // 关着：不显示。
        assert_eq!(
            menu_bar_title(false, false, &[local.clone(), pc.clone()], None),
            None
        );
        // 只连着一台：显示它。
        assert_eq!(
            menu_bar_title(true, false, &[local.clone(), pc.clone()], None),
            Some((tip("pc", "3 ms", "流畅"), LatencyLevel::Good))
        );
        // 连着两台、键鼠在本机：不知道显示哪台。
        assert_eq!(
            menu_bar_title(true, false, &[local.clone(), pc.clone(), nb.clone()], None),
            None
        );
        // 正在控制其中一台：显示它。
        let nb_active = with("nb", 45, true);
        assert_eq!(
            menu_bar_title(true, false, &[local.clone(), pc.clone(), nb_active], None),
            Some((tip("nb", "45 ms", "一般"), LatencyLevel::Fair))
        );
        // 本机正被其中一台控制：显示控制方。
        assert_eq!(
            menu_bar_title(
                true,
                false,
                &[local.clone(), pc.clone(), nb],
                Some(&DeviceId("pc".into()))
            ),
            Some((tip("pc", "3 ms", "流畅"), LatencyLevel::Good))
        );
        // 不到 1 毫秒；偏高。
        assert_eq!(
            menu_bar_title(true, false, &[local.clone(), with("pc", 0, false)], None),
            Some((tip("pc", "<1 ms", "流畅"), LatencyLevel::Good))
        );
        assert_eq!(
            menu_bar_title(true, false, &[local.clone(), with("pc", 120, false)], None)
                .map(|t| t.1),
            Some(LatencyLevel::Poor)
        );
        // 暂停共享：默认颜色。
        assert_eq!(
            menu_bar_title(true, true, &[local.clone(), pc.clone()], None),
            None
        );
        // 没连着、还没量到延迟的不算。
        let offline = DeviceView {
            latency_ms: Some(5),
            ..dev("old", "old", DeviceStatus::Offline)
        };
        assert_eq!(menu_bar_title(true, false, &[local, offline], None), None);
    }

    #[test]
    fn linux_tray_labels_say_what_a_click_does() {
        assert_eq!(tray_labels(false, false), ("暂停共享", "锁定在本机"));
        assert_eq!(tray_labels(true, true), ("恢复共享", "取消锁定"));
    }
}
