//! Windows：屏幕边缘浮光。
//!
//! 一个贴着屏幕边的分层窗口（`WS_EX_LAYERED`）：置顶、不抢焦点（`WS_EX_NOACTIVATE`）、
//! 鼠标事件穿透（`WS_EX_TRANSPARENT`）、不出现在任务栏（`WS_EX_TOOLWINDOW`）。
//! 每次显示开一个短命的线程：建窗口，按 [`crate::glow`] 逐帧算像素并更新，播完销毁。
//! 窗口必须在创建它的线程上销毁，所以整个生命周期都在这个线程里。

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flow_core::geometry::Edge;
use windows_sys::Win32::Foundation::{POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, SelectObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassExW, SPI_GETCLIENTAREAANIMATION, SW_SHOWNOACTIVATE, ShowWindow,
    SystemParametersInfoW, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::glow;

/// 每次显示加一；正在播的那次发现对不上就提前结束，不会两道光叠在一起。
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 每帧之间的间隔（约 60 帧）。
const FRAME: Duration = Duration::from_millis(16);

/// 在 `display_local_id` 这块屏幕的 `edge` 边上闪一道浮光；`along` 是进入点在这条边上的位置
/// （0..1，左右边从上往下，上下边从左往右）。找不到这块屏幕时什么也不做。
pub fn show(display_local_id: &str, edge: Edge, along: f64) {
    let Ok(monitors) = crate::windows::monitors() else {
        return;
    };
    let Some(m) = monitors.into_iter().find(|m| m.id == display_local_id) else {
        return;
    };
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = std::thread::Builder::new()
        .name("flowacross-glow".into())
        .spawn(move || {
            // SAFETY: 窗口、位图和设备上下文都在这个线程里创建、使用和销毁。
            unsafe { play(m.phys, m.scale, edge, along, generation) }
        });
}

/// 系统是否打开了窗口动画（「设置 › 辅助功能 › 视觉效果 › 动画效果」）。关掉时浮光只淡入淡出，不散开。
fn animations_enabled() -> bool {
    let mut on: i32 = 1;
    // SAFETY: 按文档传入一个 BOOL 的地址。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            &mut on as *mut i32 as *mut c_void,
            0,
        )
    };
    ok == 0 || on != 0
}

/// `phys`：显示器的物理像素矩形（left, top, right, bottom）；`scale`：像素 / 逻辑点。
unsafe fn play(phys: (i32, i32, i32, i32), scale: f64, edge: Edge, along: f64, generation: u64) {
    let (left, top, right, bottom) = phys;
    let (mon_w, mon_h) = (right - left, bottom - top);
    if mon_w <= 0 || mon_h <= 0 {
        return;
    }
    let depth = ((glow::DEPTH * scale).round() as i32).clamp(1, mon_w.min(mon_h));
    let (x, y, w, h) = match edge {
        Edge::Left => (left, top, depth, mon_h),
        Edge::Right => (right - depth, top, depth, mon_h),
        Edge::Top => (left, top, mon_w, depth),
        Edge::Bottom => (left, bottom - depth, mon_w, depth),
    };
    let length = if edge.is_vertical() { h } else { w } as f64;
    let spot = along.clamp(0.0, 1.0) * length;
    let radius = glow::spot_radius(scale, length);
    let animate = animations_enabled();

    let class: Vec<u16> = "FlowAcrossEdgeGlow"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 以下都是按文档调用的窗口和 GDI 接口；每个句柄都检查过再用，结束前逐个释放。
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(DefWindowProcW);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        // 第二次起会因为"已注册"而失败，忽略即可。
        RegisterClassExW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }

        // 32 位、第一行在最上面（高度为负）的位图，像素由 glow::render 填。
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = if dc.is_null() {
            std::ptr::null_mut()
        } else {
            CreateDIBSection(
                dc,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            )
        };
        if !bitmap.is_null() && !bits.is_null() {
            let old = SelectObject(dc, bitmap);
            let (width, height) = (w as usize, h as usize);
            let buf = std::slice::from_raw_parts_mut(bits as *mut u8, width * height * 4);
            let position = POINT { x, y };
            let size = SIZE { cx: w, cy: h };
            let origin = POINT { x: 0, y: 0 };
            let started = Instant::now();
            let mut shown = false;
            loop {
                let t = started.elapsed().as_secs_f64() / glow::DURATION;
                if t >= 1.0 || GENERATION.load(Ordering::SeqCst) != generation {
                    break;
                }
                let spread = if animate { glow::spread(t) } else { 1.0 };
                glow::render(buf, width, height, edge, spot, radius * spread);
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: (glow::envelope(t) * 255.0).round() as u8,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                UpdateLayeredWindow(
                    hwnd,
                    std::ptr::null_mut(),
                    &position,
                    &size,
                    dc,
                    &origin,
                    0,
                    &blend,
                    ULW_ALPHA,
                );
                if !shown {
                    // 显示但不激活：正在打字的窗口不会失去焦点。
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    shown = true;
                }
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(FRAME);
            }
            SelectObject(dc, old);
        }
        DestroyWindow(hwnd);
        if !bitmap.is_null() {
            DeleteObject(bitmap);
        }
        if !dc.is_null() {
            DeleteDC(dc);
        }
    }
}
