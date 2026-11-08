//! 跨设备切换判定（纯状态机，时间由调用方传入，便于测试）。
//!
//! 调用方式：
//! - 每次光标移动后，把"光标是否贴在通往另一台设备的边上"（[`EdgeContact`]）交给 [`EdgeSwitcher::update`]；
//! - 如果返回 [`Decision::Waiting`]，在 `remaining_ms` 后调用 [`EdgeSwitcher::poll`]，
//!   这样用户贴边停住不动也能在停留时间到达后切换。

use serde::{Deserialize, Serialize};

use crate::geometry::{Edge, Point, Rect};
use crate::keys::Modifiers;
use crate::layout::{Crossing, DisplayId};

/// 防误触设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchPolicy {
    /// 必须按住这些修饰键才允许跨屏；为空表示不要求。
    pub require_modifiers: Modifiers,
    /// 贴边停留多少毫秒才切换；0 表示推出边缘立即切换。
    pub dwell_ms: u64,
    /// 屏幕角落多大范围内（逻辑点）不触发切换，避免误触系统触发角 / 开始菜单。
    pub corner_deadzone: f64,
    /// 锁定在当前设备：边缘切换失效（快捷键切换不受影响）。
    pub locked: bool,
}

impl Default for SwitchPolicy {
    fn default() -> Self {
        Self {
            require_modifiers: Modifiers::NONE,
            dwell_ms: 0,
            corner_deadzone: 10.0,
            locked: false,
        }
    }
}

/// 光标贴在某条边上，而边外是另一台设备的屏幕。
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeContact {
    pub from: DisplayId,
    pub from_rect: Rect,
    pub edge: Edge,
    /// 贴边位置（布局空间）。
    pub at: Point,
    /// 如果切换，会进入的屏幕和位置。
    pub crossing: Crossing,
    /// 这次移动是否在"往外推"（而不只是沿边滑动或停在边上）。
    pub pushing: bool,
}

/// 切换判定时需要的当前输入状态。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HeldInput {
    pub modifiers: Modifiers,
    /// 是否有鼠标按钮被按住。v1 不支持跨设备拖拽，按住时不切换。
    pub buttons: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Stay,
    /// 正在停留计时，还差多少毫秒。
    Waiting {
        remaining_ms: u64,
    },
    Switch(Crossing),
}

#[derive(Debug, Clone)]
struct Pending {
    from: DisplayId,
    edge: Edge,
    target: DisplayId,
    crossing: Crossing,
    since_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct EdgeSwitcher {
    pub policy: SwitchPolicy,
    pending: Option<Pending>,
}

impl EdgeSwitcher {
    pub fn new(policy: SwitchPolicy) -> Self {
        Self {
            policy,
            pending: None,
        }
    }

    pub fn reset(&mut self) {
        self.pending = None;
    }

    fn allowed(&self, held: HeldInput) -> bool {
        !self.policy.locked
            && !held.buttons
            && held.modifiers.contains(self.policy.require_modifiers)
    }

    fn in_corner(&self, c: &EdgeContact) -> bool {
        let dz = self.policy.corner_deadzone;
        if dz <= 0.0 {
            return false;
        }
        let along = c.edge.along(c.at);
        let (s0, s1) = c.from_rect.edge_span(c.edge);
        along - s0 < dz || s1 - along < dz
    }

    /// 光标移动后调用。`contact` 为 `None` 表示光标已离开边缘。
    pub fn update(
        &mut self,
        contact: Option<&EdgeContact>,
        held: HeldInput,
        now_ms: u64,
    ) -> Decision {
        let Some(c) = contact else {
            self.pending = None;
            return Decision::Stay;
        };
        if !self.allowed(held) || self.in_corner(c) {
            self.pending = None;
            return Decision::Stay;
        }

        let dwell = self.policy.dwell_ms;
        if dwell == 0 {
            return if c.pushing {
                Decision::Switch(c.crossing.clone())
            } else {
                Decision::Stay
            };
        }

        let same = self.pending.as_ref().is_some_and(|p| {
            p.from == c.from && p.edge == c.edge && p.target == c.crossing.display
        });
        if same {
            let p = self.pending.as_mut().expect("checked above");
            p.crossing = c.crossing.clone();
            return self.poll(held, now_ms);
        }
        // 只有"往外推"才开始计时；单纯停在边上（比如点滚动条）不算。
        if c.pushing {
            self.pending = Some(Pending {
                from: c.from.clone(),
                edge: c.edge,
                target: c.crossing.display.clone(),
                crossing: c.crossing.clone(),
                since_ms: now_ms,
            });
            Decision::Waiting {
                remaining_ms: dwell,
            }
        } else {
            self.pending = None;
            Decision::Stay
        }
    }

