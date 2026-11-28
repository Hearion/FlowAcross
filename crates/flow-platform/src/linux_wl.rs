//! Linux（Wayland）各模块共用的部分：判断当前会话该用哪套实现、连接合成器、枚举显示器。
//!
//! Wayland 没有全局键鼠捕获和注入的统一接口，要看合成器支持什么：
//! - **wlroots 系**（Sway、Hyprland 等）：有 `zwlr_virtual_pointer`、`zwp_virtual_keyboard`、
//!   `zwlr_layer_shell`、`zwlr_data_control` 这些协议，见 `linux_wlr_*.rs`。
//! - **GNOME / KDE**：没有上面的注入协议，要走 xdg-desktop-portal，见 `linux_portal.rs`。
//!
//! 显示器的位置和尺寸用合成器的逻辑坐标（`xdg-output`），它本身就是与缩放无关的逻辑点；
//! 缩放 = 当前分辨率 ÷ 逻辑尺寸，所以分数缩放也能算对。

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use flow_core::Rect;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

use crate::linux::{Monitor, os_err};
use crate::{Limits, PlatformError};

/// 当前桌面会话用哪套实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    X11,
    /// Wayland，合成器提供 wlroots 的虚拟输入和 layer-shell 协议。
    Wlr,
    /// Wayland，其他合成器（GNOME、KDE）：xdg-desktop-portal。
    Portal,
}

/// wlroots 后端必须有的协议。
const WLR_REQUIRED: [&str; 3] = [
    "zwlr_virtual_pointer_manager_v1",
    "zwp_virtual_keyboard_manager_v1",
    "zwlr_layer_shell_v1",
];

fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

fn pick_backend(wayland: bool, globals: Option<&[String]>) -> Backend {
    match (wayland, globals) {
        (false, _) => Backend::X11,
        (true, Some(g)) if WLR_REQUIRED.iter().all(|r| g.iter().any(|i| i == r)) => Backend::Wlr,
        (true, _) => Backend::Portal,
    }
}

/// 当前会话的类型、合成器有没有 layer-shell（浮光要用）、以及是不是 Hyprland 并且能直接
/// 注册全局快捷键。进程启动后不会变，只判断一次。
struct Session {
    backend: Backend,
    layer_shell: bool,
    /// 合成器提供 `hyprland-global-shortcuts-v1`，并且有 `hyprctl` 可用的 Hyprland 会话。
    hypr_shortcuts: bool,
}

fn detect(wayland: bool, globals: Option<&[String]>, hypr_session: bool) -> Session {
    let has = |g: &[String], name: &str| g.iter().any(|i| i == name);
    let backend = pick_backend(wayland, globals);
    Session {
        backend,
        layer_shell: globals.is_some_and(|g| has(g, "zwlr_layer_shell_v1")),
        // 只有 Hyprland 有这套协议；再认一下环境变量，免得嵌套（别的合成器里跑的 Hyprland）时
        // 拿 hyprctl 去改错地方。
        hypr_shortcuts: backend == Backend::Wlr
            && globals.is_some_and(|g| has(g, "hyprland_global_shortcuts_manager_v1"))
            && hypr_session,
    }
}

fn session() -> &'static Session {
    static SESSION: OnceLock<Session> = OnceLock::new();
    SESSION.get_or_init(|| {
        let wayland = wayland_session();
        let globals = wayland.then(global_interfaces).and_then(Result::ok);
        let hypr = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some();
        detect(wayland, globals.as_deref(), hypr)
    })
}

pub(crate) fn backend() -> Backend {
    session().backend
}

/// 能不能把全局快捷键注册到 Hyprland（见 `linux_hypr_shortcuts.rs`）。
pub(crate) fn hyprland_shortcuts() -> bool {
    session().hypr_shortcuts
}

/// 这种会话里做不到的切换方式和效果。
fn limits_for(session: &Session) -> Limits {
    match session.backend {
        Backend::X11 => Limits::default(),
        // 看不到全局键盘。
        Backend::Wlr => Limits {
            hold_key: true,
            local_hotkey: true,
            // Hyprland 能把映射的触发键注册成自己的全局快捷键，Sway 没有这个协议。
            local_mapping: !session.hypr_shortcuts,
            dwell: false,
            edge_glow: !session.layer_shell,
            local_input: true,
            // 可以把边缘窗口铺满屏幕接住光标，见 `linux_wlr_input.rs` 的 `apply_summon`。
            commanded_switch: false,
            foreground_app: true,
        },
        // 另外，光标一撞上屏障合成器就开始捕获，等不了停留时间；GNOME 没有 layer-shell，画不了浮光。
        Backend::Portal => Limits {
            hold_key: true,
            local_hotkey: true,
            local_mapping: true,
            dwell: true,
            edge_glow: !session.layer_shell,
            local_input: true,
            commanded_switch: true,
            foreground_app: true,
        },
    }
}

