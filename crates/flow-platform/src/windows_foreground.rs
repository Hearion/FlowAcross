//! Windows：现在在前台的应用，以及它的窗口是不是全屏（按应用自动锁定用）。
//!
//! 应用名取前台窗口所属进程的可执行文件名（不带 `.exe`）。全屏 = 前台窗口盖住它所在的整块显示器
//! （含任务栏的区域）；桌面和任务栏本身也铺满屏幕，要排除。

use flow_core::autolock::Foreground;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetShellWindow, GetWindowRect, GetWindowThreadProcessId,
};

/// 桌面和任务栏的窗口类名。
const SHELL_CLASSES: [&str; 4] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];

/// 可执行文件的完整路径 → 应用名（文件名去掉 `.exe`）。
fn app_name(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let lower = file.to_lowercase();
    let stem = if lower.ends_with(".exe") {
        &file[..file.len() - 4]
    } else {
        file
    };
    stem.to_owned()
}

/// 窗口是否盖住整块显示器。
fn covers(win: &RECT, monitor: &RECT) -> bool {
    win.left <= monitor.left
        && win.top <= monitor.top
        && win.right >= monitor.right
        && win.bottom >= monitor.bottom
}

fn process_path(pid: u32) -> Option<String> {
    // SAFETY: 句柄用完关闭；缓冲区和长度对应。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(handle);
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: 缓冲区和长度对应。
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

fn is_fullscreen(hwnd: HWND) -> bool {
    // SAFETY: hwnd 是刚取到的前台窗口；结构体大小已填。
    unsafe {
        if hwnd == GetShellWindow() || SHELL_CLASSES.contains(&class_name(hwnd).as_str()) {
            return false;
        }
        let mut win: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut win) == 0 {
            return false;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, &mut info) != 0 && covers(&win, &info.rcMonitor)
    }
}

/// 现在在前台的应用；没有前台窗口时返回 `None`。前台是流界自己时应用名为空（调用方据此忽略）。
pub fn foreground() -> Option<Foreground> {
    // SAFETY: 都是查询，不改任何状态。
    let (hwnd, pid) = unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        (hwnd, pid)
    };
    if pid == 0 {
        return None;
    }
    if pid == std::process::id() {
        return Some(Foreground {
            app: String::new(),
            fullscreen: false,
        });
    }
    Some(Foreground {
        // 权限更高的进程（以管理员身份运行的程序）查不到路径：应用名留空，全屏照常判断不了名单。
        app: process_path(pid).map(|p| app_name(&p)).unwrap_or_default(),
        fullscreen: is_fullscreen(hwnd),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names_drop_the_path_and_extension() {
        assert_eq!(app_name(r"C:\Program Files\Steam\steam.exe"), "steam");
        assert_eq!(app_name(r"C:\Games\Game.EXE"), "Game");
        assert_eq!(app_name("notepad"), "notepad");
        assert_eq!(app_name(r"D:\a.b\my.app.exe"), "my.app");
    }

    #[test]
    fn fullscreen_means_covering_the_whole_monitor() {
        let rect = |left, top, right, bottom| RECT {
            left,
            top,
            right,
            bottom,
        };
        let monitor = rect(0, 0, 1920, 1080);
        assert!(covers(&rect(0, 0, 1920, 1080), &monitor));
        // 无边框窗口常常比屏幕大一圈。
        assert!(covers(&rect(-8, -8, 1928, 1088), &monitor));
        // 最大化的窗口不盖任务栏。
        assert!(!covers(&rect(0, 0, 1920, 1040), &monitor));
        assert!(!covers(&rect(100, 100, 900, 700), &monitor));
    }
}
