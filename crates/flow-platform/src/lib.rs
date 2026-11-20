//! 平台层抽象。
//!
//! 这里定义 Engine 依赖的接口，并按 `cfg(target_os)` 选择 macOS / Windows / Linux 实现。
//!
//! 已实现：显示器枚举、键鼠捕获与注入、光标锁定、剪贴板文本、文件和图片、屏幕电源与锁屏状态
//! （macOS、Windows、Linux），系统权限检测（macOS）。Linux 有三套实现：X11、wlroots 系的 Wayland
//! （Sway、Hyprland）、GNOME / KDE 的 Wayland（portal），启动时按桌面会话选一套，见 `linux.rs`。

// Windows 位图和 PNG 互转（纯字节处理）。只有 Windows 用到，但在所有平台编译，方便单元测试。
#[cfg_attr(not(windows), allow(dead_code))]
mod dib;
// 屏幕边缘浮光的形状和节奏（纯计算）。逐帧渲染只有 Windows 和 Linux 用到，同样在所有平台编译。
#[cfg_attr(
    not(any(target_os = "macos", windows, target_os = "linux")),
    allow(dead_code)
)]
mod glow;
pub mod keymap;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod linux_clipboard;
#[cfg(target_os = "linux")]
mod linux_foreground;
#[cfg(target_os = "linux")]
mod linux_glow;
#[cfg(target_os = "linux")]
mod linux_input;
#[cfg(target_os = "linux")]
mod linux_portal;
#[cfg(target_os = "linux")]
mod linux_power;
#[cfg(target_os = "linux")]
mod linux_wl;
#[cfg(target_os = "linux")]
mod linux_wl_clipboard;
#[cfg(target_os = "linux")]
mod linux_wl_glow;
#[cfg(target_os = "linux")]
mod linux_wl_power;
#[cfg(target_os = "linux")]
mod linux_wlr_input;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_awdl;
#[cfg(target_os = "macos")]
mod macos_foreground;
#[cfg(target_os = "macos")]
mod macos_glow;
#[cfg(target_os = "macos")]
mod macos_input;
#[cfg(target_os = "macos")]
mod macos_power;
// 生成以 root 运行的安装命令（纯字符串处理）。只有 macOS 用到，但在所有平台编译，方便单元测试。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod root_install;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_foreground;
#[cfg(windows)]
mod windows_glow;
#[cfg(windows)]
mod windows_input;

/// 流界自己注入的事件带这个标记，捕获时据此放行，避免把远端发来的输入又当成本机输入。
pub const INJECTED_MARK: usize = 0x464C_4F57; // "FLOW"

/// 当前系统的键鼠捕获实现。
pub fn input_capture() -> Box<dyn InputCapture> {
    #[cfg(target_os = "macos")]
    return Box::new(macos_input::MacCapture::default());
    #[cfg(windows)]
    return Box::new(windows_input::WinCapture::default());
    #[cfg(target_os = "linux")]
    return linux::input_capture();
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    return Box::new(Unsupported);
}

/// 当前系统的键鼠注入实现。
pub fn input_injector() -> Result<Box<dyn InputInjector>, PlatformError> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos_input::MacInjector::new()?));
    #[cfg(windows)]
    return Ok(Box::new(windows_input::WinInjector));
    #[cfg(target_os = "linux")]
    return linux::input_injector();
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    return Err(PlatformError::Unsupported("键鼠注入"));
}

/// 本机的大写锁定现在是否打开；查不到时返回 `None`。控制器在开始控制远端时用它告诉对方。
///
/// macOS 和 Windows 直接查系统。Linux 返回 `None`：那里在开始控制、以及控制期间状态变化时
/// 由捕获上报（[`CapturedEvent::CapsLock`]）。
pub fn caps_lock_on() -> Option<bool> {
    #[cfg(target_os = "macos")]
    return Some(macos_input::caps_lock_on());
    #[cfg(windows)]
    return Some(windows_input::caps_lock_on());
    #[cfg(not(any(target_os = "macos", windows)))]
    return None;
}

/// 当前系统的剪贴板（纯文本、文件和图片）。
pub fn clipboard() -> Result<Box<dyn ClipboardAccess>, PlatformError> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos_input::MacClipboard));
    #[cfg(windows)]
    return Ok(Box::new(windows_input::WinClipboard));
    #[cfg(target_os = "linux")]
    return linux::clipboard();
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    return Err(PlatformError::Unsupported("剪贴板"));
}

