//! Linux 的屏幕电源：点亮屏幕（X11 的 DPMS）、阻止屏幕自动熄灭（桌面环境的屏幕保护服务）、
//! 查询屏幕是否熄着。
//!
//! Wayland 没有通用的"点亮屏幕"接口。Sway、Hyprland 用合成器的屏幕电源协议（`linux_wl_power.rs`）；
//! GNOME、KDE 上 [`wake_display`] 返回"不支持"，调用方改用注入一次鼠标移动来代替。

use std::sync::Mutex;
use std::time::Duration;

use ashpd::zbus;
use x11rb::protocol::dpms::{ConnectionExt as _, DPMSMode};
use x11rb::protocol::xproto::{ConnectionExt as _, ScreenSaver};

use crate::linux::{self, os_err};
use crate::linux_wl::{self, Backend};
use crate::{PlatformError, linux_portal, linux_wl_power};

/// 正在阻止熄屏时，结束它用的发送端：丢掉它，持有 D-Bus 连接的线程就退出，系统随之解除阻止。
static KEEP_AWAKE: Mutex<Option<tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);

pub fn wake_display() -> Result<(), PlatformError> {
    match linux_wl::backend() {
        Backend::X11 => {}
        Backend::Wlr => return linux_wl_power::wake_display(),
        Backend::Portal => return Err(PlatformError::Unsupported("点亮屏幕")),
    }
    let (conn, _) = linux::connect()?;
    // 重置屏幕保护（等同 `xset s reset`），再把显示器电源强制打开（等同 `xset dpms force on`）。
    // X 服务没有 DPMS 或没启用时第二步会被拒绝，错误是异步的，不用管。
    conn.force_screen_saver(ScreenSaver::RESET)
        .map_err(|e| os_err("重置屏幕保护失败", e))?;
    let _ = conn.dpms_force_level(DPMSMode::ON);
    // 等 X 服务处理完再断开，否则请求可能还在缓冲区里。
    conn.get_input_focus()
        .map_err(|e| os_err("点亮屏幕失败", e))?
        .reply()
        .map_err(|e| os_err("点亮屏幕失败", e))?;
    Ok(())
}

/// X11：显示器电源管理（DPMS）开着、且当前不在"打开"状态。Wayland 上查不到，返回 `false`。
pub fn displays_asleep() -> bool {
    if linux_wl::backend() != Backend::X11 {
        return false;
    }
    let Ok((conn, _)) = linux::connect() else {
        return false;
    };
    conn.dpms_info()
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|i| i.state && i.power_level != DPMSMode::ON)
}

/// 开启或关闭"不让屏幕自动熄灭"：向桌面环境的屏幕保护服务（`org.freedesktop.ScreenSaver`，
/// GNOME、KDE、Xfce、Cinnamon、hypridle 等都提供）登记。连接断开时登记自动失效。
pub fn set_keep_awake(on: bool) -> Result<(), PlatformError> {
    let mut held = KEEP_AWAKE.lock().expect("keep awake");
    if !on {
        held.take();
        return Ok(());
    }
    if held.as_ref().is_some_and(|tx| !tx.is_closed()) {
        return Ok(());
    }
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("flow-keep-awake".into())
        .spawn(move || {
            let result = linux_portal::runtime().and_then(|rt| {
                rt.block_on(async {
                    let conn = inhibit().await?;
                    let _ = ready_tx.send(Ok(()));
                    // 一直持有连接，直到被要求结束。
                    let _ = stop_rx.await;
                    drop(conn);
                    Ok(())
                })
            });
            if let Err(e) = result {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| os_err("无法启动线程", e))?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {
            *held = Some(stop_tx);
            Ok(())
        }
        Ok(Err(e)) => Err(PlatformError::Os(format!(
            "当前桌面没有接受保持屏幕常亮的请求（{e}）"
        ))),
        Err(_) => Err(PlatformError::Os(
            "桌面的屏幕保护服务没有回应，没能保持屏幕常亮".into(),
        )),
    }
}

const SCREEN_SAVER: &str = "org.freedesktop.ScreenSaver";

/// 这个桌面做不到"保持屏幕常亮"的原因；做得到时返回 `None`。只看会话总线上有没有屏幕保护
/// 服务（正在运行，或者可以按需启动），不真的去登记。
pub fn keep_awake_problem() -> Option<String> {
    let probe = async {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| format!("连不上桌面的会话总线（{e}），没法阻止屏幕熄灭。"))?;
        let bus = zbus::fdo::DBusProxy::new(&conn)
            .await
            .map_err(|e| format!("会话总线不可用（{e}），没法阻止屏幕熄灭。"))?;
        let running = match zbus::names::BusName::try_from(SCREEN_SAVER) {
            Ok(name) => bus.name_has_owner(name).await.unwrap_or(false),
            Err(_) => false,
        };
        let available = running
            || bus
                .list_activatable_names()
                .await
                .is_ok_and(|names| names.iter().any(|n| n.as_str() == SCREEN_SAVER));
        if available {
            Ok(())
        } else {
            Err(
                "这个桌面没有屏幕保护服务，流界没法阻止屏幕熄灭。请在桌面自己的设置（或 swayidle 等空闲管理程序的配置）里调整熄屏时间。"
                    .to_owned(),
            )
        }
    };
    let rt = match linux_portal::runtime() {
        Ok(rt) => rt,
        Err(e) => return Some(format!("无法查询桌面的屏幕保护服务（{e}）。")),
    };
    rt.block_on(async {
        match tokio::time::timeout(Duration::from_secs(3), probe).await {
            Ok(result) => result.err(),
            Err(_) => Some("桌面的会话总线没有回应，没法阻止屏幕熄灭。".to_owned()),
        }
    })
}

