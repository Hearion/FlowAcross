//! Windows 实现：EnumDisplayMonitors + GetDpiForMonitor 枚举显示器。
//!
//! 应用以 Per-Monitor DPI Aware v2 运行（Tauri 默认清单），`rcMonitor` 是物理像素，
//! 除以该显示器的缩放即得到逻辑点。

use flow_core::Rect;
use flow_core::protocol::DisplayInfo;
use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows_sys::core::BOOL;

use crate::{DisplayProvider, PlatformError};

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
            logical: Rect::new(
                r.left as f64 / scale,
                r.top as f64 / scale,
                (r.right - r.left) as f64 / scale,
                (r.bottom - r.top) as f64 / scale,
            ),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
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