/// 控制远端期间隐藏并固定本机光标（捕获仍持续上报相对位移）；结束时恢复显示。
/// `restore_at`（本机系统坐标，逻辑点）：开始时是离开处（Windows 把光标锁在这里，macOS 和 Linux 忽略），
/// 结束时是光标要放回的位置。Linux 上同时抓住键盘和鼠标，本机程序在此期间收不到输入。
pub fn set_cursor_captured(captured: bool, restore_at: Option<(f64, f64)>) {
    #[cfg(target_os = "macos")]
    macos_input::set_cursor_captured(captured, restore_at);
    #[cfg(windows)]
    windows_input::set_cursor_captured(captured, restore_at);
    #[cfg(target_os = "linux")]
    linux::set_cursor_captured(captured, restore_at);
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    let _ = (captured, restore_at);
}

/// 告诉平台层哪些屏幕边通往别的电脑：(本机显示器, 边)。布局或已连接的设备变化时由引擎调用。
///
/// 只有 Linux 的 Wayland 用得到：那里看不到全局的鼠标移动，只能在屏幕边上放一条很窄的窗口来感知
/// "光标推到了边上"，而这条窗口会挡住最外一圈像素的点击，所以只放在确实能切过去的边上。其他系统无操作。
pub fn set_switch_edges(edges: &[(String, flow_core::geometry::Edge)]) {
    #[cfg(target_os = "linux")]
    linux::set_switch_edges(edges);
    #[cfg(not(target_os = "linux"))]
    let _ = edges;
}

/// 键鼠从别的设备来到本机时，在它进来的那条屏幕边上闪一道浮光（约 0.75 秒，不抢焦点、鼠标可穿透）。
/// `display_local_id`：本机显示器；`along`：进入点在这条边上的位置，0..1（左右边从上往下，上下边从左往右）。
/// macOS 上必须在主线程调用（其他线程调用时忽略）；Windows 和 Linux 上任意线程都可以，动画在单独的线程里播。
/// Linux 上需要桌面开着窗口合成（现在的桌面环境默认都开），没有时不显示。
pub fn edge_glow(display_local_id: &str, edge: flow_core::geometry::Edge, along: f64) {
    #[cfg(target_os = "macos")]
    macos_glow::show(display_local_id, edge, along);
    #[cfg(windows)]
    windows_glow::show(display_local_id, edge, along);
    #[cfg(target_os = "linux")]
    linux::edge_glow(display_local_id, edge, along);
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    let _ = (display_local_id, edge, along);
}

/// 当前系统的显示器枚举实现。
pub fn display_provider() -> Box<dyn DisplayProvider> {
    #[cfg(target_os = "macos")]
    return Box::new(macos::MacDisplays);
    #[cfg(windows)]
    return Box::new(windows::WinDisplays);
    #[cfg(target_os = "linux")]
    return Box::new(linux::LinuxDisplays);
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    return Box::new(Unsupported);
}

/// 刷新显示器名称（macOS 需要在主线程调用，其他线程调用时忽略；其他系统无操作）。
pub fn refresh_display_names() {
    #[cfg(target_os = "macos")]
    macos::refresh_display_names();
}

/// 告诉系统流界是延迟敏感的后台程序，不要降速（启动时调用一次）。
///
/// 流界大部分时间窗口隐藏、在后台运行；macOS 的 App Nap 和 Windows 11 的电源节流（EcoQoS）
/// 会降低后台程序的调度优先级、合并定时器，注入键鼠就会卡顿、有延迟。
pub fn keep_responsive() {
    #[cfg(target_os = "macos")]
    macos::keep_responsive();
    #[cfg(windows)]
    windows::keep_responsive();
}

