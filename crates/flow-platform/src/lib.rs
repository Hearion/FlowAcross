//! 平台层抽象。
//!
//! 这里定义 Engine 依赖的接口，并按 `cfg(target_os)` 选择 macOS / Windows 实现。
//!
//! 已实现：显示器枚举、键鼠捕获与注入、光标锁定、剪贴板文本、文件和图片（macOS、Windows），系统权限检测（macOS）。

// Windows 位图和 PNG 互转（纯字节处理）。只有 Windows 用到，但在所有平台编译，方便单元测试。
#[cfg_attr(not(windows), allow(dead_code))]
mod dib;
// 屏幕边缘浮光的形状和节奏（纯计算）。逐帧渲染只有 Windows 用到，同样在所有平台编译。
mod glow;
pub mod keymap;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_awdl;
#[cfg(target_os = "macos")]
mod macos_glow;
#[cfg(target_os = "macos")]
mod macos_input;
// 生成以 root 运行的安装命令（纯字符串处理）。只有 macOS 用到，但在所有平台编译，方便单元测试。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod root_install;
#[cfg(windows)]
mod windows;
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
    #[cfg(not(any(target_os = "macos", windows)))]
    return Box::new(Unsupported);
}

/// 当前系统的键鼠注入实现。
pub fn input_injector() -> Result<Box<dyn InputInjector>, PlatformError> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos_input::MacInjector::new()?));
    #[cfg(windows)]
    return Ok(Box::new(windows_input::WinInjector));
    #[cfg(not(any(target_os = "macos", windows)))]
    return Err(PlatformError::Unsupported("键鼠注入"));
}

/// 当前系统的剪贴板（纯文本、文件和图片）。
pub fn clipboard() -> Result<Box<dyn ClipboardAccess>, PlatformError> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos_input::MacClipboard));
    #[cfg(windows)]
    return Ok(Box::new(windows_input::WinClipboard));
    #[cfg(not(any(target_os = "macos", windows)))]
    return Err(PlatformError::Unsupported("剪贴板"));
}

/// 控制远端期间隐藏并固定本机光标（捕获仍持续上报相对位移）；结束时恢复显示。
/// `restore_at`（本机系统坐标，逻辑点）：开始时是离开处（Windows 把光标锁在这里，macOS 忽略），
/// 结束时是光标要放回的位置。
pub fn set_cursor_captured(captured: bool, restore_at: Option<(f64, f64)>) {
    #[cfg(target_os = "macos")]
    macos_input::set_cursor_captured(captured, restore_at);
    #[cfg(windows)]
    windows_input::set_cursor_captured(captured, restore_at);
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = (captured, restore_at);
}

/// 键鼠从别的设备来到本机时，在它进来的那条屏幕边上闪一道浮光（约 0.75 秒，不抢焦点、鼠标可穿透）。
/// `display_local_id`：本机显示器；`along`：进入点在这条边上的位置，0..1（左右边从上往下，上下边从左往右）。
/// macOS 上必须在主线程调用（其他线程调用时忽略）；Windows 上任意线程都可以，动画在单独的线程里播。
pub fn edge_glow(display_local_id: &str, edge: flow_core::geometry::Edge, along: f64) {
    #[cfg(target_os = "macos")]
    macos_glow::show(display_local_id, edge, along);
    #[cfg(windows)]
    windows_glow::show(display_local_id, edge, along);
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = (display_local_id, edge, along);
}

/// 当前系统的显示器枚举实现。
pub fn display_provider() -> Box<dyn DisplayProvider> {
    #[cfg(target_os = "macos")]
    return Box::new(macos::MacDisplays);
    #[cfg(windows)]
    return Box::new(windows::WinDisplays);
    #[cfg(not(any(target_os = "macos", windows)))]
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

    /// 通知后台服务现在是否需要关闭 AWDL。
    pub fn set_active(active: bool) {
        #[cfg(target_os = "macos")]
        crate::macos_awdl::set_active(active);
        #[cfg(not(target_os = "macos"))]
        let _ = active;
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
}

pub trait InputInjector: Send {
    /// 移动到指定显示器内的逻辑坐标（以该显示器左上角为原点）。
    fn move_to(&mut self, display_local_id: &str, x: f64, y: f64) -> Result<(), PlatformError>;
    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError>;
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), PlatformError>;
    fn key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Result<(), PlatformError>;
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
}
