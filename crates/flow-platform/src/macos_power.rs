//! macOS 的屏幕电源：点亮屏幕、阻止屏幕自动熄灭、查询屏幕是否熄着（IOKit 电源断言）。

use std::sync::Mutex;

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};

use crate::PlatformError;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionDeclareUserActivity(name: CFStringRef, user_type: u32, id: *mut u32) -> i32;
    fn IOPMAssertionCreateWithName(
        kind: CFStringRef,
        level: u32,
        name: CFStringRef,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayIsAsleep(display: u32) -> u32;
}

/// `kIOPMUserActiveLocal`：坐在电脑前的用户有操作。
const USER_ACTIVE_LOCAL: u32 = 0;
/// `kIOPMAssertionLevelOn`。
const LEVEL_ON: u32 = 255;

/// "用户有操作"的断言编号。系统要求每次声明都传回上一次的编号，才会复用同一个断言。
static ACTIVITY: Mutex<u32> = Mutex::new(0);
/// 阻止屏幕熄灭的断言编号；没开时为 `None`。
static KEEP_AWAKE: Mutex<Option<u32>> = Mutex::new(None);

/// 点亮屏幕：向系统声明"用户有操作"，效果和碰一下键盘相同，同时重置自动熄屏的计时。
pub fn wake_display() -> Result<(), PlatformError> {
    let name = CFString::new("流界：其他电脑的键鼠来到本机");
    let mut id = ACTIVITY.lock().expect("activity");
    // SAFETY: name 在调用期间有效，id 指向有效的 u32。
    let r = unsafe {
        IOPMAssertionDeclareUserActivity(name.as_concrete_TypeRef(), USER_ACTIVE_LOCAL, &mut *id)
    };
    if r == 0 {
        Ok(())
    } else {
        Err(PlatformError::Os(format!(
            "系统没有接受点亮屏幕的请求（IOKit 错误 {r:#x}）"
        )))
    }
}

/// 开启或关闭"不让屏幕自动熄灭"。不影响用户自己锁屏、合上盖子。
pub fn set_keep_awake(on: bool) -> Result<(), PlatformError> {
    let mut held = KEEP_AWAKE.lock().expect("keep awake");
    match (on, *held) {
        (true, None) => {
            let kind = CFString::from_static_string("PreventUserIdleDisplaySleep");
            let name = CFString::new("流界：连着其他电脑时保持屏幕常亮");
            let mut id = 0u32;
            // SAFETY: 两个字符串在调用期间有效，id 指向有效的 u32。
            let r = unsafe {
                IOPMAssertionCreateWithName(
                    kind.as_concrete_TypeRef(),
                    LEVEL_ON,
                    name.as_concrete_TypeRef(),
                    &mut id,
                )
            };
            if r != 0 {
                return Err(PlatformError::Os(format!(
                    "系统没有接受保持屏幕常亮的请求（IOKit 错误 {r:#x}）"
                )));
            }
            *held = Some(id);
        }
        (false, Some(id)) => {
            // SAFETY: id 是上面创建、还没释放的断言。
            unsafe { IOPMAssertionRelease(id) };
            *held = None;
        }
        _ => {}
    }
    Ok(())
}

/// 屏幕现在是不是熄着（任何一块连着的显示器在休眠）。一块都查不到时返回 `false`，
/// 由调用方按"列表为空"处理。
pub fn displays_asleep() -> bool {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    // SAFETY: 缓冲区长度与传入的上限一致。
    if unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return false;
    }
    ids[..count as usize]
        .iter()
        // SAFETY: 传入系统刚给出的显示器编号。
        .any(|&id| unsafe { CGDisplayIsAsleep(id) } != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断言能建、能重复建、能释放；查询熄屏状态不会出错（CI 上没有真屏幕，不断言结果）。
    #[test]
    fn assertions_can_be_taken_and_released() {
        set_keep_awake(true).unwrap();
        set_keep_awake(true).unwrap();
        assert!(KEEP_AWAKE.lock().unwrap().is_some());
        set_keep_awake(false).unwrap();
        set_keep_awake(false).unwrap();
        assert!(KEEP_AWAKE.lock().unwrap().is_none());
        wake_display().unwrap();
        wake_display().unwrap();
        let _ = displays_asleep();
    }
}