/// 当前系统（桌面会话）里做不到的切换方式和效果。`true` 表示用不了：引擎据此忽略对应的设置，
/// 界面不显示它们，免得用户打开后发现切不过去。
///
/// 只有 Linux 的 Wayland 会有：那里的普通程序看不到全局键盘，见 `linux_wlr_input.rs`、`linux_portal.rs`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// 「按住修饰键才切换」：没切到远端时看不到键盘，不知道修饰键有没有按着。
    pub hold_key: bool,
    /// 在本机按 Ctrl+Alt+←/→ 切到别的电脑（切过去之后用它切回来不受影响）。
    pub local_hotkey: bool,
    /// 「停留一会儿才切换」：光标一碰到屏幕边系统就开始捕获，等不了。
    pub dwell: bool,
    /// 边缘浮光。
    pub edge_glow: bool,
    /// 看不到本机平时的键鼠输入（只在光标碰到屏幕边、或正在控制远端时才有事件）。
    /// 引擎靠"本机有输入了"判断键鼠回到了这台电脑，看不到时就不做这种推断。
    pub local_input: bool,
    /// 不把鼠标推过屏幕边、直接把键鼠切到别的电脑（托盘菜单、命令）：GNOME / KDE 的 Wayland 上
    /// 只有光标撞上屏幕边的屏障时合成器才肯把键鼠交出来。
    pub commanded_switch: bool,
    /// 查不到前台是哪个应用、是不是全屏（[`foreground`]）：Wayland 不让普通程序看别的窗口，
    /// 「全屏时锁定在本机」和按应用锁定用不了。
    pub foreground_app: bool,
}

/// 当前系统的限制，见 [`Limits`]。
pub fn limits() -> Limits {
    #[cfg(target_os = "linux")]
    return linux::limits();
    #[cfg(not(target_os = "linux"))]
    return Limits::default();
}

/// 现在在前台的应用，以及它的窗口是不是全屏（`flow_core::autolock` 用来按应用自动锁定在本机）。
/// 查不到时返回 `None`：没有前台窗口，或这个桌面查不了（Linux 的 Wayland，见 [`Limits::foreground_app`]）。
/// 前台是流界自己的窗口时应用名为空。macOS 上要问一次对方应用（最多等四分之一秒），Linux 上要连
/// 一次 X 服务，放到阻塞线程里调用，不要查得太频繁。
pub fn foreground() -> Option<flow_core::autolock::Foreground> {
    #[cfg(target_os = "macos")]
    return macos_foreground::foreground();
    #[cfg(windows)]
    return windows_foreground::foreground();
    #[cfg(target_os = "linux")]
    return linux_foreground::foreground();
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    return None;
}

/// 系统的授权弹窗是为哪件事弹的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// 捕获这台电脑的键鼠（把键鼠切到别的电脑要用）。
    Capture,
    /// 控制这台电脑的键鼠（别的电脑的键鼠过来操作它要用）。
    Inject,
}

/// 一个正在等用户点的系统授权弹窗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthPrompt {
    pub kind: AuthKind,
    /// 还剩多少秒；到时间没人点，流界会把弹窗关掉，下次需要时再问。
    pub seconds_left: u32,
}

/// 现在有没有系统的授权弹窗在等用户点。界面据此显示倒计时。
///
/// 只有 Linux 的 GNOME / KDE（Wayland，走 portal）会有：那里捕获和注入键鼠都要用户在系统弹窗里
/// 同意，流界给每次申请限时一分钟。其他系统的授权在系统设置里做，这里一直是 `None`。
pub fn pending_authorization() -> Option<AuthPrompt> {
    #[cfg(target_os = "linux")]
    return linux::pending_authorization();
    #[cfg(not(target_os = "linux"))]
    return None;
}

/// 菜单栏（macOS 的状态栏）上流界图标旁边的文字。只有 macOS 支持。
pub mod menu_bar {
    use flow_core::autolock::LatencyLevel;

    /// 当前系统是否支持。
    pub fn supported() -> bool {
        cfg!(target_os = "macos")
    }

    /// 菜单栏图标默认的提示文字（鼠标停在图标上时显示）。
    pub const DEFAULT_TOOLTIP: &str = "流界 FlowAcross";

    /// 各档的颜色（和 macOS 深色外观下的系统绿、橙、红一致，在深浅两种菜单栏上都看得清）。
    pub fn level_rgb(level: LatencyLevel) -> [u8; 3] {
        match level {
            LatencyLevel::Good => [48, 209, 88],
            LatencyLevel::Fair => [255, 159, 10],
            LatencyLevel::Poor => [255, 69, 58],
        }
    }

