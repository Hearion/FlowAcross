//! Windows：屏幕边缘浮光。
//!
//! 一个贴着屏幕边的 DirectComposition 透明窗口：置顶、不抢焦点（`WS_EX_NOACTIVATE`）、
//! 鼠标穿透（`WS_EX_LAYERED | WS_EX_TRANSPARENT`）、不出现在任务栏（`WS_EX_TOOLWINDOW`）。
//! 每次显示开一个短命的线程：建窗口，按 [`crate::glow`] 逐帧算像素并更新，播完销毁。
//! 窗口必须在创建它的线程上销毁，所以整个生命周期都在这个线程里。

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flow_core::geometry::Edge;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1,
};
use windows::core::{Interface, Result};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassExW, SPI_GETCLIENTAREAANIMATION, SW_SHOWNOACTIVATE, ShowWindow,
    SystemParametersInfoW, TranslateMessage, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::glow;

/// 每次显示加一；正在播的那次发现对不上就提前结束，不会两道光叠在一起。
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 每帧之间的间隔（约 60 帧）。
const FRAME: Duration = Duration::from_millis(16);

/// 直接提交 BGRA 纹理，绕开 GDI 分层窗口在部分 Windows 环境中的横向错位。
struct Surface {
    context: ID3D11DeviceContext,
    chain: IDXGISwapChain1,
    _composition: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
}

impl Surface {
    unsafe fn new(
        hwnd: windows_sys::Win32::Foundation::HWND,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        // SAFETY: 所有 COM 对象只在当前绘制线程使用；输出由 windows crate 管理生命周期。
        unsafe {
            let mut device = None;
            let mut context = None;
            let create = |driver, device: &mut _, context: &mut _| {
                D3D11CreateDevice(
                    None,
                    driver,
                    Default::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(device),
                    None,
                    Some(context),
                )
            };
            if create(D3D_DRIVER_TYPE_HARDWARE, &mut device, &mut context).is_err() {
                create(D3D_DRIVER_TYPE_WARP, &mut device, &mut context)?;
            }
            let device = device.ok_or_else(|| {
                windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED)
            })?;
            let context = context.ok_or_else(|| {
                windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED)
            })?;
            let dxgi: IDXGIDevice = device.cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let chain = factory.CreateSwapChainForComposition(&device, &desc, None)?;
            let composition: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let target =
                composition.CreateTargetForHwnd(windows::Win32::Foundation::HWND(hwnd), true)?;
            let visual = composition.CreateVisual()?;
            visual.SetContent(&chain)?;
            target.SetRoot(&visual)?;
            composition.Commit()?;
            Ok(Self {
                context,
                chain,
                _composition: composition,
                _target: target,
                _visual: visual,
            })
        }
    }

    unsafe fn present(&self, pixels: &[u8], width: u32) -> Result<()> {
        // SAFETY: pixels 包含一整帧；每行 width 个 BGRA 像素，纹理尺寸在创建时固定。
        unsafe {
            let texture: ID3D11Texture2D = self.chain.GetBuffer(0)?;
            self.context
                .UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), width * 4, 0);
            self.chain.Present(0, DXGI_PRESENT(0)).ok()
        }
    }
}

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
            // SAFETY: 窗口和合成资源都在这个线程里创建、使用和销毁。
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

    // SAFETY: 窗口及全部合成资源仅在当前线程创建、使用和销毁。
    unsafe {
        let window = OverlayWindow::new(x, y, w, h);
        let hwnd = window.0;
        if hwnd.is_null() {
            return;
        }
        match Surface::new(hwnd, w as u32, h as u32) {
            Ok(surface) => {
                let (width, height) = (w as usize, h as usize);
                let mut buf = vec![0u8; width * height * 4];
                let started = Instant::now();
                let mut shown = false;
                loop {
                    let t = started.elapsed().as_secs_f64() / glow::DURATION;
                    if t >= 1.0 || GENERATION.load(Ordering::SeqCst) != generation {
                        break;
                    }
                    let spread = if animate { glow::spread(t) } else { 1.0 };
                    glow::render(&mut buf, width, height, edge, spot, radius * spread);
                    // 颜色已预乘透明度；淡出时 BGRA 四个分量必须一起缩放。
                    let opacity = glow::envelope(t);
                    for component in &mut buf {
                        *component = (*component as f64 * opacity).round() as u8;
                    }
                    if let Err(error) = surface.present(&buf, w as u32) {
                        eprintln!("屏幕边缘浮光提交失败: {error}");
                        break;
                    }
                    if !shown {
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
            }
            Err(error) => eprintln!("屏幕边缘浮光创建失败: {error}"),
        }
    }
}

/// 句柄不跨线程；先释放合成资源，再销毁承载窗口。
struct OverlayWindow(windows_sys::Win32::Foundation::HWND);

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 窗口只在创建它的线程使用和销毁。
            unsafe { DestroyWindow(self.0) };
        }
    }
}

