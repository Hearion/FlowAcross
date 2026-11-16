//! Linux（X11）屏幕边缘浮光：一个 32 位色深、带透明通道的窗口贴在屏幕边上，逐帧把
//! [`crate::glow::render`] 算出的像素画上去。
//!
//! - 窗口是 override-redirect 的：窗口管理器不管它，不加边框、不激活、不抢键盘焦点，映射时就在最上层。
//! - 输入区域设成空的（XFixes），鼠标事件穿透到下面的窗口。
//! - 透明要靠窗口合成器。没有合成器时（很精简的窗口管理器）32 位窗口会显示成黑块，所以这时不显示浮光。
//!
//! 每次浮光用一条自己的 X 连接、在单独的线程里播；新的一次开始时，上一次提前结束。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flow_core::geometry::Edge;
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::shape::SK;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ColormapAlloc, ConnectionExt as _, CreateGCAux, CreateWindowAux, ImageFormat, VisualClass,
    Visualid, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::glow;
use crate::linux;

/// 每次调用加一；播放线程发现自己不是最新的一次就收工。
static GENERATION: AtomicU64 = AtomicU64::new(0);
const FRAME: Duration = Duration::from_millis(16);

pub fn show(display_local_id: &str, edge: Edge, along: f64) {
    let monitor = linux::with_monitors(|list| {
        list.iter()
            .find(|m| m.id == display_local_id)
            .or_else(|| list.iter().find(|m| m.primary))
            .cloned()
    });
    let Some(monitor) = monitor else {
        return;
    };
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = std::thread::Builder::new()
        .name("flowacross-x11-glow".into())
        .spawn(move || {
            // 浮光只是提示，任何一步失败都直接不显示。
            let _ = play(monitor.phys, monitor.scale, edge, along, generation);
        });
}

/// 浮光窗口的位置和大小（像素）：贴着 `edge`，向屏幕内延伸 `depth`。返回 (x, y, 宽, 高)。
fn window_rect(phys: (i32, i32, i32, i32), edge: Edge, depth: i32) -> (i32, i32, i32, i32) {
    let (left, top, right, bottom) = phys;
    let (w, h) = (right - left, bottom - top);
    match edge {
        Edge::Left => (left, top, depth, h),
        Edge::Right => (right - depth, top, depth, h),
        Edge::Top => (left, top, w, depth),
        Edge::Bottom => (left, bottom - depth, w, depth),
    }
}

/// 把一帧（预乘透明度的 BGRA）整体乘上不透明度。
pub(crate) fn fade(buf: &mut [u8], opacity: f64) {
    let k = (opacity.clamp(0.0, 1.0) * 256.0).round() as u32;
    for b in buf {
        *b = ((u32::from(*b) * k) >> 8) as u8;
    }
}

/// 找一个 32 位色深的 TrueColor 视觉（带透明通道）。
fn argb_visual(conn: &RustConnection, screen: usize) -> Option<Visualid> {
    conn.setup().roots[screen]
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
}

/// 有没有窗口合成器在运行（它会占着 `_NET_WM_CM_S<屏幕号>` 这个选区）。
fn compositing(conn: &RustConnection, screen: usize) -> bool {
    let name = format!("_NET_WM_CM_S{screen}");
    let Some(atom) = conn
        .intern_atom(false, name.as_bytes())
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return false;
    };
    conn.get_selection_owner(atom.atom)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|r| r.owner != x11rb::NONE)
}

fn play(
    phys: (i32, i32, i32, i32),
    scale: f64,
    edge: Edge,
    along: f64,
    generation: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let Some(visual) = argb_visual(&conn, screen) else {
        return Ok(());
    };
    if !compositing(&conn, screen) {
        return Ok(());
    }

    let (mon_w, mon_h) = (phys.2 - phys.0, phys.3 - phys.1);
    if mon_w <= 0 || mon_h <= 0 {
        return Ok(());
    }
    let depth = ((glow::DEPTH * scale).round() as i32).clamp(1, mon_w.min(mon_h));
    let (x, y, w, h) = window_rect(phys, edge, depth);
    let (width, height) = (w as usize, h as usize);
    // 一帧要放进一个请求里；放不下（屏幕极大而服务端不支持大请求）就不显示。
    if width * height * 4 + 64 > conn.maximum_request_bytes() {
        return Ok(());
    }
    let length = match edge {
        Edge::Left | Edge::Right => h,
        Edge::Top | Edge::Bottom => w,
    } as f64;
    let spot = along.clamp(0.0, 1.0) * length;
    let radius = glow::spot_radius(scale, length);

    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, root, visual)?;
    let win = conn.generate_id()?;
    conn.create_window(
        32,
        win,
        root,
        x as i16,
        y as i16,
        w as u16,
        h as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap),
    )?;
    // 输入区域为空：鼠标点击和移动都穿透到下面的窗口。
    conn.xfixes_query_version(5, 0)?.reply()?;
    let region = conn.generate_id()?;
    conn.xfixes_create_region(region, &[])?;
    conn.xfixes_set_window_shape_region(win, SK::INPUT, 0, 0, region)?;
    conn.xfixes_destroy_region(region)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))?;
    conn.map_window(win)?;

    let mut buf = vec![0u8; width * height * 4];
    let started = Instant::now();
    loop {
        let t = started.elapsed().as_secs_f64() / glow::DURATION;
        if t >= 1.0 || GENERATION.load(Ordering::SeqCst) != generation {
            break;
        }
        glow::render(
            &mut buf,
            width,
            height,
            edge,
            spot,
            radius * glow::spread(t),
        );
        fade(&mut buf, glow::envelope(t));
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            win,
            gc,
            w as u16,
            h as u16,
            0,
            0,
            0,
            32,
            &buf,
        )?;
        conn.flush()?;
        std::thread::sleep(FRAME);
    }
    conn.destroy_window(win)?;
    conn.free_colormap(colormap)?;
    conn.flush()?;
    // 等服务端处理完再断开，窗口才会确实消失。
    conn.get_input_focus()?.reply()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_hugs_the_edge_it_glows_on() {
        let phys = (1920, 0, 5760, 2160);
        assert_eq!(window_rect(phys, Edge::Left, 192), (1920, 0, 192, 2160));
        assert_eq!(window_rect(phys, Edge::Right, 192), (5568, 0, 192, 2160));
        assert_eq!(window_rect(phys, Edge::Top, 192), (1920, 0, 3840, 192));
        assert_eq!(
            window_rect(phys, Edge::Bottom, 192),
            (1920, 1968, 3840, 192)
        );
    }

    #[test]
    fn fading_scales_premultiplied_pixels() {
        let mut px = [200u8, 100, 50, 255];
        fade(&mut px, 0.5);
        assert_eq!(px, [100, 50, 25, 127]);
        let mut px = [200u8, 100, 50, 255];
        fade(&mut px, 0.0);
        assert_eq!(px, [0, 0, 0, 0]);
        let mut px = [200u8, 100, 50, 255];
        fade(&mut px, 1.0);
        assert_eq!(px, [200, 100, 50, 255]);
    }
}