    /// 把一张单色图标（模板图片：只看透明度）整个涂成 `rgb`，透明度不变。读不出来时返回 `None`。
    pub fn tinted_png(png_data: &[u8], rgb: [u8; 3]) -> Option<Vec<u8>> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png_data));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        let channels = match info.color_type {
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgba => 4,
            // 没有透明通道的图标涂色之后就是一整块方形，不是想要的。
            _ => return None,
        };
        let pixels = info.width as usize * info.height as usize;
        let mut out = Vec::with_capacity(pixels * 4);
        for px in buf[..pixels * channels].chunks_exact(channels) {
            out.extend_from_slice(&rgb);
            out.push(px[channels - 1]);
        }
        let mut encoded = Vec::new();
        let mut encoder = png::Encoder::new(&mut encoded, info.width, info.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().ok()?.write_image_data(&out).ok()?;
        Some(encoded)
    }

    /// 按网络状况换 `status_item`（一个 `NSStatusItem` 的地址）的图标：`status` 有值时把
    /// `template_png` 涂成该档的颜色（[`level_rgb`]）显示，鼠标停在图标上时提示 `tooltip`；
    /// `None` 换回 `template_png` 本身作为模板图片，由系统按菜单栏深浅画成黑或白，提示恢复成
    /// [`DEFAULT_TOOLTIP`]。
    ///
    /// # Safety
    ///
    /// 必须在主线程上调用；`status_item` 必须指向一个还活着的 `NSStatusItem`。
    pub unsafe fn set_status(
        status_item: usize,
        status: Option<(&str, LatencyLevel)>,
        template_png: &[u8],
    ) {
        #[cfg(target_os = "macos")]
        // SAFETY: 由调用方保证。
        unsafe {
            crate::macos::set_status_icon(status_item, status, template_png)
        };
        #[cfg(not(target_os = "macos"))]
        let _ = (status_item, status, template_png);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn encode(width: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, width, 1);
            e.set_color(color);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(data).unwrap();
            out
        }

        fn decode(data: &[u8]) -> Vec<u8> {
            let mut r = png::Decoder::new(std::io::Cursor::new(data))
                .read_info()
                .unwrap();
            let mut buf = vec![0; r.output_buffer_size().unwrap()];
            let n = r.next_frame(&mut buf).unwrap().buffer_size();
            buf.truncate(n);
            buf
        }

        #[test]
        fn tinting_keeps_the_shape_and_changes_the_color() {
            let green = level_rgb(LatencyLevel::Good);
            // 黑色不透明、半透明、全透明三个像素。
            let src = encode(
                3,
                png::ColorType::Rgba,
                &[0, 0, 0, 255, 0, 0, 0, 128, 0, 0, 0, 0],
            );
            let out = decode(&tinted_png(&src, green).unwrap());
            assert_eq!(
                out,
                [48, 209, 88, 255, 48, 209, 88, 128, 48, 209, 88, 0].to_vec()
            );
            // 灰度加透明度的图标也行。
            let src = encode(2, png::ColorType::GrayscaleAlpha, &[0, 200, 0, 0]);
            let out = decode(&tinted_png(&src, [1, 2, 3]).unwrap());
            assert_eq!(out, [1, 2, 3, 200, 1, 2, 3, 0].to_vec());
            // 没有透明通道、或者不是 PNG：不涂。
            let src = encode(1, png::ColorType::Rgb, &[0, 0, 0]);
            assert!(tinted_png(&src, green).is_none());
            assert!(tinted_png(b"not a png", green).is_none());
        }

        #[test]
        fn the_real_menu_bar_icon_can_be_tinted() {
            let icon = include_bytes!("../../../app/src-tauri/icons/tray-template.png");
            for level in [LatencyLevel::Good, LatencyLevel::Fair, LatencyLevel::Poor] {
                let out = decode(&tinted_png(icon, level_rgb(level)).unwrap());
                let rgb = level_rgb(level);
                // 不透明的像素都是这个颜色，而且确实有不透明的像素。
                let opaque: Vec<&[u8]> = out.chunks_exact(4).filter(|p| p[3] > 0).collect();
                assert!(!opaque.is_empty());
                assert!(opaque.iter().all(|p| p[..3] == rgb));
            }
        }
    }
}

/// 低延迟模式：跨屏期间关闭 Mac 的 AWDL，消除 Wi‑Fi 周期性停顿造成的卡顿。只有 macOS 支持。
pub mod low_latency {
    use crate::PlatformError;

    /// 当前系统是否支持。
    pub fn supported() -> bool {
        cfg!(target_os = "macos")
    }

    /// 后台服务是否已安装。
    pub fn installed() -> bool {
        #[cfg(target_os = "macos")]
        return crate::macos_awdl::installed();
        #[cfg(not(target_os = "macos"))]
        return false;
    }

