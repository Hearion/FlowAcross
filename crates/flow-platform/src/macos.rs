//! macOS 实现：CoreGraphics 枚举显示器，ApplicationServices / CoreGraphics 检测权限。

use std::collections::HashMap;
use std::ffi::c_void;
use std::process::Command;
use std::sync::Mutex;

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::{CGDisplay, kDisplayModeNativeFlag};
use flow_core::Rect;
use flow_core::protocol::DisplayInfo;
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSNumber, NSString};

use crate::{DisplayProvider, Permission, PermissionState, Permissions, PlatformError};

/// 显示器名称缓存（CGDirectDisplayID → 系统显示的名称，如"H27P22"）。
/// 名称只能通过 AppKit 的 NSScreen 在主线程读取，后台线程枚举显示器时使用缓存。
static NAMES: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);

/// 登记一个贯穿整个进程的"用户发起、延迟敏感"活动，系统就不会对流界启用 App Nap
/// （后台降速、合并定时器会让注入的键鼠卡顿）。Info.plist 里也声明了 LSAppNapIsDisabled。
pub fn keep_responsive() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo};
    let reason = NSString::from_str("流界在电脑之间实时传递键盘和鼠标");
    let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::LatencyCritical,
        &reason,
    );
    // 活动在 token 释放时结束，进程存活期间一直保留。
    std::mem::forget(token);
}

/// 在主线程刷新显示器名称缓存；不在主线程时什么也不做。
/// 应用启动时和定期（经由主线程调度）调用。
pub fn refresh_display_names() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let key = NSString::from_str("NSScreenNumber");
    let mut names = HashMap::new();
    for screen in NSScreen::screens(mtm).iter() {
        let desc = screen.deviceDescription();
        let Some(num) = desc
            .objectForKey(&key)
            .and_then(|o| o.downcast::<NSNumber>().ok())
        else {
            continue;
        };
        names.insert(num.unsignedIntValue(), screen.localizedName().to_string());
    }
    *NAMES.lock().expect("names") = Some(names);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCopyAllDisplayModes(display: u32, options: *const c_void) -> *const c_void;
    fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
    fn CGDisplayModeGetIOFlags(mode: *const c_void) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// 面板原生分辨率：带"原生"标记的显示模式的像素尺寸。
///
/// 直接调用 CoreGraphics：core-graphics 库的 `all_display_modes` 会把数组中借用的元素
/// 当作自有对象释放，导致重复释放崩溃。
fn native_resolution(id: u32) -> Option<(u32, u32)> {
    // SAFETY: Copy 规则返回 +1 的数组，用完 CFRelease；数组元素按 Get 规则借用，不释放。
    unsafe {
        let modes = CGDisplayCopyAllDisplayModes(id, std::ptr::null());
        if modes.is_null() {
            return None;
        }
        let mut best: Option<(u32, u32)> = None;
        for i in 0..CFArrayGetCount(modes) {
            let m = CFArrayGetValueAtIndex(modes, i);
            if m.is_null() || CGDisplayModeGetIOFlags(m) & kDisplayModeNativeFlag == 0 {
                continue;
            }
            let (w, h) = (
                CGDisplayModeGetPixelWidth(m) as u32,
                CGDisplayModeGetPixelHeight(m) as u32,
            );
            if best.is_none_or(|(bw, bh)| (w as u64 * h as u64) > (bw as u64 * bh as u64)) {
                best = Some((w, h));
            }
        }
        CFRelease(modes);
        best
    }
}

pub struct MacDisplays;

