//! 统一布局空间：所有设备的所有显示器摆在同一个坐标系里（单位：逻辑点）。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::geometry::{EDGE_EPSILON, Edge, Point, Rect, overlap};

/// 设备 ID：设备证书公钥指纹的十六进制串，见 [`crate::pairing`]。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl DeviceId {
    /// 是否是合法的设备 ID：64 位小写十六进制（证书 SHA-256 指纹）。
    /// 来自网络（mDNS、对端消息）的 ID 必须先校验，攻击者可以填任意字符串。
    pub fn is_well_formed(&self) -> bool {
        self.0.len() == 64
            && self
                .0
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    /// 日志里用的简短形式：前 8 个字符。按字符截断，任意字符串都不会 panic。
    pub fn short(&self) -> &str {
        let end = self
            .0
            .char_indices()
            .nth(8)
            .map_or(self.0.len(), |(i, _)| i);
        &self.0[..end]
    }
}

/// 显示器 ID：所属设备 + 该设备系统给出的稳定显示器标识。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DisplayId {
    pub device: DeviceId,
    pub local: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub id: DisplayId,
    pub name: String,
    /// 在布局空间中的位置与逻辑尺寸。
    pub rect: Rect,
    /// 缩放系数：物理像素 = 逻辑点 × scale。
    pub scale: f64,
    /// 屏幕物理分辨率（仅用于界面显示），见 [`crate::protocol::DisplayInfo::native`]。
    #[serde(default)]
    pub native: Option<(u32, u32)>,
}

impl Display {
    pub fn device(&self) -> &DeviceId {
        &self.id.device
    }

    /// 布局空间坐标 → 该显示器内的逻辑坐标（以左上角为原点）。
    pub fn to_local(&self, p: Point) -> Point {
        Point::new(p.x - self.rect.x, p.y - self.rect.y)
    }

    /// 显示器内的逻辑坐标 → 布局空间坐标。
    pub fn from_local(&self, p: Point) -> Point {
        Point::new(p.x + self.rect.x, p.y + self.rect.y)
    }
}

/// 光标跨边时沿边方向的坐标如何换算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EdgeMapping {
    /// 沿边坐标保持不变，只能从两块屏幕真正重叠的那段边穿过（同一台电脑多显示器的行为）。
    #[default]
    Continuous,
    /// 按边长比例换算，整条边都可以穿过。仅当这条边只有一个相邻屏幕时生效，否则退回连续映射。
    Proportional,
}

/// 一次跨边的结果：进入哪块屏幕、落在布局空间的哪个点。
#[derive(Debug, Clone, PartialEq)]
pub struct Crossing {
    pub display: DisplayId,
    pub point: Point,
}

/// 两条边之间的距离不超过这个值就视为"贴在一起"。
pub const ADJACENCY_TOLERANCE: f64 = 1.0;

#[derive(Debug, Error, PartialEq)]
pub enum LayoutError {
    #[error("显示器 {0:?} 重复")]
    DuplicateDisplay(DisplayId),
    #[error("显示器 {0:?} 的尺寸或缩放无效")]
    InvalidDisplay(DisplayId),
    #[error("显示器 {0:?} 和 {1:?} 重叠")]
    Overlap(DisplayId, DisplayId),
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Layout {
    pub displays: Vec<Display>,
}

impl Layout {
    pub fn new(displays: Vec<Display>) -> Self {
        Self { displays }
    }

    pub fn display(&self, id: &DisplayId) -> Option<&Display> {
        self.displays.iter().find(|d| &d.id == id)
    }

    pub fn display_at(&self, p: Point) -> Option<&Display> {
        self.displays.iter().find(|d| d.rect.contains(p))
    }

    pub fn displays_of<'a>(&'a self, device: &'a DeviceId) -> impl Iterator<Item = &'a Display> {
        self.displays.iter().filter(move |d| d.device() == device)
    }