pub(crate) fn limits() -> Limits {
    limits_for(session())
}

pub(crate) fn connect() -> Result<Connection, PlatformError> {
    Connection::connect_to_env()
        .map_err(|e| os_err("连不上 Wayland 合成器（请在图形桌面里运行流界）", e))
}

/// 不关心任何事件的状态，只用来做一次性的查询。
struct Probe;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// 合成器提供的全部协议接口名。
fn global_interfaces() -> Result<Vec<String>, PlatformError> {
    let conn = connect()?;
    let (globals, _queue) =
        registry_queue_init::<Probe>(&conn).map_err(|e| os_err("读取 Wayland 协议列表失败", e))?;
    Ok(globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|g| g.interface)
        .collect())
}

/// 合成器里某个接口的全局对象编号和版本。编号对所有客户端都一样，可以跨连接比对。
pub(crate) fn globals_of(globals: &GlobalList, interface: &str) -> Vec<(u32, u32)> {
    globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|g| g.interface == interface)
        .map(|g| (g.name, g.version))
        .collect()
}

/// 自 1970 年以来的毫秒数（只用来比较先后）。
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 流界最近一次注入鼠标移动的时间。Wayland 的事件不带来源，捕获靠它跳过自己注入的移动
/// （相当于另外几个系统的注入标记）。
pub(crate) static LAST_INJECTED_MOVE_MS: AtomicU64 = AtomicU64::new(0);

/// 注入的移动过这么久（毫秒）以内进到屏幕边上的，都算流界自己推过去的。
pub(crate) const INJECT_ECHO_MS: u64 = 150;

pub(crate) fn mark_injected_move() {
    LAST_INJECTED_MOVE_MS.store(now_ms(), Ordering::Relaxed);
}

pub(crate) fn recently_injected_move() -> bool {
    now_ms().saturating_sub(LAST_INJECTED_MOVE_MS.load(Ordering::Relaxed)) < INJECT_ECHO_MS
}

/// 建一个只有文件描述符、没有名字的临时文件并写入内容（键盘映射、共享内存缓冲区都要用）。
/// 放在 `XDG_RUNTIME_DIR`（内存文件系统）里，建好立刻删掉名字。
pub(crate) fn anonymous_file(content: &[u8]) -> std::io::Result<std::fs::File> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!(
        "flowacross-{}-{}-{}",
        std::process::id(),
        now_ms(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path);
    file.write_all(content)?;
    file.flush()?;
    Ok(file)
}

/// 把合成器发来的文件描述符（键盘映射）读成字节。
pub(crate) fn read_fd(fd: OwnedFd, size: usize) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::from(fd);
    let mut buf = vec![0u8; size];
    let mut done = 0;
    while done < size {
        match file.read_at(&mut buf[done..], done as u64)? {
            0 => break,
            n => done += n,
        }
    }
    buf.truncate(done);
    Ok(buf)
}