    /// 安装后台服务（会弹出管理员授权窗口，阻塞到用户操作完）。
    pub fn install() -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::macos_awdl::install();
        #[cfg(not(target_os = "macos"))]
        return Err(PlatformError::Unsupported("低延迟模式"));
    }

    /// 移除后台服务并恢复 AWDL（会弹出管理员授权窗口）。
    pub fn uninstall() -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::macos_awdl::uninstall();
        #[cfg(not(target_os = "macos"))]
        return Err(PlatformError::Unsupported("低延迟模式"));
    }

    /// 通知后台服务现在是否需要关闭 AWDL。通知不到时返回错误（这次不会生效）。
    pub fn set_active(active: bool) -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::macos_awdl::set_active(active);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = active;
            Ok(())
        }
    }
}

/// 屏幕电源：点亮屏幕、不让屏幕自动熄灭、查询屏幕是否熄着。
pub mod power {
    use crate::PlatformError;

    /// 点亮本机已经熄灭的屏幕，并重置自动熄屏的计时。
    ///
    /// Linux 的 Wayland 上，Sway、Hyprland 用合成器的屏幕电源协议直接打开；GNOME、KDE 没有
    /// 这类接口，返回 [`PlatformError::Unsupported`]，调用方可以改为注入一次鼠标移动（合成器把它
    /// 当作用户操作）。锁屏不归这里管，点亮后仍停在锁屏界面。
    pub fn wake_display() -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::macos_power::wake_display();
        #[cfg(windows)]
        return crate::windows::wake_display();
        #[cfg(target_os = "linux")]
        return crate::linux_power::wake_display();
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        return Err(PlatformError::Unsupported("点亮屏幕"));
    }

    /// 别的电脑的键鼠正在操作本机时调用（调用方负责限制频率）：让系统把它当作用户操作，
    /// 熄着的屏幕会亮，自动熄屏的计时重新开始。
    ///
    /// 只有 macOS 需要：它不把 `CGEventPost` 注入的事件算作用户操作。Windows 的 `SendInput`、
    /// X11 的 XTest、Wayland 的虚拟指针 / 键盘在系统看来就是输入设备，本身会点亮屏幕，无操作。
    pub fn input_activity() -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::macos_power::wake_display();
        #[cfg(not(target_os = "macos"))]
        return Ok(());
    }

    /// 开启或关闭"不让屏幕自动熄灭"（不影响用户自己锁屏、合盖）。重复调用无副作用。
    /// macOS 用电源断言，Windows 用线程执行状态，Linux 向桌面环境的屏幕保护服务登记
    /// （没有这个服务的桌面会返回错误）。
    pub fn set_keep_awake(on: bool) -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::macos_power::set_keep_awake(on);
        #[cfg(windows)]
        return crate::windows::set_keep_awake(on);
        #[cfg(target_os = "linux")]
        return crate::linux_power::set_keep_awake(on);
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        {
            let _ = on;
            Err(PlatformError::Unsupported("保持屏幕常亮"))
        }
    }

    /// 这台电脑上做不到"保持屏幕常亮"的原因（给用户看的话）；做得到时返回 `None`。
    /// macOS、Windows 一定做得到。Linux 要问会话总线上有没有屏幕保护服务，会阻塞几毫秒，
    /// 不要在界面线程上频繁调用。
    pub fn keep_awake_problem() -> Option<String> {
        #[cfg(any(target_os = "macos", windows))]
        return None;
        #[cfg(target_os = "linux")]
        return crate::linux_power::keep_awake_problem();
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        return Some("这个系统不支持保持屏幕常亮。".into());
    }

    /// 这台电脑现在是不是停在锁屏界面；查不到时返回 `None`。引擎定时查询，变化时告诉其他电脑，
    /// 它们据此提示"需要在那台电脑上解锁"。
    ///
    /// macOS 查当前会话的状态，Windows 查终端服务会话的锁定标志，Linux 查 logind 会话的
    /// `LockedHint`（GNOME、KDE 会设置；Sway、Hyprland 上要看锁屏程序是否通过 logind 上锁，
    /// 没设置时一直是"没锁"）。Linux 上每次查询要连一次系统总线，不要查得太频繁。
    pub fn screen_locked() -> Option<bool> {
        #[cfg(target_os = "macos")]
        return crate::macos_power::screen_locked();
        #[cfg(windows)]
        return crate::windows::screen_locked();
        #[cfg(target_os = "linux")]
        return crate::linux_power::screen_locked();
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        return None;
    }

    /// 屏幕现在是不是熄着。熄屏期间枚举到的显示器不可信（见
    /// `flow_core::protocol::settle_displays`）。只有 macOS 和 Linux 的 X11 查得到，
    /// 其余返回 `false`。
    pub fn displays_asleep() -> bool {
        #[cfg(target_os = "macos")]
        return crate::macos_power::displays_asleep();
        #[cfg(target_os = "linux")]
        return crate::linux_power::displays_asleep();
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return false;
    }
}