    /// 布局中出现的设备，按首次出现顺序。
    pub fn devices(&self) -> Vec<DeviceId> {
        let mut out: Vec<DeviceId> = Vec::new();
        for d in &self.displays {
            if !out.contains(d.device()) {
                out.push(d.device().clone());
            }
        }
        out
    }

    /// 贴在 `from` 的 `edge` 这条边外侧、且沿边方向有重叠的显示器。
    pub fn neighbors(&self, from: &DisplayId, edge: Edge) -> Vec<&Display> {
        let Some(src) = self.display(from) else {
            return Vec::new();
        };
        let src_coord = src.rect.edge_coord(edge);
        let (s0, s1) = src.rect.edge_span(edge);
        self.displays
            .iter()
            .filter(|d| d.id != src.id)
            .filter(|d| {
                (d.rect.edge_coord(edge.opposite()) - src_coord).abs() <= ADJACENCY_TOLERANCE
            })
            .filter(|d| {
                let (n0, n1) = d.rect.edge_span(edge);
                overlap(s0, s1, n0, n1) > 0.0
            })
            .collect()
    }

    /// 光标在 `from` 的 `edge` 边上的 `at` 点继续向外移动时，会进入哪块屏幕的哪个点。
    /// 返回 `None` 表示这里没有可进入的屏幕（光标停在边上）。
    pub fn cross_edge(
        &self,
        from: &DisplayId,
        edge: Edge,
        at: Point,
        mapping: EdgeMapping,
    ) -> Option<Crossing> {
        let src = self.display(from)?;
        let neighbors = self.neighbors(from, edge);
        let along = edge.along(at);
        let hit = || {
            neighbors.iter().copied().find(|n| {
                let (n0, n1) = n.rect.edge_span(edge);
                along >= n0 && along < n1
            })
        };

        let (target, target_along) = match (mapping, neighbors.as_slice()) {
            (EdgeMapping::Proportional, [only]) => {
                let (f0, f1) = src.rect.edge_span(edge);
                let t = ((along - f0) / (f1 - f0)).clamp(0.0, 1.0);
                let (n0, n1) = only.rect.edge_span(edge);
                (*only, n0 + t * (n1 - n0))
            }
            _ => (hit()?, along),
        };

        // 进入点落在目标屏幕靠近来源一侧的边上。
        let r = target.rect;
        let entry = match edge {
            Edge::Right => r.left(),
            Edge::Left => r.right() - EDGE_EPSILON,
            Edge::Bottom => r.top(),
            Edge::Top => r.bottom() - EDGE_EPSILON,
        };
        let p = if edge.is_vertical() {
            Point::new(entry, target_along)
        } else {
            Point::new(target_along, entry)
        };
        Some(Crossing {
            display: target.id.clone(),
            point: r.clamp(p),
        })
    }

    /// 把某台设备的所有显示器整体平移。
    pub fn move_device(&mut self, device: &DeviceId, dx: f64, dy: f64) {
        for d in self.displays.iter_mut().filter(|d| d.device() == device) {
            d.rect = d.rect.translated(dx, dy);
        }
    }