/// 现在是不是停在锁屏界面：读 logind 里本会话的 `LockedHint`。GNOME、KDE 锁屏时会设置；
/// Sway、Hyprland 上要看锁屏程序是不是通过 logind 上锁的（`loginctl lock-session`），没设置时
/// 一直是"没锁"。没有 logind、或两秒内没有回应时返回 `None`。
pub fn screen_locked() -> Option<bool> {
    let rt = linux_portal::runtime().ok()?;
    rt.block_on(async {
        let query = async {
            let conn = zbus::Connection::system().await.ok()?;
            // 直接调一次 `Properties.Get`，不建带属性缓存的代理（那要多订阅一次变化信号，
            // 而这条连接查完就丢）。`session/auto`：调用方所在的会话；流界由用户服务管理器
            // 启动时，是该用户的图形会话。
            let reply = conn
                .call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1/session/auto",
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &("org.freedesktop.login1.Session", "LockedHint"),
                )
                .await
                .ok()?;
            let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
            bool::try_from(value).ok()
        };
        tokio::time::timeout(Duration::from_secs(2), query)
            .await
            .ok()
            .flatten()
    })
}

async fn inhibit() -> Result<zbus::Connection, String> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| format!("连不上会话总线：{e}"))?;
    conn.call_method(
        Some(SCREEN_SAVER),
        "/org/freedesktop/ScreenSaver",
        Some(SCREEN_SAVER),
        "Inhibit",
        &("流界", "连着其他电脑时保持屏幕常亮"),
    )
    .await
    .map_err(|e| format!("org.freedesktop.ScreenSaver 不可用：{e}"))?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 查询锁屏状态不会卡住、不会出错。不断言结果：没有 logind（容器、CI）时查不到，
    /// 有的话跑测试的时候屏幕也可能正锁着。
    #[test]
    fn lock_state_query_returns_promptly() {
        let started = std::time::Instant::now();
        let locked = screen_locked();
        eprintln!("screen_locked() = {locked:?}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    /// Xvfb 里屏幕永远亮着；点亮屏幕的请求应当被接受。
    #[test]
    #[ignore = "需要 X 服务"]
    fn x11_wake_is_accepted() {
        if std::env::var_os("DISPLAY").is_none() {
            return;
        }
        wake_display().unwrap();
        assert!(!displays_asleep());
    }

    /// 没有屏幕保护服务时要报错，而不是卡住或假装成功；关闭随时可以调用。
    #[test]
    #[ignore = "需要 X 服务"]
    fn keep_awake_reports_a_missing_service() {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            return;
        }
        assert!(keep_awake_problem().is_some());
        assert!(set_keep_awake(true).is_err());
        set_keep_awake(false).unwrap();
    }
}