/// 当前系统的权限检测实现。
pub fn permissions() -> Box<dyn Permissions> {
    #[cfg(target_os = "macos")]
    return Box::new(macos::MacPermissions);
    #[cfg(not(target_os = "macos"))]
    return Box::new(Unsupported);
}

/// 当前系统需要用户授予的权限。
pub fn required_permissions() -> &'static [Permission] {
    if cfg!(target_os = "macos") {
        &[Permission::Accessibility, Permission::InputMonitoring]
    } else {
        &[]
    }
}

/// 尚未支持的系统，或不需要某项能力的系统。
#[allow(dead_code)]
struct Unsupported;

impl DisplayProvider for Unsupported {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        Err(PlatformError::Unsupported("显示器枚举"))
    }
}

impl InputCapture for Unsupported {
    fn start(&mut self, _: CaptureHandler) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("键鼠捕获"))
    }
    fn stop(&mut self) {}
}

impl Permissions for Unsupported {
    fn state(&self, _: Permission) -> PermissionState {
        PermissionState::NotApplicable
    }
    fn request(&self, _: Permission) {}
    fn open_settings(&self, _: Permission) -> Result<(), PlatformError> {
        Ok(())
    }
}

use flow_core::keys::{KeyCode, MouseButton};
use flow_core::protocol::DisplayInfo;
use thiserror::Error;

/// 从本机捕获到的原始输入。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CapturedEvent {
    /// 鼠标移动。`x/y` 为本机系统坐标（逻辑点）下**移动前**的光标位置；`dx/dy` 为这次移动的相对位移
    /// （逻辑点，未被屏幕边缘截断）。`x + dx`、`y + dy` 可能落在屏幕外，表示光标被推出了边缘。
    /// 两个系统含义一致：Windows 钩子在系统移动光标前调用，直接取当前光标；macOS 事件带的是移动后、
    /// 被限制在屏幕内的位置，减去位移后上报（贴边时算出的起点偏远，但 `x + dx` 仍是光标实际所在）。
    /// 光标被锁定期间 `x/y` 没有意义，只有 `dx/dy` 有意义。
    MouseMove {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// 单位：1/120 格。`dy > 0` 向上，`dx > 0` 向右（与 Windows 一致）。
    Wheel {
        dx: i32,
        dy: i32,
    },
    /// `repeat` 为系统产生的长按连发。
    Key {
        code: KeyCode,
        down: bool,
        repeat: bool,
    },
    /// 本机的大写锁定现在是开还是关。开始控制远端时、以及控制期间状态变化时上报，
    /// 控制器转告对方，让两边一致。只有 Linux（X11、Hyprland / Sway）用这个事件；
    /// macOS 和 Windows 由控制器在开始控制时直接查询（[`caps_lock_on`]）。
    CapsLock(bool),
}

/// 捕获回调对每个事件的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 交给本机系统正常处理。
    Pass,
    /// 拦截，本机系统看不到这个事件。
    Suppress,
}

/// 捕获回调。在系统钩子线程上同步调用，必须尽快返回（不能做网络 IO，只能投递到通道）。
pub type CaptureHandler = Box<dyn FnMut(CapturedEvent) -> Verdict + Send + 'static>;

/// 全局键鼠捕获。在独立线程上运行系统钩子，直到 `stop` 或对象被丢弃。
pub trait InputCapture: Send {
    fn start(&mut self, handler: CaptureHandler) -> Result<(), PlatformError>;
    fn stop(&mut self);
    /// 启动之后捕获自己停了的原因；还在运行（或者是被 `stop` 停的）返回 `None`。
    ///
    /// 只有 Linux 的 portal 实现会报：`start` 等几秒就返回了，授权弹窗可能之后才被拒绝、到时间
    /// 自动关闭，或者桌面后来把权限收回。引擎定时查，查到就把它卸掉并告诉用户。
    fn failure(&self) -> Option<PlatformError> {
        None
    }
}

