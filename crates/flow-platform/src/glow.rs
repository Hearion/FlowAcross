//! 屏幕边缘浮光的形状和节奏：两个系统共用的参数和纯计算，不调用系统 API。
//!
//! 浮光由两部分叠成：整条边上一道很浅的光带，加上进入点处一团更亮的光，亮的那团沿着边向两侧散开。
//! macOS 用 Core Animation 的渐变图层按这里的参数画；Windows 没有现成的，用 [`render`] 逐帧算像素。

use flow_core::geometry::Edge;

/// 整个动画的时长（秒）。
pub const DURATION: f64 = 0.75;
/// 浮光从屏幕边缘向内延伸的深度（逻辑点）。
pub const DEPTH: f64 = 96.0;
/// 进入点那团光沿边方向的半径（逻辑点），屏幕边很短时不超过边长的 45%。
pub const SPOT_RADIUS: f64 = 320.0;
/// 品牌蓝（深色模式下的 `--accent`，亮一档，在深色和浅色桌面上都看得清）。
pub const COLOR: (u8, u8, u8) = (0x6B, 0x9B, 0xFF);

/// 整体不透明度随时间的关键帧（时间占比，不透明度）：很快亮起，停一下，慢慢淡出。
pub const FADE: [(f64, f64); 4] = [(0.0, 0.0), (0.15, 1.0), (0.4, 1.0), (1.0, 0.0)];
/// 进入点那团光沿边方向的缩放：从 [`SPREAD_FROM`] 散开到 [`SPREAD_TO`]。
pub const SPREAD_FROM: f64 = 0.35;
pub const SPREAD_TO: f64 = 1.6;
/// 整条边的光带贴边处的不透明度。
pub const BAND_ALPHA: f64 = 0.5;
/// 进入点那团光中心的不透明度。
pub const SPOT_ALPHA: f64 = 0.9;

/// 进入点那团光沿边方向的半径：`scale` 为像素 / 逻辑点，`edge_len` 为这条边的长度（同单位）。
pub fn spot_radius(scale: f64, edge_len: f64) -> f64 {
    (SPOT_RADIUS * scale).min(edge_len * 0.45).max(1.0)
}

/// `t`（0..1）时刻的整体不透明度，按 [`FADE`] 线性插值。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn envelope(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    for pair in FADE.windows(2) {
        let ((t0, v0), (t1, v1)) = (pair[0], pair[1]);
        if t <= t1 {
            return v0 + (v1 - v0) * (t - t0) / (t1 - t0);
        }
    }
    0.0
}

/// `t`（0..1）时刻那团光的缩放：先快后慢地散开。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn spread(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - t).powi(3);
    SPREAD_FROM + (SPREAD_TO - SPREAD_FROM) * eased
}

/// 一个点的不透明度。`depth`：离屏幕边缘的距离 / 浮光深度（0 贴边，1 最里面）；
/// `along`：沿边方向离进入点的距离 / 那团光此刻的半径。
#[cfg_attr(not(windows), allow(dead_code))]
fn alpha_at(depth: f64, along: f64) -> f64 {
    let band = BAND_ALPHA * (1.0 - depth).max(0.0).powi(3);
    let r = (depth * depth + along * along).sqrt();
    let spot = if r < 1.0 {
        SPOT_ALPHA * (1.0 - r) * (1.0 - r)
    } else {
        0.0
    };
    (band + spot).min(1.0)
}

/// 画一帧：`buf` 是 `width × height` 的 32 位位图（BGRA、预乘透明度、第一行在最上面），
/// 窗口贴着 `edge` 这条屏幕边。`spot`：进入点在沿边方向上的像素位置；`radius`：那团光此刻的半径（像素）。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn render(buf: &mut [u8], width: usize, height: usize, edge: Edge, spot: f64, radius: f64) {
    if width == 0 || height == 0 || buf.len() < width * height * 4 {
        return;
    }
    let radius = radius.max(1.0);
    let (r, g, b) = (COLOR.0 as f64, COLOR.1 as f64, COLOR.2 as f64);
    for y in 0..height {
        for x in 0..width {
            // 像素中心离屏幕边缘的距离占浮光深度的比例，以及沿边方向的位置。
            let (depth, along) = match edge {
                Edge::Left => ((x as f64 + 0.5) / width as f64, y as f64),
                Edge::Right => ((width - x) as f64 - 0.5, y as f64).div_first(width as f64),
                Edge::Top => ((y as f64 + 0.5) / height as f64, x as f64),
                Edge::Bottom => ((height - y) as f64 - 0.5, x as f64).div_first(height as f64),
            };
            let a = alpha_at(depth, (along + 0.5 - spot) / radius);
            let px = &mut buf[(y * width + x) * 4..][..4];
            px[0] = (b * a).round() as u8;
            px[1] = (g * a).round() as u8;
            px[2] = (r * a).round() as u8;
            px[3] = (a * 255.0).round() as u8;
        }
    }
}