    /// 停留计时到期时调用（光标没有再移动也需要能切换）。
    pub fn poll(&mut self, held: HeldInput, now_ms: u64) -> Decision {
        let Some(p) = &self.pending else {
            return Decision::Stay;
        };
        if !self.allowed(held) {
            self.pending = None;
            return Decision::Stay;
        }
        let elapsed = now_ms.saturating_sub(p.since_ms);
        if elapsed >= self.policy.dwell_ms {
            let crossing = p.crossing.clone();
            self.pending = None;
            Decision::Switch(crossing)
        } else {
            Decision::Waiting {
                remaining_ms: self.policy.dwell_ms - elapsed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::DeviceId;

    fn did(dev: &str) -> DisplayId {
        DisplayId {
            device: DeviceId(dev.into()),
            local: "0".into(),
        }
    }

    fn contact(y: f64, pushing: bool) -> EdgeContact {
        EdgeContact {
            from: did("mac"),
            from_rect: Rect::new(0.0, 0.0, 1440.0, 900.0),
            edge: Edge::Right,
            at: Point::new(1440.0, y),
            crossing: Crossing {
                display: did("pc"),
                point: Point::new(1440.0, y),
            },
            pushing,
        }
    }

    const FREE: HeldInput = HeldInput {
        modifiers: Modifiers::NONE,
        buttons: false,
    };

    #[test]
    fn immediate_switch_requires_push() {
        let mut s = EdgeSwitcher::default();
        assert_eq!(
            s.update(Some(&contact(400.0, false)), FREE, 0),
            Decision::Stay
        );
        assert!(matches!(
            s.update(Some(&contact(400.0, true)), FREE, 0),
            Decision::Switch(_)
        ));
    }

    #[test]
    fn corner_deadzone_blocks() {
        let mut s = EdgeSwitcher::default();
        assert_eq!(s.update(Some(&contact(3.0, true)), FREE, 0), Decision::Stay);
        assert_eq!(
            s.update(Some(&contact(895.0, true)), FREE, 0),
            Decision::Stay
        );
    }

    #[test]
    fn required_modifier() {
        let mut s = EdgeSwitcher::new(SwitchPolicy {
            require_modifiers: Modifiers::CTRL,
            ..Default::default()
        });
        assert_eq!(
            s.update(Some(&contact(400.0, true)), FREE, 0),
            Decision::Stay
        );
        let ctrl = HeldInput {
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            buttons: false,
        };
        assert!(matches!(
            s.update(Some(&contact(400.0, true)), ctrl, 0),
            Decision::Switch(_)
        ));
    }

    #[test]
    fn locked_or_dragging_blocks() {
        let mut s = EdgeSwitcher::new(SwitchPolicy {
            locked: true,
            ..Default::default()
        });
        assert_eq!(
            s.update(Some(&contact(400.0, true)), FREE, 0),
            Decision::Stay
        );
        let mut s = EdgeSwitcher::default();
        let drag = HeldInput {
            modifiers: Modifiers::NONE,
            buttons: true,
        };
        assert_eq!(
            s.update(Some(&contact(400.0, true)), drag, 0),
            Decision::Stay
        );
    }

    #[test]
    fn dwell_waits_then_switches_via_poll() {
        let mut s = EdgeSwitcher::new(SwitchPolicy {
            dwell_ms: 300,
            ..Default::default()
        });
        assert_eq!(
            s.update(Some(&contact(400.0, true)), FREE, 1000),
            Decision::Waiting { remaining_ms: 300 }
        );
        // 沿边滑动，计时继续
        assert_eq!(
            s.update(Some(&contact(420.0, false)), FREE, 1100),
            Decision::Waiting { remaining_ms: 200 }
        );
        // 不再移动，定时器到期
        match s.poll(FREE, 1300) {
            Decision::Switch(c) => assert_eq!(c.point.y, 420.0),
            other => panic!("expected switch, got {other:?}"),
        }
        assert_eq!(s.poll(FREE, 1400), Decision::Stay);
    }

    #[test]
    fn dwell_resets_when_leaving_edge() {
        let mut s = EdgeSwitcher::new(SwitchPolicy {
            dwell_ms: 300,
            ..Default::default()
        });
        s.update(Some(&contact(400.0, true)), FREE, 0);
        assert_eq!(s.update(None, FREE, 100), Decision::Stay);
        assert_eq!(s.poll(FREE, 1000), Decision::Stay);
        // 只停在边上、不往外推，不开始计时
        assert_eq!(
            s.update(Some(&contact(400.0, false)), FREE, 1000),
            Decision::Stay
        );
    }
}