    /// 用户在布局图里把设备拖动 `(dx, dy)` 时，吸附到附近其他设备的边缘后的实际位移。
    /// 只在 `threshold` 范围内吸附；吸附包括"边贴边"和"边对齐"。
    pub fn snap_device_offset(
        &self,
        device: &DeviceId,
        dx: f64,
        dy: f64,
        threshold: f64,
    ) -> (f64, f64) {
        let moving: Vec<Rect> = self
            .displays_of(device)
            .map(|d| d.rect.translated(dx, dy))
            .collect();
        let others: Vec<Rect> = self
            .displays
            .iter()
            .filter(|d| d.device() != device)
            .map(|d| d.rect)
            .collect();

        let near = |a0: f64, a1: f64, b0: f64, b1: f64| {
            overlap(a0 - threshold, a1 + threshold, b0, b1) > 0.0
        };
        let mut best_x: Option<f64> = None;
        let mut best_y: Option<f64> = None;
        let consider = |best: &mut Option<f64>, diff: f64| {
            if diff.abs() <= threshold && best.is_none_or(|b| diff.abs() < b.abs()) {
                *best = Some(diff);
            }
        };

        for m in &moving {
            for o in &others {
                if near(m.top(), m.bottom(), o.top(), o.bottom()) {
                    // 左右贴边
                    consider(&mut best_x, o.left() - m.right());
                    consider(&mut best_x, o.right() - m.left());
                }
                if near(m.left(), m.right(), o.left(), o.right()) {
                    // 上下贴边
                    consider(&mut best_y, o.top() - m.bottom());
                    consider(&mut best_y, o.bottom() - m.top());
                }
                let touching_x = (m.right() - o.left()).abs() <= threshold
                    || (m.left() - o.right()).abs() <= threshold;
                let touching_y = (m.bottom() - o.top()).abs() <= threshold
                    || (m.top() - o.bottom()).abs() <= threshold;
                if touching_x {
                    // 左右相邻时，顶边或底边对齐
                    consider(&mut best_y, o.top() - m.top());
                    consider(&mut best_y, o.bottom() - m.bottom());
                }
                if touching_y {
                    // 上下相邻时，左边或右边对齐
                    consider(&mut best_x, o.left() - m.left());
                    consider(&mut best_x, o.right() - m.right());
                }
            }
        }
        (dx + best_x.unwrap_or(0.0), dy + best_y.unwrap_or(0.0))
    }

    /// 设备拖动 `(dx, dy)` 后是否会和其他设备的屏幕重叠。
    fn overlaps_after(&self, device: &DeviceId, dx: f64, dy: f64) -> bool {
        self.displays_of(device).any(|m| {
            let m = m.rect.translated(dx, dy);
            self.displays
                .iter()
                .filter(|o| o.device() != device)
                .any(|o| m.intersects(&o.rect))
        })
    }

    /// 用户拖动设备后的最终位移：先吸附，仍和其他屏幕重叠时推到离落点最近的贴边位置。
    /// 找不到不重叠的位置时返回 `None`。
    pub fn place_device(
        &self,
        device: &DeviceId,
        dx: f64,
        dy: f64,
        threshold: f64,
    ) -> Option<(f64, f64)> {
        let (dx, dy) = self.snap_device_offset(device, dx, dy, threshold);
        if !self.overlaps_after(device, dx, dy) {
            return Some((dx, dy));
        }
        // 对每一对重叠的屏幕，分别尝试往左、右、上、下推到刚好贴边。
        let mut candidates = Vec::new();
        for m in self.displays_of(device) {
            let m = m.rect.translated(dx, dy);
            for o in self.displays.iter().filter(|o| o.device() != device) {
                let o = o.rect;
                if !m.intersects(&o) {
                    continue;
                }
                candidates.push((dx + o.left() - m.right(), dy));
                candidates.push((dx + o.right() - m.left(), dy));
                candidates.push((dx, dy + o.top() - m.bottom()));
                candidates.push((dx, dy + o.bottom() - m.top()));
            }
        }
        let dist = |c: &(f64, f64)| (c.0 - dx).abs() + (c.1 - dy).abs();
        candidates.sort_by(|a, b| dist(a).total_cmp(&dist(b)));
        candidates
            .into_iter()
            .find(|&(x, y)| !self.overlaps_after(device, x, y))
            .map(|(x, y)| self.snap_device_offset(device, x, y, threshold))
            .filter(|&(x, y)| !self.overlaps_after(device, x, y))
    }