/// 只把二元组的第一项除以一个数（让上面四条边的写法对齐）。
trait DivFirst {
    fn div_first(self, by: f64) -> (f64, f64);
}

impl DivFirst for (f64, f64) {
    fn div_first(self, by: f64) -> (f64, f64) {
        (self.0 / by, self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(buf: &[u8], width: usize, x: usize, y: usize) -> u8 {
        buf[(y * width + x) * 4 + 3]
    }

    #[test]
    fn fades_in_holds_and_fades_out() {
        assert_eq!(envelope(0.0), 0.0);
        assert!((envelope(0.075) - 0.5).abs() < 1e-9);
        assert_eq!(envelope(0.15), 1.0);
        assert_eq!(envelope(0.3), 1.0);
        assert!((envelope(0.7) - 0.5).abs() < 1e-9);
        assert_eq!(envelope(1.0), 0.0);
        assert_eq!(envelope(5.0), 0.0);
    }

    #[test]
    fn spot_spreads_out_and_slows_down() {
        assert_eq!(spread(0.0), SPREAD_FROM);
        assert!((spread(1.0) - SPREAD_TO).abs() < 1e-9);
        assert!(spread(0.2) < spread(0.4) && spread(0.4) < spread(0.9));
        // 前半段走得比后半段多。
        assert!(spread(0.5) - spread(0.0) > spread(1.0) - spread(0.5));
        assert_eq!(spot_radius(2.0, 10_000.0), 640.0);
        assert_eq!(spot_radius(1.0, 400.0), 180.0);
    }

    #[test]
    fn brightest_at_the_edge_next_to_the_entry_point() {
        // 左边：10 像素深、100 像素长，从第 30 行进来。
        let (w, h) = (10, 100);
        let mut buf = vec![0u8; w * h * 4];
        render(&mut buf, w, h, Edge::Left, 30.0, 20.0);
        let at_spot = alpha(&buf, w, 0, 30);
        assert!(at_spot > alpha(&buf, w, 0, 80), "进入点比远处亮");
        assert!(at_spot > alpha(&buf, w, 5, 30), "贴边比里面亮");
        assert!(alpha(&buf, w, 0, 80) > 0, "整条边都有一道浅光");
        assert!(alpha(&buf, w, w - 1, 80) < 3, "最里面几乎全透明");
        // 预乘：颜色分量不超过不透明度。
        for px in buf.chunks_exact(4) {
            assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3]);
        }

        // 右边是左边的镜像；下边是上边的镜像。
        let mut right = vec![0u8; w * h * 4];
        render(&mut right, w, h, Edge::Right, 30.0, 20.0);
        assert_eq!(alpha(&right, w, w - 1, 30), at_spot);
        let (w, h) = (100, 10);
        let mut top = vec![0u8; w * h * 4];
        let mut bottom = vec![0u8; w * h * 4];
        render(&mut top, w, h, Edge::Top, 30.0, 20.0);
        render(&mut bottom, w, h, Edge::Bottom, 30.0, 20.0);
        assert_eq!(alpha(&top, w, 30, 0), at_spot);
        assert_eq!(alpha(&bottom, w, 30, h - 1), at_spot);
        assert_eq!(alpha(&top, w, 70, 3), alpha(&bottom, w, 70, h - 4));
    }

    #[test]
    fn short_buffers_are_left_alone() {
        let mut buf = vec![7u8; 8];
        render(&mut buf, 10, 10, Edge::Top, 0.0, 5.0);
        assert_eq!(buf, [7; 8]);
    }
}
