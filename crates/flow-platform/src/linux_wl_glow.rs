//! Linux（Wayland）屏幕边缘浮光：一条贴在屏幕边上的 layer-shell 窗口（最上层），逐帧把
//! [`crate::glow::render`] 算出的像素写进共享内存缓冲区。
//!
//! - 输入区域设成空的，鼠标事件穿透到下面的窗口；不要键盘焦点。
//! - 按逻辑点的分辨率画（不乘缩放）：浮光本来就是一团柔和的光，放大后看不出差别，省掉一半以上的像素。
//! - 合成器没有 layer-shell（GNOME）时不显示。
//!
//! 每次浮光用一条自己的连接、在单独的线程里播；新的一次开始时，上一次提前结束。

use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flow_core::geometry::Edge;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::glow;
use crate::linux::{self, Monitor};
use crate::linux_glow::fade;
use crate::linux_wl::{self, anonymous_file, globals_of};

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
        .name("flowacross-wl-glow".into())
        .spawn(move || {
            // 浮光只是提示，任何一步失败都直接不显示。
            let _ = play(&monitor, edge, along, generation);
        });
}

/// 合成器配置好的窗口尺寸（逻辑点）；窗口被关掉时 `closed` 为真。
#[derive(Default)]
struct State {
    size: Option<(u32, u32)>,
    closed: bool,
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

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                state.size = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ZwlrLayerShellV1);

/// 贴着 `edge` 的窗口：锚点和请求的尺寸（0 表示沿这条边铺满）。
fn placement(edge: Edge, depth: u32) -> (Anchor, u32, u32) {
    match edge {
        Edge::Left => (Anchor::Left | Anchor::Top | Anchor::Bottom, depth, 0),
        Edge::Right => (Anchor::Right | Anchor::Top | Anchor::Bottom, depth, 0),
        Edge::Top => (Anchor::Top | Anchor::Left | Anchor::Right, 0, depth),
        Edge::Bottom => (Anchor::Bottom | Anchor::Left | Anchor::Right, 0, depth),
    }
}

fn play(
    monitor: &Monitor,
    edge: Edge,
    along: f64,
    generation: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = linux_wl::connect().map_err(|e| e.to_string())?;
    let (globals, mut queue): (GlobalList, EventQueue<State>) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let Ok(layer_shell) = globals.bind::<ZwlrLayerShellV1, _, _>(&qh, 1..=4, ()) else {
        return Ok(());
    };
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let Some((global, version)) = globals_of(&globals, "wl_output")
        .into_iter()
        .find(|(name, _)| *name == monitor.wl_global)
    else {
        return Ok(());
    };
    let output: wl_output::WlOutput = globals.registry().bind(global, version.min(4), &qh, ());

    let (mon_w, mon_h) = (monitor.logical.width, monitor.logical.height);
    let depth = (glow::DEPTH.round() as u32).clamp(1, mon_w.min(mon_h).max(1.0) as u32);
    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        Some(&output),
        Layer::Overlay,
        "flowacross-glow".to_owned(),
        &qh,
        (),
    );
    let (anchor, w, h) = placement(edge, depth);
    layer.set_anchor(anchor);
    layer.set_size(w, h);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // 输入区域为空：鼠标点击和移动都穿透到下面的窗口。
    let region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();
    surface.commit();

    let mut state = State::default();
    for _ in 0..3 {
        queue.roundtrip(&mut state)?;
        if state.size.is_some() {
            break;
        }
    }
    let Some((width, height)) = state.size.filter(|(w, h)| *w > 0 && *h > 0) else {
        return Ok(());
    };
    let (width, height) = (width as usize, height as usize);
    let length = match edge {
        Edge::Left | Edge::Right => height,
        Edge::Top | Edge::Bottom => width,
    } as f64;
    let spot = along.clamp(0.0, 1.0) * length;
    let radius = glow::spot_radius(1.0, length);

    let mut buf = vec![0u8; width * height * 4];
    let file = anonymous_file(&buf)?;
    let pool = shm.create_pool(file.as_fd(), buf.len() as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        width as i32 * 4,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );

    let started = Instant::now();
    loop {
        let t = started.elapsed().as_secs_f64() / glow::DURATION;
        if t >= 1.0 || state.closed || GENERATION.load(Ordering::SeqCst) != generation {
            break;
        }
        // 小端机器上 ARGB8888 的字节序就是 B、G、R、A，和 `render` 的输出一致。
        glow::render(
            &mut buf,
            width,
            height,
            edge,
            spot,
            radius * glow::spread(t),
        );
        fade(&mut buf, glow::envelope(t));
        file.write_all_at(&buf, 0)?;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, i32::MAX, i32::MAX);
        surface.commit();
        conn.flush()?;
        queue.dispatch_pending(&mut state)?;
        std::thread::sleep(FRAME);
    }
    layer.destroy();
    surface.destroy();
    buffer.destroy();
    pool.destroy();
    // 等合成器处理完再断开，窗口才会确实消失。
    queue.roundtrip(&mut state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 grim 截屏，返回几个像素的 RGB。
    fn pixels(points: &[(usize, usize)]) -> Vec<(u8, u8, u8)> {
        let out = std::process::Command::new("grim")
            .args(["-t", "png", "-"])
            .output()
            .expect("运行 grim");
        assert!(out.status.success(), "grim 失败");
        let mut reader = png::Decoder::new(std::io::Cursor::new(out.stdout))
            .read_info()
            .unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let bpp = info.color_type.samples();
        points
            .iter()
            .map(|(x, y)| {
                let i = y * info.line_size + x * bpp;
                (buf[i], buf[i + 1], buf[i + 2])
            })
            .collect()
    }

    #[test]
    #[ignore = "需要 wlroots 系合成器和 grim"]
    fn glow_shows_on_the_edge_and_goes_away() {
        let displays = crate::display_provider().displays().expect("枚举显示器");
        let d = &displays[0];
        let h = d.rect.height as usize;
        let scale = d.scale;
        let px = |x: f64, y: f64| ((x * scale) as usize, (y * scale) as usize);
        let spot = px(2.0, 0.3 * h as f64);
        let far = px(2.0, h as f64 - 40.0);
        let inside = px(400.0, 0.3 * h as f64);
        let before = pixels(&[spot, far, inside]);

        show(&d.local_id, Edge::Left, 0.3);
        std::thread::sleep(Duration::from_millis(280));
        let during = pixels(&[spot, far, inside]);
        // 进入点是亮蓝色，沿边远处是浅一些的光带，浮光深度以外不受影响。
        let (r, g, b) = during[0];
        assert!(
            b > 180 && b > r && b > g,
            "进入点应该是亮蓝色：{:?}",
            during[0]
        );
        assert_ne!(during[1], before[1], "整条边应该有光带");
        assert!(during[1].2 < b, "远处比进入点暗");
        assert_eq!(during[2], before[2], "浮光深度以外不应该变");

        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(pixels(&[spot, far, inside]), before, "播完后应该消失");
    }

    #[test]
    fn window_is_anchored_to_its_edge_and_stretches_along_it() {
        assert_eq!(
            placement(Edge::Left, 96),
            (Anchor::Left | Anchor::Top | Anchor::Bottom, 96, 0)
        );
        assert_eq!(
            placement(Edge::Bottom, 96),
            (Anchor::Bottom | Anchor::Left | Anchor::Right, 0, 96)
        );
        assert_eq!(placement(Edge::Top, 10).2, 10);
        assert_eq!(placement(Edge::Right, 10).1, 10);
    }
}
