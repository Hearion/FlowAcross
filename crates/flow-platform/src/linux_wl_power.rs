//! Wayland（wlroots 系：Sway、Hyprland）：用 wlr-output-power-management 协议直接把屏幕打开。
//!
//! 屏幕是被空闲管理程序（swayidle、hypridle）通过合成器关掉的，这个协议改的是同一个开关，
//! 不需要假装有键鼠操作。GNOME、KDE 没有这个协议，由调用方改用移动鼠标的办法。

use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1;
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_v1::{
    self, Mode, ZwlrOutputPowerV1,
};

use crate::PlatformError;
use crate::linux::os_err;
use crate::linux_wl::{self, globals_of};

/// 每块屏幕最近一次报告的电源状态（按绑定顺序）；`None` 表示合成器不让控制这块屏幕。
#[derive(Default)]
struct State {
    on: Vec<Option<bool>>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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

impl Dispatch<ZwlrOutputPowerV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(slot) = state.on.get_mut(*index) else {
            return;
        };
        match event {
            zwlr_output_power_v1::Event::Mode { mode } => {
                *slot = Some(mode == WEnum::Value(Mode::On));
            }
            zwlr_output_power_v1::Event::Failed => *slot = None,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ZwlrOutputPowerManagerV1);

/// 把所有屏幕的电源设为 `set`（`None` 只查询），返回每块屏幕之后的状态。
/// 合成器没有这个协议、或者一块屏幕都没有时返回"不支持"。
fn output_power(set: Option<bool>) -> Result<Vec<Option<bool>>, PlatformError> {
    let conn = linux_wl::connect()?;
    let (globals, mut queue): (GlobalList, EventQueue<State>) =
        registry_queue_init(&conn).map_err(|e| os_err("读取 Wayland 全局对象失败", e))?;
    let qh = queue.handle();
    let Ok(manager) = globals.bind::<ZwlrOutputPowerManagerV1, _, _>(&qh, 1..=1, ()) else {
        return Err(PlatformError::Unsupported("点亮屏幕"));
    };
    let outputs = globals_of(&globals, "wl_output");
    if outputs.is_empty() {
        return Err(PlatformError::Unsupported("点亮屏幕"));
    }
    let mut state = State {
        on: vec![None; outputs.len()],
    };
    for (index, (global, version)) in outputs.into_iter().enumerate() {
        let output: wl_output::WlOutput = globals.registry().bind(global, version.min(4), &qh, ());
        let power = manager.get_output_power(&output, &qh, index);
        if let Some(on) = set {
            power.set_mode(if on { Mode::On } else { Mode::Off });
        }
    }
    // 合成器改完之后才报回新状态，可能晚于第一次往返：最多等 1 秒，等到每块屏幕都是想要的状态
    // （或被拒绝）为止。只查询时往返一次就够。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        queue
            .roundtrip(&mut state)
            .map_err(|e| os_err("和 Wayland 合成器通信失败", e))?;
        let settled = match set {
            Some(want) => state.on.iter().all(|m| m.is_none_or(|on| on == want)),
            None => true,
        };
        if settled || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(state.on)
}

/// 把所有屏幕打开。合成器不让控制任何一块屏幕时返回"不支持"。
pub fn wake_display() -> Result<(), PlatformError> {
    let after = output_power(Some(true))?;
    if after.iter().all(Option::is_none) {
        return Err(PlatformError::Unsupported("点亮屏幕"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 关掉屏幕后能重新打开（无头 sway）。
    ///
    /// sway 1.7 的无头后端关不掉虚拟屏幕（它自己的 `swaymsg "output * dpms off"` 也报
    /// "Failed to commit output"），这时只能验证点亮的请求被接受、屏幕是开着的；
    /// sway 1.10 上能完整走一遍"关 → 开"。
    #[test]
    #[ignore = "需要 wlroots 系的 Wayland 合成器"]
    fn linux_wl_outputs_are_powered_back_on() {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return;
        }
        let off = output_power(Some(false)).unwrap();
        assert!(!off.is_empty());
        if off.iter().any(|m| *m != Some(false)) {
            eprintln!("合成器没有关掉屏幕（{off:?}），只验证点亮");
        }
        wake_display().unwrap();
        let on = output_power(None).unwrap();
        assert!(on.iter().all(|m| *m == Some(true)), "{on:?}");
    }
}
