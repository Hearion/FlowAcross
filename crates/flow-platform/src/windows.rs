//! Windows 实现：EnumDisplayMonitors + GetDpiForMonitor 枚举显示器。
//!
//! 应用以 Per-Monitor DPI Aware v2 运行（Tauri 默认清单），`rcMonitor` 是物理像素。
//! 尺寸除以该显示器的缩放得到逻辑尺寸；位置由 `flow_core::layout::logical_arrangement`
//! 按相贴关系排出来，缩放不同的屏幕在逻辑空间里仍然贴在一起。

use flow_core::Rect;
use flow_core::layout::{PhysDisplay, logical_arrangement};
use flow_core::protocol::DisplayInfo;
use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows_sys::core::BOOL;

use crate::{DisplayProvider, PlatformError};

/// 关闭本进程的电源节流（EcoQoS）。Windows 11 会给窗口隐藏的后台进程降速、合并定时器，
/// 注入键鼠和发送鼠标位置就会卡顿。
pub fn keep_responsive() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        // 控制这两项，状态全部置 0 = 不节流。
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // SAFETY: 传入本进程伪句柄和正确大小的结构体；旧系统不支持时调用失败，忽略即可。
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

pub struct WinDisplays;

unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    // SAFETY: data 是 displays() 中传入的 Vec<HMONITOR> 指针，在 EnumDisplayMonitors 返回前有效。
    let list = unsafe { &mut *(data as *mut Vec<HMONITOR>) };
    list.push(monitor);
    1
}

/// 一块显示器的物理像素矩形与逻辑坐标矩形。键鼠捕获和注入用它在两种坐标之间换算。
#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    pub id: String,
    pub index: usize,
    /// 物理像素：left, top, right, bottom。
    pub phys: (i32, i32, i32, i32),
    pub scale: f64,
    pub logical: Rect,
    pub primary: bool,
}

impl Monitor {
    pub fn contains_phys(&self, x: i32, y: i32) -> bool {
        x >= self.phys.0 && x < self.phys.2 && y >= self.phys.1 && y < self.phys.3
    }

    pub fn phys_to_logical(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.logical.x + (x - self.phys.0 as f64) / self.scale,
            self.logical.y + (y - self.phys.1 as f64) / self.scale,
        )
    }

    pub fn logical_to_phys(&self, x: f64, y: f64) -> (i32, i32) {
        let px = self.phys.0 as f64 + (x - self.logical.x) * self.scale;
        let py = self.phys.1 as f64 + (y - self.logical.y) * self.scale;
        (
            (px.round() as i32).clamp(self.phys.0, self.phys.2 - 1),
            (py.round() as i32).clamp(self.phys.1, self.phys.3 - 1),
        )
    }
}

pub(crate) fn monitors() -> Result<Vec<Monitor>, PlatformError> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: 回调只在本次调用期间执行，handles 一直有效。
    let ok = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            &mut handles as *mut Vec<HMONITOR> as LPARAM,
        )
    };
    if ok == 0 {
        return Err(PlatformError::Os("EnumDisplayMonitors 失败".into()));
    }

    let mut out = Vec::new();
    for (i, m) in handles.into_iter().enumerate() {
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: info 大小已正确设置，GetMonitorInfoW 按 cbSize 写入。
        if unsafe { GetMonitorInfoW(m, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) } == 0
        {
            continue;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        // SAFETY: 输出指针指向本地变量。
        unsafe { GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
        let scale = (dx.max(96) as f64) / 96.0;
        let r = info.monitorInfo.rcMonitor;
        let device = String::from_utf16_lossy(&info.szDevice);
        out.push(Monitor {
            id: device.trim_end_matches('\0').to_owned(),
            index: i,
            phys: (r.left, r.top, r.right, r.bottom),
            scale,
            // 先占位，下面统一排布。
            logical: Rect::new(0.0, 0.0, 0.0, 0.0),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    let phys: Vec<PhysDisplay> = out
        .iter()
        .map(|m| PhysDisplay {
            rect: m.phys,
            scale: m.scale,
            primary: m.primary,
        })
        .collect();
    for (m, logical) in out.iter_mut().zip(logical_arrangement(&phys)) {
        m.logical = logical;
    }
    Ok(out)
}

impl DisplayProvider for WinDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        let list = monitors()?;
        crate::windows_input::update_monitors(list.clone());
        Ok(list
            .into_iter()
            .map(|m| DisplayInfo {
                name: format!("显示器 {}", m.index + 1),
                native: Some(((m.phys.2 - m.phys.0) as u32, (m.phys.3 - m.phys.1) as u32)),
                local_id: m.id,
                rect: m.logical,
                scale: m.scale,
                primary: m.primary,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// 屏幕电源
// ---------------------------------------------------------------------------

/// 点亮屏幕：Windows 只在有输入时打开已经关掉的显示器，所以注入一次来回 1 像素的鼠标移动
/// （光标不动），再重置自动熄屏的计时。
pub fn wake_display() -> Result<(), crate::PlatformError> {
    use windows_sys::Win32::System::Power::{
        ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE;

    use crate::windows_input::{mouse_input, send};
    // SAFETY: 只传标志位。不带 ES_CONTINUOUS 时只重置一次计时，不留下状态。
    unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };
    send(&[
        mouse_input(1, 0, 0, MOUSEEVENTF_MOVE),
        mouse_input(-1, 0, 0, MOUSEEVENTF_MOVE),
    ])
}

/// 正在阻止熄屏时，结束它用的发送端：丢掉它，持有状态的线程就恢复默认并退出。
static KEEP_AWAKE: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>> =
    std::sync::Mutex::new(None);

/// 开启或关闭"不让屏幕自动熄灭"。`SetThreadExecutionState` 的持续状态跟着线程走，
/// 所以专门留一个线程拿着它，关闭时由同一个线程恢复。
pub fn set_keep_awake(on: bool) -> Result<(), crate::PlatformError> {
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    let mut held = KEEP_AWAKE.lock().expect("keep awake");
    if !on {
        held.take();
        return Ok(());
    }
    if held.is_some() {
        return Ok(());
    }
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("flow-keep-awake".into())
        .spawn(move || {
            // SAFETY: 只传标志位。
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED)
            };
            // 发送端被丢掉时返回。
            let _ = rx.recv();
            // SAFETY: 同上；只留 ES_CONTINUOUS 表示恢复默认。
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        })
        .map_err(|e| crate::PlatformError::Os(format!("无法启动线程：{e}")))?;
    *held = Some(tx);
    Ok(())
}
