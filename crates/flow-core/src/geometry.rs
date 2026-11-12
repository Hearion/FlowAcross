//! 布局空间中的基础几何类型。单位为逻辑点（与缩放无关）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// 点被夹到矩形内时，距离右/下边界的最小距离。
/// 坐标是连续的逻辑点，用极小值而不是 1，保证高分屏上也能到达最后一列物理像素。
pub const EDGE_EPSILON: f64 = 1e-3;

/// 轴对齐矩形，半开区间：`x ∈ [left, right)`、`y ∈ [top, bottom)`。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn left(&self) -> f64 {
        self.x
    }
    pub fn top(&self) -> f64 {
        self.y
    }
    pub fn right(&self) -> f64 {
        self.x + self.width
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.left() && p.x < self.right() && p.y >= self.top() && p.y < self.bottom()
    }

    /// 把点夹到矩形内（右/下边界最多到 `right - EDGE_EPSILON`）。
    pub fn clamp(&self, p: Point) -> Point {
        Point::new(
            p.x.clamp(self.left(), (self.right() - EDGE_EPSILON).max(self.left())),
            p.y.clamp(self.top(), (self.bottom() - EDGE_EPSILON).max(self.top())),
        )
    }

    /// 两个矩形是否有面积大于 0 的重叠。
    pub fn intersects(&self, other: &Rect) -> bool {
        overlap(self.left(), self.right(), other.left(), other.right()) > 0.0
            && overlap(self.top(), self.bottom(), other.top(), other.bottom()) > 0.0
    }

    pub fn translated(&self, dx: f64, dy: f64) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.width, self.height)
    }

    /// 离点最近的那条边（点在矩形外时按夹到矩形内之后算）。距离相同时按左、右、上、下的顺序取。
    /// 键鼠从别的设备进来时，落点就贴着它穿过的那条边，据此知道是从哪边进来的。
    pub fn nearest_edge(&self, p: Point) -> Edge {
        let p = self.clamp(p);
        let mut best = (Edge::Left, f64::INFINITY);
        for edge in Edge::ALL {
            let across = if edge.is_vertical() { p.x } else { p.y };
            let d = (self.edge_coord(edge) - across).abs();
            if d < best.1 {
                best = (edge, d);
            }
        }
        best.0
    }

    /// 点在某条边上的位置占这条边长度的比例，0..1（左右边从上往下，上下边从左往右）。
    pub fn edge_fraction(&self, edge: Edge, p: Point) -> f64 {
        let (start, end) = self.edge_span(edge);
        if end <= start {
            return 0.0;
        }
        ((edge.along(p) - start) / (end - start)).clamp(0.0, 1.0)
    }

    /// 某条边在"沿边方向"上的区间，例如左右边返回 `[top, bottom)`。
    pub fn edge_span(&self, edge: Edge) -> (f64, f64) {
        match edge {
            Edge::Left | Edge::Right => (self.top(), self.bottom()),
            Edge::Top | Edge::Bottom => (self.left(), self.right()),
        }
    }

    /// 某条边在"垂直于边方向"上的坐标，例如右边返回 `right`。
    pub fn edge_coord(&self, edge: Edge) -> f64 {
        match edge {
            Edge::Left => self.left(),
            Edge::Right => self.right(),
            Edge::Top => self.top(),
            Edge::Bottom => self.bottom(),
        }
    }
}

/// 一维区间 `[a0, a1)` 与 `[b0, b1)` 的重叠长度（无重叠为 0）。
pub fn overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    /// 是否为左右边（沿边方向为 y）。
    pub fn is_vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }

    /// 点在"沿边方向"上的分量。
    pub fn along(self, p: Point) -> f64 {
        if self.is_vertical() { p.y } else { p.x }
    }
}

/// 测试用：两个点在 0.01 逻辑点内视为相等。
#[cfg(test)]
pub(crate) fn assert_near(a: Point, b: Point) {
    assert!(
        (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01,
        "{a:?} != {b:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(r.contains(Point::new(99.5, 49.5)));
        assert!(!r.contains(Point::new(100.0, 10.0)));
        assert!(!r.contains(Point::new(10.0, 50.0)));
    }

    #[test]
    fn clamp_stays_inside() {
        let r = Rect::new(10.0, 10.0, 100.0, 50.0);
        let p = r.clamp(Point::new(500.0, -5.0));
        assert!(r.contains(p));
        assert!((p.x - 110.0).abs() < 0.01 && p.y == 10.0);
    }

    #[test]
    fn nearest_edge_is_where_the_pointer_came_in() {
        let r = Rect::new(100.0, 50.0, 1920.0, 1080.0);
        assert_eq!(r.nearest_edge(Point::new(100.0, 600.0)), Edge::Left);
        assert_eq!(r.nearest_edge(Point::new(2019.9, 600.0)), Edge::Right);
        assert_eq!(r.nearest_edge(Point::new(900.0, 50.5)), Edge::Top);
        assert_eq!(r.nearest_edge(Point::new(900.0, 1129.0)), Edge::Bottom);
        // 屏幕外的点先夹进来；正好在角上时取左右边。
        assert_eq!(r.nearest_edge(Point::new(-500.0, 600.0)), Edge::Left);
        assert_eq!(r.nearest_edge(Point::new(5000.0, 5000.0)), Edge::Right);
        assert_eq!(r.nearest_edge(Point::new(100.0, 50.0)), Edge::Left);
        // 在这条边上的位置：左右边看 y，上下边看 x；超出范围的夹住。
        assert_eq!(r.edge_fraction(Edge::Left, Point::new(100.0, 590.0)), 0.5);
        assert_eq!(r.edge_fraction(Edge::Top, Point::new(580.0, 50.0)), 0.25);
        assert_eq!(r.edge_fraction(Edge::Bottom, Point::new(9000.0, 0.0)), 1.0);
        assert_eq!(
            Rect::new(0.0, 0.0, 0.0, 0.0).edge_fraction(Edge::Left, Point::new(1.0, 1.0)),
            0.0
        );
    }

    #[test]
    fn touching_rects_do_not_intersect() {
        let a = Rect::new(0.0, 0.0, 100.0, 100.0);
        let b = Rect::new(100.0, 0.0, 100.0, 100.0);
        assert!(!a.intersects(&b));
        assert!(a.intersects(&b.translated(-1.0, 0.0)));
    }
}