// ---------------------------------------------------------------------------
// 显示器
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Output {
    global: u32,
    name: Option<String>,
    /// `wl_output.geometry` 的位置（没有 xdg-output 时用）。
    pos: (i32, i32),
    /// 当前分辨率（像素，未旋转）。
    mode: Option<(i32, i32)>,
    rotated: bool,
    int_scale: i32,
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

#[derive(Default)]
struct Outputs {
    list: Vec<Output>,
}

impl Outputs {
    fn get(&mut self, global: u32) -> Option<&mut Output> {
        self.list.iter_mut().find(|o| o.global == global)
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Outputs {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, u32> for Outputs {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(out) = state.get(*global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                out.pos = (x, y);
                out.rotated = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let current =
                    matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current));
                if current || out.mode.is_none() {
                    out.mode = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => out.int_scale = factor,
            wl_output::Event::Name { name } => out.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for Outputs {
    fn event(
        _: &mut Self,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for Outputs {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(out) = state.get(*global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => out.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                out.logical_size = Some((width, height))
            }
            zxdg_output_v1::Event::Name { name } if out.name.is_none() => out.name = Some(name),
            _ => {}
        }
    }
}

/// 一块输出收集到的信息换成 [`Monitor`]。信息不全（没有分辨率）时返回 `None`。
fn to_monitor(out: &Output, index: usize) -> Option<Monitor> {
    let (mw, mh) = out.mode?;
    let (mw, mh) = if out.rotated { (mh, mw) } else { (mw, mh) };
    if mw <= 0 || mh <= 0 {
        return None;
    }
    let int_scale = f64::from(out.int_scale.max(1));
    // 没有 xdg-output 时退回整数缩放。
    let (lw, lh) = out.logical_size.filter(|(w, h)| *w > 0 && *h > 0).map_or(
        (f64::from(mw) / int_scale, f64::from(mh) / int_scale),
        |s| (f64::from(s.0), f64::from(s.1)),
    );
    let (lx, ly) = out.logical_pos.unwrap_or(out.pos);
    Some(Monitor {
        id: out
            .name
            .clone()
            .unwrap_or_else(|| format!("output-{}", out.global)),
        index,
        phys: (0, 0, mw, mh),
        scale: (f64::from(mw) / lw).clamp(0.25, 8.0),
        logical: Rect::new(f64::from(lx), f64::from(ly), lw, lh),
        primary: false,
        wl_global: out.global,
    })
}

/// Wayland 没有"主屏"的概念：把左上角在原点的那块当主屏，没有就取第一块。
fn mark_primary(list: &mut [Monitor]) {
    let at_origin = list
        .iter()
        .position(|m| m.logical.x == 0.0 && m.logical.y == 0.0);
    if let Some(m) = list.get_mut(at_origin.unwrap_or(0)) {
        m.primary = true;
    }
}

pub(crate) fn monitors() -> Result<Vec<Monitor>, PlatformError> {
    let fail = |e: &dyn std::fmt::Display| os_err("枚举显示器失败", e);
    let conn = connect()?;
    let (globals, mut queue): (GlobalList, EventQueue<Outputs>) =
        registry_queue_init(&conn).map_err(|e| fail(&e))?;
    let qh = queue.handle();
    let mut state = Outputs::default();
    let xdg: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1> =
        globals.bind(&qh, 1..=3, ()).ok();
    for (global, version) in globals_of(&globals, "wl_output") {
        let output: wl_output::WlOutput =
            globals.registry().bind(global, version.min(4), &qh, global);
        state.list.push(Output {
            global,
            int_scale: 1,
            ..Output::default()
        });
        if let Some(xdg) = &xdg {
            xdg.get_xdg_output(&output, &qh, global);
        }
    }
    // 两次往返：第一次让合成器处理绑定并发来各输出的属性，第二次收 xdg-output 的属性。
    queue.roundtrip(&mut state).map_err(|e| fail(&e))?;
    queue.roundtrip(&mut state).map_err(|e| fail(&e))?;

    let mut list: Vec<Monitor> = Vec::new();
    for out in &state.list {
        if let Some(m) = to_monitor(out, list.len()) {
            // 镜像的输出占同一个矩形，只算一块。
            if !list.iter().any(|x| x.logical == m.logical) {
                list.push(m);
            }
        }
    }
    if list.is_empty() {
        return Err(PlatformError::Os("合成器没有报告任何显示器".into()));
    }
    mark_primary(&mut list);
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn backend_follows_session_and_protocols() {
        assert_eq!(pick_backend(false, None), Backend::X11);
        let sway = names(&[
            "wl_compositor",
            "zwlr_layer_shell_v1",
            "zwlr_virtual_pointer_manager_v1",
            "zwp_virtual_keyboard_manager_v1",
            "zwlr_data_control_manager_v1",
        ]);
        assert_eq!(pick_backend(true, Some(&sway)), Backend::Wlr);
        // KDE 有 layer-shell 但没有虚拟指针：走 portal。
        let kde = names(&[
            "wl_compositor",
            "zwlr_layer_shell_v1",
            "org_kde_plasma_shell",
        ]);
        assert_eq!(pick_backend(true, Some(&kde)), Backend::Portal);
        let gnome = names(&["wl_compositor", "xdg_wm_base", "gtk_shell1"]);
        assert_eq!(pick_backend(true, Some(&gnome)), Backend::Portal);
        // 连不上合成器也按 portal 处理，由它报出具体的错误。
        assert_eq!(pick_backend(true, None), Backend::Portal);
    }

    fn session(b: Backend, layer_shell: bool, hypr: bool) -> Session {
        Session {
            backend: b,
            layer_shell,
            hypr_shortcuts: hypr,
        }
    }

    #[test]
    fn wayland_sessions_report_what_they_cannot_do() {
        assert_eq!(
            limits_for(&session(Backend::X11, false, false)),
            Limits::default()
        );
        let sway = limits_for(&session(Backend::Wlr, true, false));
        assert!(sway.hold_key && sway.local_hotkey && sway.local_input && sway.foreground_app);
        assert!(!sway.dwell && !sway.edge_glow);
        // 只有 Hyprland 能直接注册映射的触发键。
        assert!(sway.local_mapping);
        let hypr = limits_for(&session(Backend::Wlr, true, true));
        assert!(!hypr.local_mapping);
        assert!(hypr.local_hotkey && hypr.hold_key);
        let kde = limits_for(&session(Backend::Portal, true, false));
        assert!(kde.hold_key && kde.local_hotkey && kde.dwell && !kde.edge_glow);
        assert!(kde.local_mapping);
        // GNOME 没有 layer-shell：浮光也没有。
        assert!(limits_for(&session(Backend::Portal, false, false)).edge_glow);
    }

    #[test]
    fn only_hyprland_sessions_can_register_shortcuts() {
        let hypr = names(&[
            "wl_compositor",
            "zwlr_layer_shell_v1",
            "zwlr_virtual_pointer_manager_v1",
            "zwp_virtual_keyboard_manager_v1",
            "hyprland_global_shortcuts_manager_v1",
        ]);
        // 协议在、又是 Hyprland 会话（有 hyprctl）才算。
        let detected = detect(true, Some(&hypr), true);
        assert_eq!(detected.backend, Backend::Wlr);
        assert!(detected.hypr_shortcuts);
        // 不在 Hyprland 会话里（嵌套的合成器）不算。
        assert!(!detect(true, Some(&hypr), false).hypr_shortcuts);
        // Sway 没有这个协议。
        let sway = names(&[
            "zwlr_layer_shell_v1",
            "zwlr_virtual_pointer_manager_v1",
            "zwp_virtual_keyboard_manager_v1",
        ]);
        assert!(!detect(true, Some(&sway), true).hypr_shortcuts);
        // X11 会话里不算。
        assert!(!detect(false, Some(&hypr), true).hypr_shortcuts);
    }

    fn output(mode: (i32, i32), logical: Option<((i32, i32), (i32, i32))>) -> Output {
        Output {
            global: 7,
            name: Some("DP-1".into()),
            pos: (100, 0),
            mode: Some(mode),
            rotated: false,
            int_scale: 2,
            logical_pos: logical.map(|l| l.0),
            logical_size: logical.map(|l| l.1),
        }
    }

    #[test]
    fn fractional_scale_comes_from_mode_and_logical_size() {
        let m = to_monitor(&output((3840, 2160), Some(((1920, 0), (2560, 1440)))), 1).unwrap();
        assert_eq!(m.id, "DP-1");
        assert_eq!(m.logical, Rect::new(1920.0, 0.0, 2560.0, 1440.0));
        assert_eq!(m.scale, 1.5);
        assert_eq!(m.phys, (0, 0, 3840, 2160));
        assert_eq!(m.wl_global, 7);
    }

    #[test]
    fn falls_back_to_integer_scale_without_xdg_output() {
        let m = to_monitor(&output((3840, 2160), None), 0).unwrap();
        assert_eq!(m.logical, Rect::new(100.0, 0.0, 1920.0, 1080.0));
        assert_eq!(m.scale, 2.0);
    }

    #[test]
    fn rotated_outputs_swap_width_and_height() {
        let mut out = output((1920, 1080), Some(((0, 0), (1080, 1920))));
        out.rotated = true;
        let m = to_monitor(&out, 0).unwrap();
        assert_eq!(m.phys, (0, 0, 1080, 1920));
        assert_eq!(m.scale, 1.0);
        // 还没收到分辨率的输出不算。
        let mut blank = output((0, 0), None);
        blank.mode = None;
        assert!(to_monitor(&blank, 0).is_none());
    }

    #[test]
    fn output_at_origin_is_primary() {
        let mk = |x: f64| Monitor {
            id: "x".into(),
            index: 0,
            phys: (0, 0, 1, 1),
            scale: 1.0,
            logical: Rect::new(x, 0.0, 100.0, 100.0),
            primary: false,
            wl_global: 0,
        };
        let mut list = vec![mk(100.0), mk(0.0)];
        mark_primary(&mut list);
        assert!(!list[0].primary && list[1].primary);
        let mut list = vec![mk(50.0), mk(150.0)];
        mark_primary(&mut list);
        assert!(list[0].primary && !list[1].primary);
    }
}
