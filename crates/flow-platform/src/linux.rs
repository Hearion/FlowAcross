//! Linux 实现的入口：按当前桌面会话选 X11、Wayland（wlroots 系）或 Wayland（portal）的实现，
//! 以及它们共用的显示器快照。本文件其余部分是 X11 的显示器枚举和连接。
//!
//! X11 没有可靠的每屏缩放：桌面环境把整体缩放写在 `Xft.dpi` 里（GNOME、KDE 都是 96 × 缩放），
//! 所有显示器共用这一个值。逻辑坐标 = 像素 ÷ 缩放，各屏之间的相对位置不变。
//! Wayland 的显示器枚举见 `linux_wl.rs`。

use std::sync::Mutex;

use flow_core::Rect;
use flow_core::protocol::DisplayInfo;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

use crate::linux_wl::{self, Backend};
use crate::{
    ClipboardAccess, DisplayProvider, InputCapture, InputInjector, PlatformError, linux_clipboard,
    linux_glow, linux_input, linux_portal, linux_wl_clipboard, linux_wl_glow, linux_wlr_input,
};

// ---------------------------------------------------------------------------
// 按会话类型分发
// ---------------------------------------------------------------------------

pub fn input_capture() -> Box<dyn InputCapture> {
    match linux_wl::backend() {
        Backend::X11 => Box::new(linux_input::X11Capture::default()),
        Backend::Wlr => Box::new(linux_wlr_input::WlrCapture::default()),
        Backend::Portal => Box::new(linux_portal::PortalCapture::default()),
    }
}

pub fn input_injector() -> Result<Box<dyn InputInjector>, PlatformError> {
    Ok(match linux_wl::backend() {
        Backend::X11 => Box::new(linux_input::X11Injector::new()?),
        Backend::Wlr => Box::new(linux_wlr_input::WlrInjector::new()?),
        Backend::Portal => Box::new(linux_portal::PortalInjector::new()?),
    })
}

pub fn clipboard() -> Result<Box<dyn ClipboardAccess>, PlatformError> {
    Ok(match linux_wl::backend() {
        Backend::X11 => Box::new(linux_clipboard::X11Clipboard::new()?),
        Backend::Wlr => Box::new(linux_wl_clipboard::WlClipboard::new()?),
        // KDE 也有 data-control，直接用；GNOME 没有。
        Backend::Portal if linux_wl_clipboard::available() => {
            Box::new(linux_wl_clipboard::WlClipboard::new()?)
        }
        // GNOME：挂在远程桌面会话上的剪贴板 portal。
        Backend::Portal => Box::new(linux_wl_clipboard::WlClipboard::with_backend(
            linux_portal::selection()?,
        )),
    })
}

pub fn limits() -> crate::Limits {
    linux_wl::limits()
}

pub fn set_cursor_captured(captured: bool, restore_at: Option<(f64, f64)>) {
    match linux_wl::backend() {
        Backend::X11 => linux_input::set_cursor_captured(captured, restore_at),
        Backend::Wlr => linux_wlr_input::set_cursor_captured(captured, restore_at),
        Backend::Portal => linux_portal::set_cursor_captured(captured, restore_at),
    }
}

pub fn set_switch_edges(edges: &[(String, flow_core::geometry::Edge)]) {
    // 两种 Wayland 实现共用同一份设置；变了才通知。
    let changed = linux_wlr_input::set_switch_edges(edges);
    if changed && linux_wl::backend() == Backend::Portal {
        linux_portal::edges_changed();
    }
}

pub fn edge_glow(display_local_id: &str, edge: flow_core::geometry::Edge, along: f64) {
    match linux_wl::backend() {
        Backend::X11 => linux_glow::show(display_local_id, edge, along),
        // 合成器没有 layer-shell（GNOME）时它自己什么都不做。
        Backend::Wlr | Backend::Portal => linux_wl_glow::show(display_local_id, edge, along),
    }
}

pub(crate) fn os_err(what: &str, e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Os(format!("{what}：{e}"))
}

/// 连接 `DISPLAY` 指向的 X 服务，返回连接和默认屏幕的序号。
pub(crate) fn connect() -> Result<(RustConnection, usize), PlatformError> {
    x11rb::connect(None).map_err(|e| os_err("连不上 X11 显示服务（请在图形桌面里运行流界）", e))
}

