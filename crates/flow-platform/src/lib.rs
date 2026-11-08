//! 平台层抽象。
//!
//! 这里定义 Engine 依赖的接口，并按 `cfg(target_os)` 选择 macOS / Windows 实现。
//!
//! 已实现：显示器枚举、键鼠捕获与注入、光标锁定、剪贴板文本（macOS、Windows），系统权限检测（macOS）。

pub mod keymap;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_input;
#[cfg(windows)]
mod windows;
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

/// 当前系统的剪贴板（纯文本）。
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
    /// 鼠标移动。`x/y` 为本机系统坐标（逻辑点）下的光标位置（macOS 为移动后、被系统限制在屏幕内的位置，
    /// Windows 为移动前的位置）；`dx/dy` 为这次移动的相对位移（逻辑点，未被屏幕边缘截断）。
    /// 光标被锁定期间 `x/y` 不再变化，只有 `dx/dy` 有意义。
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
    /// 剪贴板变化计数（macOS changeCount / Windows GetClipboardSequenceNumber），用于检测变化。
    fn change_count(&self) -> u64;
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
