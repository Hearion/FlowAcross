//! macOS：现在在前台的应用，以及它的窗口是不是全屏（按应用自动锁定用）。
//!
//! - 前台应用：`NSWorkspace.frontmostApplication`。
//! - 全屏：先问辅助功能（前台应用的焦点窗口的 `AXFullScreen`，原生全屏）；游戏自己铺满屏幕的
//!   "无边框全屏"不算原生全屏，再看这个应用最前面的普通窗口是不是正好盖住一整块显示器。
//!   不能只比窗口大小：带刘海的 MacBook 上原生全屏的窗口在刘海下面，比屏幕矮。
//!
//! 辅助功能的调用是发给对方应用的消息，对方卡住时会一直等，所以把超时设得很短。
//! 流界本来就需要「辅助功能」权限；没有权限时 `AXFullScreen` 查不到，只剩窗口大小的判断。

use std::ffi::c_void;

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use flow_core::autolock::Foreground;
use objc2_app_kit::NSWorkspace;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *const c_void;
    fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
}

/// 只列屏幕上看得见的窗口，不含桌面图标和壁纸。
const ON_SCREEN_ONLY: u32 = 1 << 0;
const EXCLUDE_DESKTOP: u32 = 1 << 4;
/// 等对方应用回答辅助功能查询的时间。
const AX_TIMEOUT_SECS: f32 = 0.25;

/// 读辅助功能元素的一个属性；查不到（没有权限、对方不支持、超时）返回 `None`。
fn ax_attribute(element: &CFType, name: &str) -> Option<CFType> {
    let name = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    // SAFETY: element 是有效的 AXUIElement；成功时 value 是 +1 的对象，交给 CFType 管理。
    unsafe {
        let err = AXUIElementCopyAttributeValue(
            element.as_CFTypeRef(),
            name.as_concrete_TypeRef(),
            &mut value,
        );
        (err == 0 && !value.is_null()).then(|| CFType::wrap_under_create_rule(value))
    }
}

/// 这个应用的焦点窗口是不是原生全屏。
fn ax_fullscreen(pid: i32) -> bool {
    // SAFETY: 创建出的元素是 +1 的对象；对无效的 pid 也返回一个元素，后面的查询会失败。
    let app = unsafe {
        let el = AXUIElementCreateApplication(pid);
        if el.is_null() {
            return false;
        }
        AXUIElementSetMessagingTimeout(el, AX_TIMEOUT_SECS);
        CFType::wrap_under_create_rule(el)
    };
    ax_attribute(&app, "AXFocusedWindow")
        .and_then(|window| ax_attribute(&window, "AXFullScreen"))
        .and_then(|v| v.downcast::<CFBoolean>())
        .is_some_and(|b| b == CFBoolean::true_value())
}

/// 窗口正好盖住一整块显示器（允许 1 个点的误差）。
fn covers_a_display(win: CGRect, displays: &[CGRect]) -> bool {
    let near = |a: f64, b: f64| (a - b).abs() <= 1.0;
    displays
        .iter()
        .any(|d| near(win.x, d.x) && near(win.y, d.y) && near(win.w, d.w) && near(win.h, d.h))
}

/// 这个应用最前面的普通窗口（layer 0）的位置和大小。
fn front_window_bounds(pid: i32) -> Option<CGRect> {
    // SAFETY: 返回 +1 的数组（可能为空指针），元素是字典。
    let list: CFArray<CFDictionary<CFString, CFType>> = unsafe {
        let raw = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if raw.is_null() {
            return None;
        }
        CFArray::wrap_under_create_rule(raw.cast())
    };
    let number = |d: &CFDictionary<CFString, CFType>, key: &'static str| {
        d.find(CFString::from_static_string(key))
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
    };
    // 列表按从前到后的顺序。
    list.iter().find_map(|w| {
        if number(&w, "kCGWindowOwnerPID") != Some(i64::from(pid))
            || number(&w, "kCGWindowLayer") != Some(0)
        {
            return None;
        }
        let bounds = w
            .find(CFString::from_static_string("kCGWindowBounds"))?
            .downcast::<CFDictionary>()?;
        let mut rect = CGRect::default();
        // SAFETY: bounds 是系统给的矩形字典，rect 是有效的输出位置。
        unsafe { CGRectMakeWithDictionaryRepresentation(bounds.as_concrete_TypeRef(), &mut rect) }
            .then_some(rect)
    })
}

fn display_bounds() -> Vec<CGRect> {
    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let b = CGDisplay::new(id).bounds();
            CGRect {
                x: b.origin.x,
                y: b.origin.y,
                w: b.size.width,
                h: b.size.height,
            }
        })
        .collect()
}

/// 现在在前台的应用；查不到时返回 `None`。前台是流界自己时应用名为空（调用方据此忽略）。
pub fn foreground() -> Option<Foreground> {
    objc2::rc::autoreleasepool(|_| {
        let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        // SAFETY: `processIdentifier` 返回 pid_t（i32）。绑定里这个方法要额外的 libc 特性，直接发消息。
        let pid: i32 = unsafe { objc2::msg_send![&*app, processIdentifier] };
        if pid <= 0 {
            return None;
        }
        if pid as u32 == std::process::id() {
            return Some(Foreground {
                app: String::new(),
                fullscreen: false,
            });
        }
        let name = app
            .localizedName()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let fullscreen = ax_fullscreen(pid)
            || front_window_bounds(pid).is_some_and(|w| covers_a_display(w, &display_bounds()));
        Some(Foreground {
            app: name,
            fullscreen,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect { x, y, w, h }
    }

    #[test]
    fn only_a_window_covering_a_whole_display_counts() {
        let displays = [
            rect(0.0, 0.0, 1512.0, 982.0),
            rect(1512.0, 0.0, 2560.0, 1440.0),
        ];
        assert!(covers_a_display(rect(0.0, 0.0, 1512.0, 982.0), &displays));
        assert!(covers_a_display(
            rect(1512.0, 0.5, 2560.0, 1440.0),
            &displays
        ));
        // 最大化的窗口在菜单栏下面，不算全屏。
        assert!(!covers_a_display(rect(0.0, 38.0, 1512.0, 944.0), &displays));
        assert!(!covers_a_display(
            rect(100.0, 100.0, 800.0, 600.0),
            &displays
        ));
        assert!(!covers_a_display(rect(0.0, 0.0, 1512.0, 982.0), &[]));
    }

    /// 只读：查一次真实的前台应用，确认各个系统调用能走通、不会崩。
    #[test]
    fn foreground_query_runs_on_this_mac() {
        let fg = foreground();
        println!("前台应用：{fg:?}");
        if let Some(fg) = fg {
            assert!(fg.app.chars().count() < 256);
        }
        // 对一个不存在的进程查全屏：返回 false，不阻塞、不崩。
        assert!(!ax_fullscreen(i32::MAX - 1));
        assert!(front_window_bounds(i32::MAX - 1).is_none());
    }
}