/// 一块显示器的像素矩形与逻辑坐标矩形。键鼠捕获、注入和浮光用它在两种坐标之间换算。
#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    /// RandR 输出名（如 `eDP-1`、`HDMI-1`）。
    pub id: String,
    pub index: usize,
    /// 像素：left, top, right, bottom。
    pub phys: (i32, i32, i32, i32),
    pub scale: f64,
    pub logical: Rect,
    pub primary: bool,
    /// Wayland：这块输出的 `wl_output` 全局对象编号（对所有连接都一样）；X11 上是 0。
    pub wl_global: u32,
}

impl Monitor {
    pub fn logical_to_phys(&self, x: f64, y: f64) -> (i32, i32) {
        let px = self.phys.0 as f64 + (x - self.logical.x) * self.scale;
        let py = self.phys.1 as f64 + (y - self.logical.y) * self.scale;
        (
            (px.round() as i32).clamp(self.phys.0, self.phys.2 - 1),
            (py.round() as i32).clamp(self.phys.1, self.phys.3 - 1),
        )
    }
}

/// 像素矩形：left, top, right, bottom。
type PhysRect = (i32, i32, i32, i32);

static MONITORS: Mutex<Vec<Monitor>> = Mutex::new(Vec::new());

pub(crate) fn update_monitors(list: Vec<Monitor>) {
    *MONITORS.lock().unwrap_or_else(|e| e.into_inner()) = list;
}

/// 在显示器快照上执行查询；快照为空时先刷新。
pub(crate) fn with_monitors<R>(f: impl FnOnce(&[Monitor]) -> R) -> R {
    let mut guard = MONITORS.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_empty()
        && let Ok(list) = monitors()
    {
        *guard = list;
    }
    f(&guard)
}

/// 当前的整体缩放（像素 / 逻辑点）。
pub(crate) fn scale() -> f64 {
    with_monitors(|m| m.first().map_or(1.0, |m| m.scale))
}

/// 本机系统坐标（逻辑点）换成根窗口像素，限制在离它最近的显示器内。
pub(crate) fn logical_to_phys_nearest(x: f64, y: f64) -> Option<(i32, i32)> {
    with_monitors(|list| {
        list.iter()
            .min_by(|a, b| {
                let dist = |m: &Monitor| {
                    let r = m.logical;
                    let cx = x.clamp(r.x, r.x + r.width);
                    let cy = y.clamp(r.y, r.y + r.height);
                    (cx - x).abs() + (cy - y).abs()
                };
                dist(a).total_cmp(&dist(b))
            })
            .map(|m| m.logical_to_phys(x, y))
    })
}

/// 从 X 资源（`RESOURCE_MANAGER`）里读 `Xft.dpi`。
fn parse_xft_dpi(resources: &str) -> Option<f64> {
    resources
        .lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:")?.trim().parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
}

/// `Xft.dpi` 换成缩放。低于 96 的按 100% 算（和 Windows 一致），上限 400%。
fn scale_from_dpi(dpi: Option<f64>) -> f64 {
    dpi.map_or(1.0, |d| (d / 96.0).clamp(1.0, 4.0))
}

fn read_scale(conn: &RustConnection, root: Window) -> f64 {
    let resources = conn
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            1 << 20,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| String::from_utf8_lossy(&r.value).into_owned());
    scale_from_dpi(resources.as_deref().and_then(parse_xft_dpi))
}

/// X11 的显示器（X11 的捕获和注入用）。
pub(crate) fn x11_monitors() -> Result<Vec<Monitor>, PlatformError> {
    let (conn, screen) = connect()?;
    monitors_on(&conn, screen)
}

/// 当前会话的显示器。
pub(crate) fn monitors() -> Result<Vec<Monitor>, PlatformError> {
    match linux_wl::backend() {
        Backend::X11 => x11_monitors(),
        Backend::Wlr | Backend::Portal => linux_wl::monitors(),
    }
}

