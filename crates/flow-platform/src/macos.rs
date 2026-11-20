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

/// 见 [`crate::menu_bar::set_status`]。
///
/// # Safety
///
/// 必须在主线程上调用；`status_item` 必须指向一个还活着的 `NSStatusItem`。
pub(crate) unsafe fn set_status_icon(
    status_item: usize,
    status: Option<(&str, flow_core::autolock::LatencyLevel)>,
    template_png: &[u8],
) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSSize;

    if status_item == 0 {
        return;
    }
    // 有颜色时换成涂好色的图（不是模板，系统照原样画）；没有时换回模板图，系统按菜单栏深浅画黑或白。
    // 试过给按钮设 `contentTintColor`：菜单栏按钮不认，清空时还会把模板图标画成固定的黑色（真机发现）。
    let (png_data, template, tooltip) = match status {
        Some((tooltip, level)) => {
            match crate::menu_bar::tinted_png(template_png, crate::menu_bar::level_rgb(level)) {
                Some(png_data) => (png_data, false, tooltip),
                None => return,
            }
        }
        None => (
            template_png.to_vec(),
            true,
            crate::menu_bar::DEFAULT_TOOLTIP,
        ),
    };
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: 由调用方保证 `status_item` 是主线程上一个活着的 NSStatusItem；下面的消息
        // 都是 AppKit / Foundation 公开的方法，参数类型和签名一致，NSImage 用完释放。
        unsafe {
            let item = status_item as *mut AnyObject;
            let button: *mut AnyObject = msg_send![item, button];
            if button.is_null() {
                return;
            }
            let data: *mut AnyObject = msg_send![
                class!(NSData),
                dataWithBytes: png_data.as_ptr().cast::<std::ffi::c_void>(),
                length: png_data.len()
            ];
            if data.is_null() {
                return;
            }
            let alloc: *mut AnyObject = msg_send![class!(NSImage), alloc];
            let image: *mut AnyObject = msg_send![alloc, initWithData: data];
            if image.is_null() {
                return;
            }
            // 和托盘库一样：菜单栏图标高 18 点，宽度按比例。
            let size: NSSize = msg_send![image, size];
            let height = 18.0;
            let width = if size.height > 0.0 {
                size.width * height / size.height
            } else {
                height
            };
            let _: () = msg_send![image, setSize: NSSize::new(width, height)];
            let _: () = msg_send![image, setTemplate: template];
            let _: () = msg_send![button, setImage: image];
            let _: () = msg_send![image, release];
            let tip = NSString::from_str(tooltip);
            let _: () = msg_send![button, setToolTip: &*tip];
            fit_click_area(button);
        }
    });

    /// 托盘库在按钮上盖了一层透明视图来接收点击（弹出菜单靠它），只在通过它自己改图标、标题时
    /// 才把这层的大小同步成按钮的大小。这里绕过它改了标题，按钮变宽了，这层却还是原来的大小和位置，
    /// 点图标就弹不出菜单（真机发现）。照它的做法把这层（类名 `TaoTrayTarget`，tray-icon 0.25）
    /// 设成按钮的大小；按钮自己的其他子视图不动。
    ///
    /// # Safety
    ///
    /// 在主线程上，`button` 是一个活着的 NSStatusBarButton。
    unsafe fn fit_click_area(button: *mut objc2::runtime::AnyObject) {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use objc2_foundation::NSRect;
        // SAFETY: 由调用方保证；`subviews` 返回按钮持有的数组，只在这里读。
        unsafe {
            let frame: NSRect = msg_send![button, frame];
            let subviews: *mut AnyObject = msg_send![button, subviews];
            if subviews.is_null() {
                return;
            }
            let count: usize = msg_send![subviews, count];
            for i in 0..count {
                let view: *mut AnyObject = msg_send![subviews, objectAtIndex: i];
                if view.is_null() {
                    continue;
                }
                let class_name: *mut NSString = msg_send![view, className];
                if !class_name.is_null() && (*class_name).to_string() == "TaoTrayTarget" {
                    let _: () = msg_send![view, setFrame: frame];
                }
            }
        }
    }
}

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
/// 面板的原生分辨率不会变，而查一次要列出这块屏幕的全部显示模式：按屏幕编号记住结果，
/// 引擎每隔几秒枚举一次显示器时不用每次都查。已经不在的屏幕从表里清掉。
fn cached_native_resolution(id: u32, active: &[u32]) -> Option<(u32, u32)> {
    /// (屏幕编号, 查到的原生分辨率)。
    type Entry = (u32, Option<(u32, u32)>);
    static CACHE: std::sync::Mutex<Vec<Entry>> = std::sync::Mutex::new(Vec::new());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(known, _)| active.contains(known));
    if let Some((_, found)) = cache.iter().find(|(known, _)| *known == id) {
        return *found;
    }
    let found = native_resolution(id);
    cache.push((id, found));
    found
}

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
        for &id in &ids {
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
                native: cached_native_resolution(id, &ids),
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