impl DisplayProvider for MacDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        refresh_display_names();
        let names = NAMES.lock().expect("names").clone().unwrap_or_default();
        let ids = CGDisplay::active_displays()
            .map_err(|e| PlatformError::Os(format!("CGGetActiveDisplayList 失败（{e}）")))?;
        let mut external = 0;
        let mut out = Vec::new();
        for id in ids {
            let d = CGDisplay::new(id);
            // 镜像显示器与主屏内容相同，不作为独立屏幕。
            if d.mirrors_display() != 0 {
                continue;
            }
            // bounds 为全局坐标（逻辑点，即系统设置里"看起来像"的分辨率），原点是主屏左上角。
            let b = d.bounds();
            // 渲染倍率：HiDPI 模式下为 2（系统先按 2 倍渲染，再缩放到面板）。
            let scale = d
                .display_mode()
                .filter(|m| m.width() > 0)
                .map(|m| m.pixel_width() as f64 / m.width() as f64)
                .unwrap_or(1.0);
            let name = match names.get(&id) {
                Some(n) if !n.is_empty() => n.clone(),
                _ if d.is_builtin() => "内建显示器".to_owned(),
                _ => {
                    external += 1;
                    format!("外接显示器 {external}")
                }
            };
            out.push(DisplayInfo {
                local_id: id.to_string(),
                name,
                rect: Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height),
                scale,
                primary: d.is_main(),
                native: native_resolution(id),
            });
        }
        Ok(out)
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef) -> u8;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

pub struct MacPermissions;

impl Permissions for MacPermissions {
    fn state(&self, permission: Permission) -> PermissionState {
        match permission {
            Permission::Accessibility => {
                // SAFETY: 无参数的系统查询函数。
                if unsafe { AXIsProcessTrusted() } != 0 {
                    PermissionState::Granted
                } else {
                    PermissionState::Denied
                }
            }
            Permission::InputMonitoring => {
                // SAFETY: 无参数的系统查询函数（macOS 10.15+）。
                if unsafe { CGPreflightListenEventAccess() } {
                    PermissionState::Granted
                } else {
                    PermissionState::Denied
                }
            }
            // 系统没有提供查询本地网络权限的接口，首次访问局域网时系统会自动弹窗。
            Permission::LocalNetwork => PermissionState::NotDetermined,
        }
    }

    fn request(&self, permission: Permission) {
        match permission {
            Permission::Accessibility => {
                // SAFETY: kAXTrustedCheckOptionPrompt 是系统导出的常量字符串，按 get 规则包装不转移所有权。
                let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
                let opts = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
                // SAFETY: opts 在调用期间有效。
                unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef()) };
            }
            Permission::InputMonitoring => {
                // SAFETY: 无参数，首次调用时系统弹出授权对话框。
                unsafe { CGRequestListenEventAccess() };
            }
            Permission::LocalNetwork => {}
        }
    }

    fn open_settings(&self, permission: Permission) -> Result<(), PlatformError> {
        let anchor = match permission {
            Permission::Accessibility => "Privacy_Accessibility",
            Permission::InputMonitoring => "Privacy_ListenEvent",
            Permission::LocalNetwork => "Privacy_LocalNetwork",
        };
        Command::new("open")
            .arg(format!(
                "x-apple.systempreferences:com.apple.preference.security?{anchor}"
            ))
            .status()
            .map_err(|e| PlatformError::Os(format!("无法打开系统设置：{e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 反复、并发调用，防止引用计数错误导致的崩溃再次出现。
    #[test]
    fn native_resolution_is_memory_safe() {
        let ids = CGDisplay::active_displays().unwrap_or_default();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let ids = ids.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        for &id in &ids {
                            let _ = native_resolution(id);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    /// CI 和无显示器的环境里可能枚举不到屏幕，这里只验证调用本身不出错、数据合理。
    #[test]
    fn enumerate_displays() {
        let displays = MacDisplays.displays().expect("枚举显示器");
        for d in &displays {
            assert!(d.rect.width > 0.0 && d.rect.height > 0.0);
            assert!(d.scale >= 1.0);
        }
        assert!(displays.iter().filter(|d| d.primary).count() <= 1);
        // 打印出来便于人工核对：cargo test -p flow-platform -- --nocapture
        for d in &displays {
            println!(
                "{} {:?} scale={} native={:?}",
                d.name, d.rect, d.scale, d.native
            );
        }
    }
}