pub trait InputInjector: Send {
    /// 移动到指定显示器内的逻辑坐标（以该显示器左上角为原点）。
    fn move_to(&mut self, display_local_id: &str, x: f64, y: f64) -> Result<(), PlatformError>;
    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError>;
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), PlatformError>;
    fn key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Result<(), PlatformError>;
    /// 把大写锁定对齐到控制方的状态（见 [`CapturedEvent::CapsLock`]）。已经一致时什么都不做。
    /// 默认无操作：Linux 的 GNOME / KDE（portal）还没有实现。
    fn set_caps_lock(&mut self, on: bool) -> Result<(), PlatformError> {
        let _ = on;
        Ok(())
    }
}

pub trait DisplayProvider: Send + Sync {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError>;
}

pub trait ClipboardAccess: Send {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError>;
    fn set_text(&mut self, text: &str) -> Result<(), PlatformError>;
    /// 在 Finder / 资源管理器里复制的文件和文件夹（绝对路径）；剪贴板里不是文件时返回 `None`。
    fn get_files(&mut self) -> Result<Option<Vec<std::path::PathBuf>>, PlatformError>;
    /// 把文件放进剪贴板（替换原有内容），之后可以在 Finder / 资源管理器里粘贴。
    fn set_files(&mut self, paths: &[std::path::PathBuf]) -> Result<(), PlatformError>;
    /// 剪贴板里的图片（截图、在浏览器或看图软件里复制的图片），统一转成 PNG；没有图片时返回 `None`。
    /// 大图转换要几十到几百毫秒，不要在持有引擎状态锁时调用。
    fn get_image(&mut self) -> Result<Option<Vec<u8>>, PlatformError>;
    /// 把一张图片放进剪贴板（替换原有内容），之后可以粘贴进聊天窗口、文档和画图软件。
    fn set_image(&mut self, image: &ClipboardImage) -> Result<(), PlatformError>;
    /// 剪贴板变化计数（macOS changeCount / Windows GetClipboardSequenceNumber），用于检测变化。
    fn change_count(&self) -> u64;
}

/// 准备好写入剪贴板的图片。Windows 上要先把 PNG 解成位图，大图要几十到几百毫秒：
/// 先在不持锁的地方用 [`ClipboardImage::from_png`] 准备好，[`ClipboardAccess::set_image`] 只管写入。
pub struct ClipboardImage {
    png: Vec<u8>,
    /// `CF_DIB` 数据；解不出来时只写 PNG。
    #[cfg(windows)]
    dib: Option<Vec<u8>>,
}

impl ClipboardImage {
    /// 不是 PNG 时返回 `None`。
    pub fn from_png(png: Vec<u8>) -> Option<Self> {
        if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return None;
        }
        Some(Self {
            #[cfg(windows)]
            dib: dib::png_to_dib(&png),
            png,
        })
    }

    pub fn png(&self) -> &[u8] {
        &self.png
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// macOS 辅助功能：注入与拦截键鼠。
    Accessibility,
    /// macOS 输入监控：捕获键盘。
    InputMonitoring,
    /// macOS 本地网络：mDNS 与局域网连接。
    LocalNetwork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    NotDetermined,
    /// 当前系统不需要这项权限（如 Windows 上的辅助功能）。
    NotApplicable,
}

pub trait Permissions: Send + Sync {
    fn state(&self, permission: Permission) -> PermissionState;
    /// 触发系统授权弹窗（如果系统支持）。
    fn request(&self, permission: Permission);
    /// 打开系统设置中对应的权限页面。
    fn open_settings(&self, permission: Permission) -> Result<(), PlatformError>;
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("缺少系统权限：{0:?}")]
    PermissionDenied(Permission),
    #[error("当前系统暂不支持：{0}")]
    Unsupported(&'static str),
    #[error("系统调用失败：{0}")]
    Os(String),
    /// 没有拿到用户的授权：在系统的授权弹窗里选了拒绝、到时间没选，或者系统后来把权限收回了。
    /// 和别的失败不同，这种等"下次需要时"重新申请就行，不用隔一阵自动重试。
    #[error("没有拿到授权：{0}")]
    Denied(String),
}
