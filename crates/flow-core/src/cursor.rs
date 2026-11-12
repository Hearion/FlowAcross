//! 控制远端设备时，源设备在布局空间里维护的虚拟光标。
//!
//! 源设备只捕获相对位移，自己计算光标在目标屏幕上的绝对位置再发过去，
//! 这样目标系统的鼠标加速不会叠加第二次，丢包也不会导致位置漂移。

use crate::geometry::{EDGE_EPSILON, Edge, Point};
use crate::layout::{DisplayId, EdgeMapping, Layout};
use crate::switching::EdgeContact;

#[derive(Debug, Clone, PartialEq)]
pub struct VirtualCursor {
    pub display: DisplayId,
    /// 布局空间坐标。
    pub pos: Point,
}

impl VirtualCursor {
    pub fn new(display: DisplayId, pos: Point) -> Self {
        Self { display, pos }
    }

    /// 按逻辑点位移移动光标。
    ///
    /// - 在同一台设备的多块屏幕之间直接穿过（和系统原生行为一致）；
    /// - 碰到通往其他设备的边时停在边上，并返回 [`EdgeContact`] 交给切换判定；
    /// - 当前屏幕已不在布局中时返回 `Err(())`，调用方应结束远程控制。
    #[allow(clippy::result_unit_err)]
    pub fn apply_delta(
        &mut self,
        layout: &Layout,
        dx: f64,
        dy: f64,
        mapping: EdgeMapping,
    ) -> Result<Option<EdgeContact>, ()> {
        let r = layout.display(&self.display).ok_or(())?.rect;
        let target = Point::new(self.pos.x + dx, self.pos.y + dy);
        if r.contains(target) {
            self.pos = target;
            return Ok(self.contact(layout, mapping, None));
        }

        // 找出位移线段最先穿过的那条边。
        let mut first: Option<(f64, Edge)> = None;
        let mut consider = |t: f64, e: Edge| {
            if t.is_finite() && t >= 0.0 && first.is_none_or(|(bt, _)| t < bt) {
                first = Some((t, e));
            }
        };
        if target.x >= r.right() {
            consider((r.right() - self.pos.x) / dx, Edge::Right);
        }
        if target.x < r.left() {
            consider((r.left() - self.pos.x) / dx, Edge::Left);
        }
        if target.y >= r.bottom() {
            consider((r.bottom() - self.pos.y) / dy, Edge::Bottom);
        }
        if target.y < r.top() {
            consider((r.top() - self.pos.y) / dy, Edge::Top);
        }
        let Some((t, edge)) = first else {
            self.pos = r.clamp(target);
            return Ok(None);
        };

        let exit = r.clamp(Point::new(self.pos.x + dx * t, self.pos.y + dy * t));
        match layout.cross_edge(&self.display, edge, exit, mapping) {
            Some(c) if c.display.device == self.display.device => {
                self.display = c.display;
                self.pos = c.point;
                Ok(self.contact(layout, mapping, None))
            }
            _ => {
                self.pos = r.clamp(target);
                Ok(self.contact(layout, mapping, Some(edge)))
            }
        }
    }

    /// 光标当前是否贴在通往其他设备的边上。`pushed` 是这次移动往外推的那条边。
    fn contact(
        &self,
        layout: &Layout,
        mapping: EdgeMapping,
        pushed: Option<Edge>,
    ) -> Option<EdgeContact> {
        let r = layout.display(&self.display)?.rect;
        Edge::ALL.into_iter().find_map(|edge| {
            let dist = (r.edge_coord(edge)
                - if edge.is_vertical() {
                    self.pos.x
                } else {
                    self.pos.y
                })
            .abs();
            if dist > 2.0 * EDGE_EPSILON {
                return None;
            }
            let crossing = layout.cross_edge(&self.display, edge, self.pos, mapping)?;
            (crossing.display.device != self.display.device).then(|| EdgeContact {
                from: self.display.clone(),
                from_rect: r,
                edge,
                at: self.pos,
                crossing,
                pushing: pushed == Some(edge),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Rect, assert_near};
    use crate::layout::tests::{display, id, mac_and_pc};

    #[test]
    fn moves_inside_display() {
        let l = mac_and_pc();
        let mut c = VirtualCursor::new(id("pc", "0"), Point::new(2000.0, 500.0));
        assert_eq!(
            c.apply_delta(&l, 10.0, -5.0, EdgeMapping::Continuous),
            Ok(None)
        );
        assert_eq!(c.pos, Point::new(2010.0, 495.0));
    }

    #[test]
    fn pushing_into_other_device_reports_contact() {
        let l = mac_and_pc();
        let mut c = VirtualCursor::new(id("pc", "0"), Point::new(1450.0, 300.0));
        let contact = c
            .apply_delta(&l, -30.0, 4.0, EdgeMapping::Continuous)
            .unwrap()
            .unwrap();
        assert_eq!(contact.edge, Edge::Left);
        assert!(contact.pushing);
        assert_eq!(contact.crossing.display, id("mac", "0"));
        // 光标停在 PC 左边缘，沿边分量照常移动
        assert_near(c.pos, Point::new(1440.0, 304.0));

        // 继续沿边上下滑动：仍贴边，但不算往外推
        let contact = c
            .apply_delta(&l, 0.0, 10.0, EdgeMapping::Continuous)
            .unwrap()
            .unwrap();
        assert!(!contact.pushing);
    }

    #[test]
    fn edge_without_neighbor_just_blocks() {
        let l = mac_and_pc();
        let mut c = VirtualCursor::new(id("pc", "0"), Point::new(3990.0, 300.0));
        assert_eq!(
            c.apply_delta(&l, 50.0, 0.0, EdgeMapping::Continuous),
            Ok(None)
        );
        assert!(l.display(&id("pc", "0")).unwrap().rect.contains(c.pos));
    }

    #[test]
    fn crosses_freely_between_displays_of_same_device() {
        let l = Layout::new(vec![
            display("pc", "0", Rect::new(0.0, 0.0, 1920.0, 1080.0), 1.0),
            display("pc", "1", Rect::new(1920.0, 0.0, 1920.0, 1080.0), 1.0),
        ]);
        let mut c = VirtualCursor::new(id("pc", "0"), Point::new(1910.0, 500.0));
        assert_eq!(
            c.apply_delta(&l, 20.0, 0.0, EdgeMapping::Continuous),
            Ok(None)
        );
        assert_eq!(c.display, id("pc", "1"));
        assert_eq!(c.pos, Point::new(1920.0, 500.0));
    }

    #[test]
    fn missing_display_is_error() {
        let l = mac_and_pc();
        let mut c = VirtualCursor::new(id("gone", "0"), Point::new(0.0, 0.0));
        assert_eq!(
            c.apply_delta(&l, 1.0, 1.0, EdgeMapping::Continuous),
            Err(())
        );
    }
}
