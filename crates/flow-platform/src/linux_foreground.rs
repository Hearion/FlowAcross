//! Linux（X11）：现在在前台的应用，以及它的窗口是不是全屏（按应用自动锁定用）。
//!
//! 窗口管理器按 EWMH 约定把活动窗口写在根窗口的 `_NET_ACTIVE_WINDOW` 上，全屏状态写在窗口的
//! `_NET_WM_STATE` 里。应用名取 `WM_CLASS` 的类名（第二段，如 `steam`、`firefox`）。
//! Wayland 不让普通程序看别的窗口，查不了（`Limits::foreground_app`）。

use flow_core::autolock::Foreground;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

use crate::linux;
use crate::linux_wl::{self, Backend};

fn atom(conn: &RustConnection, name: &[u8]) -> Option<Atom> {
    Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
}

/// 读一个由 32 位数组成的属性。
fn property_u32(conn: &RustConnection, window: Window, property: Atom) -> Option<Vec<u32>> {
    let reply = conn
        .get_property(false, window, property, AtomEnum::ANY, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    Some(reply.value32()?.collect())
}

/// `WM_CLASS` 的内容是两段以 0 结尾的字符串：实例名、类名。取类名，没有就取实例名。
fn class_from_wm_class(value: &[u8]) -> String {
    let mut parts = value.split(|&b| b == 0).filter(|p| !p.is_empty());
    let instance = parts.next();
    let class = parts.next().or(instance).unwrap_or_default();
    String::from_utf8_lossy(class).into_owned()
}

/// 现在在前台的应用；没有活动窗口、不是 X11 会话时返回 `None`。前台是流界自己时应用名为空。
pub fn foreground() -> Option<Foreground> {
    if linux_wl::backend() != Backend::X11 {
        return None;
    }
    let (conn, screen) = linux::connect().ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let active = atom(&conn, b"_NET_ACTIVE_WINDOW")?;
    let window = *property_u32(&conn, root, active)?.first()?;
    if window == 0 {
        return None;
    }
    let own = atom(&conn, b"_NET_WM_PID")
        .and_then(|a| property_u32(&conn, window, a))
        .and_then(|v| v.first().copied())
        == Some(std::process::id());
    if own {
        return Some(Foreground {
            app: String::new(),
            fullscreen: false,
        });
    }
    let fullscreen = atom(&conn, b"_NET_WM_STATE")
        .zip(atom(&conn, b"_NET_WM_STATE_FULLSCREEN"))
        .is_some_and(|(state, full)| {
            property_u32(&conn, window, state).is_some_and(|v| v.contains(&full))
        });
    let app = conn
        .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| class_from_wm_class(&r.value))
        .unwrap_or_default();
    Some(Foreground { app, fullscreen })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_name_is_the_second_part_of_wm_class() {
        assert_eq!(class_from_wm_class(b"Navigator\0firefox\0"), "firefox");
        assert_eq!(class_from_wm_class(b"steam\0Steam\0"), "Steam");
        // 只有一段、没有内容。
        assert_eq!(class_from_wm_class(b"xterm\0"), "xterm");
        assert_eq!(class_from_wm_class(b""), "");
    }
}