    /// 校验布局：ID 不重复、尺寸和缩放有效、任意两块屏幕不重叠。
    pub fn validate(&self) -> Result<(), LayoutError> {
        for (i, a) in self.displays.iter().enumerate() {
            if !(a.rect.width > 0.0 && a.rect.height > 0.0 && a.scale > 0.0) {
                return Err(LayoutError::InvalidDisplay(a.id.clone()));
            }
            for b in &self.displays[i + 1..] {
                if a.id == b.id {
                    return Err(LayoutError::DuplicateDisplay(a.id.clone()));
                }
                if a.rect.intersects(&b.rect) {
                    return Err(LayoutError::Overlap(a.id.clone(), b.id.clone()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::geometry::assert_near;

    #[test]
    fn device_id_validation_and_short_never_panics() {
        let good = DeviceId("0123456789abcdef".repeat(4));
        assert!(good.is_well_formed());
        assert_eq!(good.short(), "01234567");
        for bad in ["", "ab", &"A".repeat(64), &"g".repeat(64), &"+f".repeat(32)] {
            assert!(!DeviceId(bad.to_owned()).is_well_formed(), "{bad}");
        }
        // 7 个 ASCII + 汉字：按字节切第 8 位会落在字符中间。
        let evil = DeviceId("abcdefg中文字符".into());
        assert!(!evil.is_well_formed());
        assert_eq!(evil.short(), "abcdefg中");
        assert_eq!(DeviceId("短".into()).short(), "短");
    }

    pub fn display(device: &str, local: &str, rect: Rect, scale: f64) -> Display {
        Display {
            id: DisplayId {
                device: DeviceId(device.into()),
                local: local.into(),
            },
            name: format!("{device}-{local}"),
            rect,
            scale,
            native: None,
        }
    }

    pub fn id(device: &str, local: &str) -> DisplayId {
        DisplayId {
            device: DeviceId(device.into()),
            local: local.into(),
        }
    }

    /// Mac 笔记本 1440x900@2x 在左，顶边对齐；Windows 2560x1440@1.5x 在右。
    pub fn mac_and_pc() -> Layout {
        Layout::new(vec![
            display("mac", "0", Rect::new(0.0, 0.0, 1440.0, 900.0), 2.0),
            display("pc", "0", Rect::new(1440.0, 0.0, 2560.0, 1440.0), 1.5),
        ])
    }

    #[test]
    fn neighbors_require_touching_and_overlap() {
        let l = mac_and_pc();
        assert_eq!(l.neighbors(&id("mac", "0"), Edge::Right).len(), 1);
        assert!(l.neighbors(&id("mac", "0"), Edge::Left).is_empty());
        assert!(l.neighbors(&id("mac", "0"), Edge::Bottom).is_empty());
    }

    #[test]
    fn continuous_crossing_keeps_along_coordinate() {
        let l = mac_and_pc();
        let c = l
            .cross_edge(
                &id("mac", "0"),
                Edge::Right,
                Point::new(1439.999, 450.0),
                EdgeMapping::Continuous,
            )
            .unwrap();
        assert_eq!(c.display, id("pc", "0"));
        assert_eq!(c.point, Point::new(1440.0, 450.0));
    }

    #[test]
    fn continuous_crossing_blocked_outside_overlap() {
        let l = mac_and_pc();
        // PC 左边 y=1200 处，Mac 那边没有屏幕
        let c = l.cross_edge(
            &id("pc", "0"),
            Edge::Left,
            Point::new(1440.0, 1200.0),
            EdgeMapping::Continuous,
        );
        assert!(c.is_none());
    }

    #[test]
    fn proportional_crossing_uses_full_edge() {
        let l = mac_and_pc();
        let c = l
            .cross_edge(
                &id("pc", "0"),
                Edge::Left,
                Point::new(1440.0, 1080.0),
                EdgeMapping::Proportional,
            )
            .unwrap();
        assert_eq!(c.display, id("mac", "0"));
        // 1080 / 1440 = 0.75 → 0.75 * 900 = 675；进入点在 Mac 右边缘
        assert_near(c.point, Point::new(1440.0, 675.0));
    }

    #[test]
    fn crossing_up_and_down() {
        let l = Layout::new(vec![
            display("a", "0", Rect::new(0.0, 0.0, 1920.0, 1080.0), 1.0),
            display("b", "0", Rect::new(200.0, -900.0, 1440.0, 900.0), 2.0),
        ]);
        let up = l
            .cross_edge(
                &id("a", "0"),
                Edge::Top,
                Point::new(500.0, 0.0),
                EdgeMapping::Continuous,
            )
            .unwrap();
        assert_near(up.point, Point::new(500.0, 0.0));
        assert_eq!(up.display, id("b", "0"));
        let down = l
            .cross_edge(
                &id("b", "0"),
                Edge::Bottom,
                up.point,
                EdgeMapping::Continuous,
            )
            .unwrap();
        assert_eq!(down.display, id("a", "0"));
        assert_eq!(down.point, Point::new(500.0, 0.0));
    }

    #[test]
    fn proportional_falls_back_with_multiple_neighbors() {
        let l = Layout::new(vec![
            display("a", "0", Rect::new(0.0, 0.0, 1000.0, 1000.0), 1.0),
            display("b", "0", Rect::new(1000.0, 0.0, 500.0, 400.0), 1.0),
            display("b", "1", Rect::new(1000.0, 400.0, 500.0, 400.0), 1.0),
        ]);
        let c = l
            .cross_edge(
                &id("a", "0"),
                Edge::Right,
                Point::new(999.0, 500.0),
                EdgeMapping::Proportional,
            )
            .unwrap();
        assert_eq!(c.display, id("b", "1"));
        assert_eq!(c.point.y, 500.0);
        assert!(
            l.cross_edge(
                &id("a", "0"),
                Edge::Right,
                Point::new(999.0, 900.0),
                EdgeMapping::Proportional
            )
            .is_none()
        );
    }

    #[test]
    fn snap_to_adjacent_edge_and_align_top() {
        let l = mac_and_pc();
        // 把 PC 往右拖了 12、往下拖了 7：应吸附回贴边并顶边对齐
        let mut moved = l.clone();
        moved.move_device(&DeviceId("pc".into()), 12.0, 7.0);
        let (dx, dy) = moved.snap_device_offset(&DeviceId("pc".into()), 0.0, 0.0, 16.0);
        assert_eq!((dx, dy), (-12.0, -7.0));
    }

    #[test]
    fn no_snap_when_far_away() {
        let l = mac_and_pc();
        let (dx, dy) = l.snap_device_offset(&DeviceId("pc".into()), 300.0, 300.0, 16.0);
        assert_eq!((dx, dy), (300.0, 300.0));
    }

    #[test]
    fn dragging_into_overlap_pushes_to_touching_edge() {
        let l = mac_and_pc();
        let pc = DeviceId("pc".into());
        // PC 往左拖进 Mac 200 点（超出吸附范围）：推回贴边，而不是报错
        let (dx, dy) = l.place_device(&pc, -200.0, 30.0, 16.0).unwrap();
        let mut moved = l.clone();
        moved.move_device(&pc, dx, dy);
        assert!(moved.validate().is_ok());
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 30.0);
        // 落点本来就不重叠时保持不变
        assert_eq!(
            l.place_device(&pc, 300.0, 300.0, 16.0),
            Some((300.0, 300.0))
        );
    }

    #[test]
    fn validate_detects_overlap_and_bad_scale() {
        let mut l = mac_and_pc();
        assert!(l.validate().is_ok());
        l.move_device(&DeviceId("pc".into()), -10.0, 0.0);
        assert!(matches!(l.validate(), Err(LayoutError::Overlap(_, _))));

        let bad = Layout::new(vec![display(
            "x",
            "0",
            Rect::new(0.0, 0.0, 10.0, 10.0),
            0.0,
        )]);
        assert!(matches!(
            bad.validate(),
            Err(LayoutError::InvalidDisplay(_))
        ));
    }
}