impl OverlayWindow {
    unsafe fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let class: Vec<u16> = "FlowAcrossEdgeGlow"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: 类名在调用期间有效，窗口由返回值持有并在当前线程销毁。
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
                WS_EX_NOREDIRECTIONBITMAP
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | windows_sys::Win32::UI::WindowsAndMessaging::WS_EX_LAYERED,
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
            // LAYERED 仅用于让鼠标完全穿透；不再调用会错位的 UpdateLayeredWindow。
            // 像素与透明度由 DirectComposition 提交，窗口本身保持不透明度 255。
            windows_sys::Win32::UI::WindowsAndMessaging::SetLayeredWindowAttributes(
                hwnd,
                0,
                255,
                windows_sys::Win32::UI::WindowsAndMessaging::LWA_ALPHA,
            );
            Self(hwnd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, WindowFromPoint};

    #[test]
    #[ignore = "需要可见的 SDR Windows 桌面，会在四条屏幕边短暂显示测试图案"]
    fn composed_pixels_keep_their_columns_and_overlay_ignores_input() {
        let monitors = crate::windows::monitors().unwrap();
        let m = monitors.iter().find(|m| m.primary).unwrap();
        let (left, top, right, bottom) = m.phys;
        let (w, h) = (96, 64);
        let locations = [
            (left, top + (bottom - top - h) / 2),
            (right - w, top + (bottom - top - h) / 2),
            (left + (right - left - w) / 2, top),
            (left + (right - left - w) / 2, bottom - h),
        ];
        let color = |x: i32, y: i32| {
            [
                (x * 5 + y * 17 + 30) as u8,
                (x * 7 + y * 11 + 20) as u8,
                (x * 13 + y * 3 + 10) as u8,
                255,
            ]
        };
        let pixels: Vec<u8> = (0..h)
            .flat_map(|y| (0..w).flat_map(move |x| color(x, y)))
            .collect();
        // SAFETY: 只操作测试自建的窗口和纹理；每次采样后释放 DC，句柄由当前线程 RAII 清理。
        unsafe {
            for (x, y) in locations {
                let focus = GetForegroundWindow();
                let window = OverlayWindow::new(x, y, w, h);
                assert!(!window.0.is_null());
                let surface = Surface::new(window.0, w as u32, h as u32).unwrap();
                surface.present(&pixels, w as u32).unwrap();
                ShowWindow(window.0, SW_SHOWNOACTIVATE);
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    std::thread::sleep(FRAME);
                    let dc = GetDC(std::ptr::null_mut());
                    assert!(!dc.is_null());
                    let mut mismatch = None;
                    for row in [0, 7, h - 1] {
                        for col in [0, 1, 3, 4, 15, w - 1] {
                            let expected = color(col, row);
                            let actual = GetPixel(dc, x + col, y + row);
                            let rgb = [
                                ((actual >> 16) & 255) as u8,
                                ((actual >> 8) & 255) as u8,
                                (actual & 255) as u8,
                            ];
                            if rgb != expected[..3] {
                                mismatch = Some((col, row, expected, actual));
                            }
                        }
                    }
                    ReleaseDC(std::ptr::null_mut(), dc);
                    if mismatch.is_none() {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "屏幕边 ({x},{y}) 合成像素错位: {mismatch:?}"
                    );
                }
                assert_eq!(GetForegroundWindow(), focus, "浮光不能抢走焦点");
                assert_ne!(
                    WindowFromPoint(POINT {
                        x: x + 10,
                        y: y + 10
                    }),
                    window.0,
                    "浮光不能拦截鼠标"
                );
                // 全透明的第二层必须露出下面的已知图案，不能出现黑底或残留蓝线。
                let clear_window = OverlayWindow::new(x, y, w, h);
                let clear_surface = Surface::new(clear_window.0, w as u32, h as u32).unwrap();
                clear_surface
                    .present(&vec![0; pixels.len()], w as u32)
                    .unwrap();
                ShowWindow(clear_window.0, SW_SHOWNOACTIVATE);
                std::thread::sleep(Duration::from_millis(80));
                let dc = GetDC(std::ptr::null_mut());
                let actual: Vec<_> = [0, 3, 4, w - 1]
                    .into_iter()
                    .map(|col| GetPixel(dc, x + col, y + 7))
                    .collect();
                ReleaseDC(std::ptr::null_mut(), dc);
                for (col, actual) in [0, 3, 4, w - 1].into_iter().zip(actual) {
                    let [b, g, r, _] = color(col, 7);
                    assert_eq!(
                        actual,
                        r as u32 | ((g as u32) << 8) | ((b as u32) << 16),
                        "全透明浮层不能覆盖下方图案"
                    );
                }
            }
        }
    }
}