pub(crate) fn monitors_on(
    conn: &RustConnection,
    screen: usize,
) -> Result<Vec<Monitor>, PlatformError> {
    let root_screen = conn
        .setup()
        .roots
        .get(screen)
        .ok_or_else(|| PlatformError::Os("X11 没有可用的屏幕".into()))?;
    let root = root_screen.root;
    let scale = read_scale(conn, root);

    // (名称, 像素矩形, 是否主屏)
    let mut found: Vec<(String, PhysRect, bool)> = Vec::new();
    let reply = conn
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|c| c.reply().ok());
    for m in reply.map(|r| r.monitors).unwrap_or_default() {
        if m.width == 0 || m.height == 0 {
            continue;
        }
        let (x, y) = (i32::from(m.x), i32::from(m.y));
        let rect = (x, y, x + i32::from(m.width), y + i32::from(m.height));
        // 镜像显示的几块屏幕占同一个矩形，只算一块。
        if found.iter().any(|f| f.1 == rect) {
            continue;
        }
        let name = conn
            .get_atom_name(m.name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("monitor-{}", found.len()));
        found.push((name, rect, m.primary));
    }
    if found.is_empty() {
        // 没有 RandR 1.5（很老的 X 服务或某些远程桌面）：把整个根窗口当一块屏幕。
        let (w, h) = (
            i32::from(root_screen.width_in_pixels),
            i32::from(root_screen.height_in_pixels),
        );
        found.push(("screen".to_owned(), (0, 0, w, h), true));
    }
    if !found.iter().any(|f| f.2) {
        found[0].2 = true;
    }

    Ok(found
        .into_iter()
        .enumerate()
        .map(|(index, (id, phys, primary))| Monitor {
            id,
            index,
            phys,
            scale,
            logical: Rect::new(
                phys.0 as f64 / scale,
                phys.1 as f64 / scale,
                (phys.2 - phys.0) as f64 / scale,
                (phys.3 - phys.1) as f64 / scale,
            ),
            primary,
            wl_global: 0,
        })
        .collect())
}

pub struct LinuxDisplays;

impl DisplayProvider for LinuxDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        let list = monitors()?;
        update_monitors(list.clone());
        Ok(list
            .into_iter()
            .map(|m| DisplayInfo {
                name: format!("显示器 {}", m.index + 1),
                native: Some(((m.phys.2 - m.phys.0) as u32, (m.phys.3 - m.phys.1) as u32)),
                local_id: m.id,
                rect: m.logical,
                scale: m.scale,
                primary: m.primary,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_xft_dpi_from_resources() {
        let res = "Xcursor.size:\t24\nXft.dpi:\t192\nXft.antialias:\t1\n";
        assert_eq!(parse_xft_dpi(res), Some(192.0));
        assert_eq!(parse_xft_dpi("Xft.dpi: 120.5\n"), Some(120.5));
        assert_eq!(parse_xft_dpi("Xft.antialias: 1\n"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi: abc\n"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi: 0\n"), None);
    }

    #[test]
    fn scale_follows_dpi_within_limits() {
        assert_eq!(scale_from_dpi(None), 1.0);
        assert_eq!(scale_from_dpi(Some(96.0)), 1.0);
        assert_eq!(scale_from_dpi(Some(144.0)), 1.5);
        assert_eq!(scale_from_dpi(Some(192.0)), 2.0);
        // 小于 96 不缩小，离谱的大值封顶。
        assert_eq!(scale_from_dpi(Some(72.0)), 1.0);
        assert_eq!(scale_from_dpi(Some(9600.0)), 4.0);
    }

    #[test]
    fn logical_point_maps_into_monitor_pixels() {
        let m = Monitor {
            id: "HDMI-1".into(),
            index: 0,
            phys: (1920, 0, 5760, 2160),
            scale: 2.0,
            logical: Rect::new(960.0, 0.0, 1920.0, 1080.0),
            primary: false,
            wl_global: 0,
        };
        assert_eq!(m.logical_to_phys(960.0, 0.0), (1920, 0));
        assert_eq!(m.logical_to_phys(1060.0, 50.0), (2120, 100));
        // 超出的点收回显示器内。
        assert_eq!(m.logical_to_phys(99999.0, -5.0), (5759, 0));
    }
}
