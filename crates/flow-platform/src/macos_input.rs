//! macOS 键鼠捕获（CGEventTap）与注入（CGEventPost）。
//!
//! - 捕获需要「输入监控」，拦截与注入需要「辅助功能」权限。
//! - **两项权限都确认已授予之后才调用 `CGEventTapCreate`**：缺权限时调用它会让窗口服务器去问系统的
//!   权限服务，用户正在系统设置里授权（等待密码 / Touch ID）时，这会让全系统的点击和键盘卡住。
//! - 事件回调运行在专用线程的 CFRunLoop 上，必须尽快返回，否则系统会暂时停用事件监听。
//! - 控制远端时用 `CGAssociateMouseAndMouseCursorPosition(false)` 把光标固定住，事件里仍然带相对位移。

use std::collections::HashSet;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::string::{CFString, CFStringRef};
use flow_core::keys::{KeyCode, MouseButton};

use crate::{
    CaptureHandler, CapturedEvent, INJECTED_MARK, InputCapture, InputInjector, Permission,
    PermissionState, PlatformError, Verdict, keymap,
};

type CGEventRef = *mut c_void;
type CFTypeRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGPoint,
}

type TapCallback = unsafe extern "C" fn(
    proxy: *const c_void,
    kind: u32,
    event: CGEventRef,
    user: *mut c_void,
) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events: u64,
        callback: TapCallback,
        user: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetDoubleValueField(event: CGEventRef, field: u32) -> f64;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetType(event: CGEventRef, kind: u32);
    fn CGEventCreate(source: *mut c_void) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        kind: u32,
        at: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, vk: u16, down: bool) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: *mut c_void,
        units: u32,
        count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventSourceCreate(state: i32) -> *mut c_void;
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventSourceSetLocalEventsSuppressionInterval(source: *mut c_void, seconds: f64);
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGMainDisplayID() -> u32;
    fn CGWarpMouseCursorPosition(at: CGPoint) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    // 私有接口：允许后台应用隐藏光标（Barrier、Deskflow 等同类软件都依赖它）。
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(cid: i32, target: i32, key: CFStringRef, value: CFTypeRef) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(
        alloc: *const c_void,
        port: *mut c_void,
        order: isize,
    ) -> *mut c_void;
    fn CFMachPortInvalidate(port: *mut c_void);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: CFStringRef);
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
    fn CFRunLoopStop(rl: *mut c_void);
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopCommonModes: CFStringRef;
    static kCFRunLoopDefaultMode: CFStringRef;
}

// CGEventType
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOUSE_MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const SCROLL_WHEEL: u32 = 22;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER: u32 = 0xFFFF_FFFF;

// CGEventField
const FIELD_CLICK_STATE: u32 = 1;
const FIELD_BUTTON_NUMBER: u32 = 3;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;
const FIELD_AUTOREPEAT: u32 = 8;
const FIELD_KEYCODE: u32 = 9;
const FIELD_USER_DATA: u32 = 42;
const FIELD_SCROLL_FIXED_1: u32 = 93;
const FIELD_SCROLL_FIXED_2: u32 = 94;

const HID_TAP: u32 = 0;
const SESSION_TAP: u32 = 1;

// 修饰键标志：通用位 | 区分左右的设备位
const FLAG_SHIFT: u64 = 0x2_0000;
const FLAG_CTRL: u64 = 0x4_0000;
const FLAG_ALT: u64 = 0x8_0000;
const FLAG_CMD: u64 = 0x10_0000;
const FLAG_CAPS: u64 = 0x1_0000;
/// 系统创建键盘事件时按键位自动带上的标志（小键盘、方向键 / Fn 区），注入时要保留。
const FLAG_NUMPAD: u64 = 0x20_0000;
const FLAG_FN: u64 = 0x80_0000;

/// 修饰键对应的 (通用标志, 设备标志)。
fn modifier_flags(code: KeyCode) -> Option<(u64, u64)> {
    Some(match code {
        KeyCode::LEFT_CTRL => (FLAG_CTRL, 0x1),
        KeyCode::RIGHT_CTRL => (FLAG_CTRL, 0x2000),
        KeyCode::LEFT_SHIFT => (FLAG_SHIFT, 0x2),
        KeyCode::RIGHT_SHIFT => (FLAG_SHIFT, 0x4),
        KeyCode::LEFT_ALT => (FLAG_ALT, 0x20),
        KeyCode::RIGHT_ALT => (FLAG_ALT, 0x40),
        KeyCode::LEFT_META => (FLAG_CMD, 0x8),
        KeyCode::RIGHT_META => (FLAG_CMD, 0x10),
        _ => return None,
    })
}

fn mask(kind: u32) -> u64 {
    1u64 << kind
}

// ---------------------------------------------------------------------------
// 捕获
// ---------------------------------------------------------------------------

struct TapState {
    handler: CaptureHandler,
    tap: *mut c_void,
    scroll: (f64, f64),
}

unsafe extern "C" fn tap_callback(
    _proxy: *const c_void,
    kind: u32,
    event: CGEventRef,
    user: *mut c_void,
) -> CGEventRef {
    // SAFETY: user 是 run_tap 中创建的 TapState，在 tap 存活期间有效，且只在本线程访问。
    let state = unsafe { &mut *(user as *mut TapState) };
    if kind == TAP_DISABLED_BY_TIMEOUT || kind == TAP_DISABLED_BY_USER {
        // SAFETY: tap 指针在本线程有效。
        unsafe { CGEventTapEnable(state.tap, true) };
        return event;
    }
    // SAFETY: event 由系统传入，回调期间有效。
    if unsafe { CGEventGetIntegerValueField(event, FIELD_USER_DATA) } == INJECTED_MARK as i64 {
        return event;
    }
    // 回调里 panic 会直接终止进程：兜住，出错时放行并恢复光标。
    let verdict = catch_unwind(AssertUnwindSafe(|| {
        let mut verdict = Verdict::Pass;
        for ev in translate(state, kind, event).into_iter().flatten() {
            if (state.handler)(ev) == Verdict::Suppress {
                verdict = Verdict::Suppress;
            }
        }
        verdict
    }))
    .unwrap_or_else(|_| {
        set_cursor_captured(false, None);
        Verdict::Pass
    });
    match verdict {
        Verdict::Pass => event,
        Verdict::Suppress => std::ptr::null_mut(),
    }
}

/// 系统事件 → 平台无关事件。一个系统事件可能对应 0 个（不关心的键）或 2 个（Caps Lock）事件。
/// 回调里每个鼠标事件都会走到这里，用定长数组返回，不分配内存。
fn translate(state: &mut TapState, kind: u32, event: CGEventRef) -> [Option<CapturedEvent>; 2] {
    // SAFETY: 以下读取的都是回调期间有效的 event 的字段。
    unsafe {
        match kind {
            MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED => {
                let p = CGEventGetLocation(event);
                let dx = CGEventGetDoubleValueField(event, FIELD_DELTA_X);
                let dy = CGEventGetDoubleValueField(event, FIELD_DELTA_Y);
                // 事件里是移动后的位置；按约定上报移动前的位置（见 CapturedEvent::MouseMove）。
                // 直接上报移动后的位置会让调用方把位移算两遍，快速甩动时没到边就判成推出边缘。
                [
                    Some(CapturedEvent::MouseMove {
                        x: p.x - dx,
                        y: p.y - dy,
                        dx,
                        dy,
                    }),
                    None,
                ]
            }
            LEFT_DOWN | LEFT_UP | RIGHT_DOWN | RIGHT_UP | OTHER_DOWN | OTHER_UP => {
                let button = match kind {
                    LEFT_DOWN | LEFT_UP => MouseButton::Left,
                    RIGHT_DOWN | RIGHT_UP => MouseButton::Right,
                    _ => match CGEventGetIntegerValueField(event, FIELD_BUTTON_NUMBER) {
                        2 => MouseButton::Middle,
                        3 => MouseButton::Back,
                        4 => MouseButton::Forward,
                        _ => return [None, None],
                    },
                };
                let down = matches!(kind, LEFT_DOWN | RIGHT_DOWN | OTHER_DOWN);
                [Some(CapturedEvent::Button { button, down }), None]
            }
            SCROLL_WHEEL => {
                // 单位是"行"（触控板为小数），换算成 1/120 格并累积不足一个单位的余数。
                let ly = CGEventGetDoubleValueField(event, FIELD_SCROLL_FIXED_1) * 120.0;
                let lx = -CGEventGetDoubleValueField(event, FIELD_SCROLL_FIXED_2) * 120.0;
                state.scroll.0 += lx;
                state.scroll.1 += ly;
                let (dx, dy) = (state.scroll.0.trunc(), state.scroll.1.trunc());
                state.scroll.0 -= dx;
                state.scroll.1 -= dy;
                if dx == 0.0 && dy == 0.0 {
                    return [None, None];
                }
                [
                    Some(CapturedEvent::Wheel {
                        dx: dx as i32,
                        dy: dy as i32,
                    }),
                    None,
                ]
            }
            KEY_DOWN | KEY_UP => {
                let vk = CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16;
                let Some(code) = keymap::from_mac(vk) else {
                    return [None, None];
                };
                [
                    Some(CapturedEvent::Key {
                        code,
                        down: kind == KEY_DOWN,
                        repeat: CGEventGetIntegerValueField(event, FIELD_AUTOREPEAT) != 0,
                    }),
                    None,
                ]
            }
            FLAGS_CHANGED => {
                let vk = CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16;
                let Some(code) = keymap::from_mac(vk) else {
                    return [None, None];
                };
                if code == KeyCode::CAPS_LOCK {
                    // Caps Lock 每按一次只产生一个 flagsChanged，当作一次完整的按下 + 抬起。
                    return [
                        Some(CapturedEvent::Key {
                            code,
                            down: true,
                            repeat: false,
                        }),
                        Some(CapturedEvent::Key {
                            code,
                            down: false,
                            repeat: false,
                        }),
                    ];
                }
                let Some((_, device_bit)) = modifier_flags(code) else {
                    return [None, None];
                };
                [
                    Some(CapturedEvent::Key {
                        code,
                        down: CGEventGetFlags(event) & device_bit != 0,
                        repeat: false,
                    }),
                    None,
                ]
            }
            _ => [None, None],
        }
    }
}

#[derive(Default)]
pub struct MacCapture {
    run_loop: Option<usize>,
    thread: Option<JoinHandle<()>>,
    /// 通知输入线程退出。`CFRunLoopStop` 对还没开始运行的 run loop 不起作用，
    /// 只靠它的话，刚启动就停止会永远等不到线程结束。
    stop: Arc<AtomicBool>,
}

impl InputCapture for MacCapture {
    fn start(&mut self, handler: CaptureHandler) -> Result<(), PlatformError> {
        self.stop();
        // 只在本进程里查询权限（不经过窗口服务器），缺任何一项都不创建事件监听。
        let perms = crate::permissions();
        for p in [Permission::InputMonitoring, Permission::Accessibility] {
            if perms.state(p) != PermissionState::Granted {
                return Err(PlatformError::PermissionDenied(p));
            }
        }
        let (tx, rx) = mpsc::channel::<Result<usize, PlatformError>>();
        let thread = std::thread::Builder::new()
            .name("flowacross-event-tap".into())
            .spawn({
                self.stop = Arc::new(AtomicBool::new(false));
                let stop = self.stop.clone();
                move || run_tap(handler, tx, stop)
            })
            .map_err(|e| PlatformError::Os(format!("无法创建输入线程：{e}")))?;
        match rx.recv() {
            Ok(Ok(rl)) => {
                self.run_loop = Some(rl);
                self.thread = Some(thread);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(PlatformError::Os("输入线程意外退出".into())),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(rl) = self.run_loop.take() {
            // SAFETY: rl 是输入线程的 CFRunLoop，线程退出前一直有效；CFRunLoopStop 可跨线程调用。
            unsafe { CFRunLoopStop(rl as *mut c_void) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        set_cursor_captured(false, None);
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_tap(
    handler: CaptureHandler,
    ready: mpsc::Sender<Result<usize, PlatformError>>,
    stop: Arc<AtomicBool>,
) {
    let events = mask(MOUSE_MOVED)
        | mask(LEFT_DRAGGED)
        | mask(RIGHT_DRAGGED)
        | mask(OTHER_DRAGGED)
        | mask(LEFT_DOWN)
        | mask(LEFT_UP)
        | mask(RIGHT_DOWN)
        | mask(RIGHT_UP)
        | mask(OTHER_DOWN)
        | mask(OTHER_UP)
        | mask(SCROLL_WHEEL)
        | mask(KEY_DOWN)
        | mask(KEY_UP)
        | mask(FLAGS_CHANGED);
    let state = Box::into_raw(Box::new(TapState {
        handler,
        tap: std::ptr::null_mut(),
        scroll: (0.0, 0.0),
    }));
    // SAFETY: state 在 CFRunLoopRun 返回、tap 失效之后才释放；所有 CF 对象按 Create 规则释放。
    unsafe {
        let tap = CGEventTapCreate(
            SESSION_TAP,
            0,
            0,
            events,
            tap_callback,
            state as *mut c_void,
        );
        if tap.is_null() {
            drop(Box::from_raw(state));
            let _ = ready.send(Err(PlatformError::Os(
                "系统拒绝创建键鼠监听（CGEventTapCreate 返回空）".into(),
            )));
            return;
        }
        (*state).tap = tap;
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        let rl = CFRunLoopGetCurrent();
        CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        let _ = ready.send(Ok(rl as usize));
        // 分段运行并检查退出标志：stop() 在 run loop 开始运行前调用时 CFRunLoopStop 是空操作，
        // 靠标志保证最多半秒后退出；正常情况下 CFRunLoopStop 让这次调用立即返回。
        while !stop.load(Ordering::Relaxed) {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.5, 0);
        }
        CGEventTapEnable(tap, false);
        CFMachPortInvalidate(tap);
        CFRelease(source);
        CFRelease(tap);
        drop(Box::from_raw(state));
    }
}

// ---------------------------------------------------------------------------
// 光标锁定
// ---------------------------------------------------------------------------

static CURSOR_HIDDEN: Mutex<bool> = Mutex::new(false);

pub fn set_cursor_captured(captured: bool, restore_at: Option<(f64, f64)>) {
    let mut hidden = CURSOR_HIDDEN.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: 以下均为无副作用风险的 CoreGraphics 光标调用；隐藏/显示成对进行（由 hidden 记录）。
    unsafe {
        if captured {
            CGAssociateMouseAndMouseCursorPosition(0);
            if !*hidden {
                let key = CFString::from_static_string("SetsCursorInBackground");
                let cid = _CGSDefaultConnection();
                CGSSetConnectionProperty(
                    cid,
                    cid,
                    key.as_concrete_TypeRef(),
                    CFBoolean::true_value().as_CFTypeRef(),
                );
                CGDisplayHideCursor(CGMainDisplayID());
                *hidden = true;
            }
        } else {
            if let Some((x, y)) = restore_at {
                CGWarpMouseCursorPosition(CGPoint { x, y });
            }
            // 移动光标后紧接着重新关联鼠标，可以取消系统默认的 0.25 秒输入冻结。
            CGAssociateMouseAndMouseCursorPosition(1);
            if *hidden {
                CGDisplayShowCursor(CGMainDisplayID());
                *hidden = false;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 注入
// ---------------------------------------------------------------------------

pub struct MacInjector {
    source: *mut c_void,
    pos: CGPoint,
    buttons: HashSet<MouseButton>,
    modifiers: HashSet<KeyCode>,
    last_click: Option<(MouseButton, Instant, CGPoint, i64)>,
    /// 高精度滚动里还不够 1 像素的余量（1/120 格），累计到下一次，慢速滚动才不会被丢掉。
    scroll_rem: (i32, i32),
    /// Caps Lock 是否打开。注入的 Caps Lock 按键不会改变系统的大写锁定状态，
    /// 所以自己记着，给之后的事件带上大写锁定标志。
    caps: bool,
}

// SAFETY: CGEventSource 可以在任意线程使用；注入器同一时间只被一个线程持有（外层有锁）。
unsafe impl Send for MacInjector {}

impl MacInjector {
    pub fn new() -> Result<Self, PlatformError> {
        // SAFETY: 创建事件源；查询当前光标位置的临时事件用完释放。
        unsafe {
            let source = CGEventSourceCreate(1); // kCGEventSourceStateHIDSystemState
            if source.is_null() {
                return Err(PlatformError::Os("CGEventSourceCreate 失败".into()));
            }
            CGEventSourceSetLocalEventsSuppressionInterval(source, 0.0);
            let probe = CGEventCreate(std::ptr::null_mut());
            let pos = if probe.is_null() {
                CGPoint::default()
            } else {
                let p = CGEventGetLocation(probe);
                CFRelease(probe);
                p
            };
            Ok(Self {
                source,
                pos,
                buttons: HashSet::new(),
                modifiers: HashSet::new(),
                last_click: None,
                scroll_rem: (0, 0),
                // 从本机当前的大写锁定状态开始。
                caps: CGEventSourceFlagsState(1) & FLAG_CAPS != 0,
            })
        }
    }

    fn flags(&self) -> u64 {
        self.modifiers
            .iter()
            .filter_map(|&k| modifier_flags(k))
            .fold(if self.caps { FLAG_CAPS } else { 0 }, |acc, (g, d)| {
                acc | g | d
            })
    }

    /// 打上流界标记、带上当前修饰键后发送，并释放事件。
    fn post(&self, event: CGEventRef) -> Result<(), PlatformError> {
        if event.is_null() {
            return Err(PlatformError::Os("创建系统输入事件失败".into()));
        }
        // SAFETY: event 由 CGEventCreate* 返回（+1），发送后释放。
        unsafe {
            CGEventSetIntegerValueField(event, FIELD_USER_DATA, INJECTED_MARK as i64);
            // 修饰键以远端按下的为准（不混入本机键盘的状态），但保留系统按键位加上的小键盘 / Fn 标志，
            // 否则方向键、Home/End、小键盘在部分程序里行为不对。
            let keep = CGEventGetFlags(event) & (FLAG_NUMPAD | FLAG_FN);
            CGEventSetFlags(event, self.flags() | keep);
            CGEventPost(HID_TAP, event);
            CFRelease(event);
        }
        Ok(())
    }

    fn drag_kind(&self) -> (u32, u32) {
        if self.buttons.contains(&MouseButton::Left) {
            (LEFT_DRAGGED, 0)
        } else if self.buttons.contains(&MouseButton::Right) {
            (RIGHT_DRAGGED, 1)
        } else if let Some(b) = self.buttons.iter().next() {
            (OTHER_DRAGGED, button_number(*b))
        } else {
            (MOUSE_MOVED, 0)
        }
    }
}

impl Drop for MacInjector {
    fn drop(&mut self) {
        // SAFETY: source 由 CGEventSourceCreate 创建。
        unsafe { CFRelease(self.source) };
    }
}

fn button_number(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

impl InputInjector for MacInjector {
    fn move_to(&mut self, display_local_id: &str, x: f64, y: f64) -> Result<(), PlatformError> {
        let id = display_local_id
            .parse::<u32>()
            // SAFETY: 无参数查询。
            .unwrap_or_else(|_| unsafe { CGMainDisplayID() });
        // SAFETY: 无效的显示器 ID 返回空矩形，不会出错。
        let mut b = unsafe { CGDisplayBounds(id) };
        if b.size.x <= 0.0 {
            // SAFETY: 同上。
            b = unsafe { CGDisplayBounds(CGMainDisplayID()) };
        }
        let to = CGPoint {
            x: b.origin.x + x.clamp(0.0, (b.size.x - 0.01).max(0.0)),
            y: b.origin.y + y.clamp(0.0, (b.size.y - 0.01).max(0.0)),
        };
        let (kind, button) = self.drag_kind();
        // SAFETY: 创建事件，post 负责释放。
        let ev = unsafe { CGEventCreateMouseEvent(self.source, kind, to, button) };
        if !ev.is_null() {
            // SAFETY: ev 有效。
            unsafe {
                CGEventSetIntegerValueField(ev, FIELD_DELTA_X, (to.x - self.pos.x).round() as i64);
                CGEventSetIntegerValueField(ev, FIELD_DELTA_Y, (to.y - self.pos.y).round() as i64);
            }
        }
        self.pos = to;
        self.post(ev)
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let kind = match (button, down) {
            (MouseButton::Left, true) => LEFT_DOWN,
            (MouseButton::Left, false) => LEFT_UP,
            (MouseButton::Right, true) => RIGHT_DOWN,
            (MouseButton::Right, false) => RIGHT_UP,
            (_, true) => OTHER_DOWN,
            (_, false) => OTHER_UP,
        };
        // 连击计数：同一按钮在 0.5 秒内、位置几乎不变时算双击 / 三击。
        let clicks = if down {
            let n = match self.last_click {
                Some((b, t, p, n))
                    if b == button
                        && t.elapsed().as_millis() < 500
                        && (p.x - self.pos.x).abs() < 4.0
                        && (p.y - self.pos.y).abs() < 4.0 =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.last_click = Some((button, Instant::now(), self.pos, n));
            self.buttons.insert(button);
            n
        } else {
            self.buttons.remove(&button);
            self.last_click.map(|c| c.3).unwrap_or(1)
        };
        // SAFETY: 创建事件并设置字段，post 负责释放。
        unsafe {
            let ev = CGEventCreateMouseEvent(self.source, kind, self.pos, button_number(button));
            if !ev.is_null() {
                CGEventSetIntegerValueField(ev, FIELD_BUTTON_NUMBER, button_number(button) as i64);
                CGEventSetIntegerValueField(ev, FIELD_CLICK_STATE, clicks);
            }
            self.post(ev)
        }
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        // 整格滚动按"行"发送；高精度滚动（触控板）按像素发送，约 10 像素一行。
        let (units, wy, wx) = if dx % 120 == 0 && dy % 120 == 0 {
            self.scroll_rem = (0, 0);
            (1, dy / 120, -dx / 120)
        } else {
            // 不足 1 像素（12 个单位）的部分留到下一次，否则慢速滚动每次都被整除成 0。
            let (ax, ay) = (self.scroll_rem.0 + dx, self.scroll_rem.1 + dy);
            self.scroll_rem = (ax % 12, ay % 12);
            (0, ay / 12, -ax / 12)
        };
        if wx == 0 && wy == 0 {
            return Ok(());
        }
        // SAFETY: 创建事件，post 负责释放。
        let ev = unsafe { CGEventCreateScrollWheelEvent2(self.source, units, 2, wy, wx, 0) };
        self.post(ev)
    }

    fn key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Result<(), PlatformError> {
        let Some(vk) = keymap::to_mac(code) else {
            return Ok(());
        };
        // Caps Lock：系统里每按一次只有一个 flagsChanged（捕获端也是这样拆成按下 + 抬起的）。
        // 按下时切换状态并发一个 flagsChanged，抬起不发。
        let is_caps = code == KeyCode::CAPS_LOCK;
        if is_caps {
            if !down || repeat {
                return Ok(());
            }
            self.caps = !self.caps;
        }
        let is_modifier = is_caps || modifier_flags(code).is_some();
        if modifier_flags(code).is_some() {
            if down {
                self.modifiers.insert(code);
            } else {
                self.modifiers.remove(&code);
            }
        }
        // SAFETY: 创建事件并设置字段，post 负责释放。
        unsafe {
            let ev = CGEventCreateKeyboardEvent(self.source, vk, down);
            if !ev.is_null() {
                if is_modifier {
                    CGEventSetType(ev, FLAGS_CHANGED);
                }
                if repeat {
                    CGEventSetIntegerValueField(ev, FIELD_AUTOREPEAT, 1);
                }
            }
            self.post(ev)
        }
    }
}

// ---------------------------------------------------------------------------
// 剪贴板
// ---------------------------------------------------------------------------

/// 系统剪贴板（NSPasteboard：纯文本、文件 URL 和图片）。
pub struct MacClipboard;

impl crate::ClipboardAccess for MacClipboard {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError> {
        objc2::rc::autoreleasepool(|_| {
            let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
            // SAFETY: NSPasteboardTypeString 是 AppKit 导出的常量。
            let text = pb.stringForType(unsafe { objc2_app_kit::NSPasteboardTypeString });
            Ok(text.map(|s| s.to_string()))
        })
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        objc2::rc::autoreleasepool(|_| {
            let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
            pb.clearContents();
            let s = objc2_foundation::NSString::from_str(text);
            // SAFETY: 同上。
            if pb.setString_forType(&s, unsafe { objc2_app_kit::NSPasteboardTypeString }) {
                Ok(())
            } else {
                Err(PlatformError::Os("写入剪贴板失败".into()))
            }
        })
    }

    fn get_files(&mut self) -> Result<Option<Vec<std::path::PathBuf>>, PlatformError> {
        use objc2_foundation::{NSString, NSURL};
        objc2::rc::autoreleasepool(|_| {
            let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
            let Some(items) = pb.pasteboardItems() else {
                return Ok(None);
            };
            let file_url = NSString::from_str("public.file-url");
            let mut out = Vec::new();
            for item in items.iter() {
                let Some(s) = item.stringForType(&file_url) else {
                    continue;
                };
                let Some(url) = NSURL::URLWithString(&s) else {
                    continue;
                };
                // Finder 有时给的是文件引用 URL（file:///.file/id=…），先换成路径 URL。
                let url = url.filePathURL().unwrap_or(url);
                if let Some(path) = url.path() {
                    out.push(std::path::PathBuf::from(path.to_string()));
                }
            }
            Ok((!out.is_empty()).then_some(out))
        })
    }

    fn set_files(&mut self, paths: &[std::path::PathBuf]) -> Result<(), PlatformError> {
        use objc2::rc::Retained;
        use objc2::runtime::ProtocolObject;
        use objc2_app_kit::NSPasteboardWriting;
        use objc2_foundation::{NSArray, NSString, NSURL};
        objc2::rc::autoreleasepool(|_| {
            let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
                .iter()
                .map(|p| {
                    let url = NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()));
                    ProtocolObject::from_retained(url)
                })
                .collect();
            let array = NSArray::from_retained_slice(&urls);
            let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
            pb.clearContents();
            if pb.writeObjects(&array) {
                Ok(())
            } else {
                Err(PlatformError::Os("写入剪贴板失败".into()))
            }
        })
    }

    fn get_image(&mut self) -> Result<Option<Vec<u8>>, PlatformError> {
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
        use objc2_foundation::NSDictionary;
        objc2::rc::autoreleasepool(|_| {
            let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
            // SAFETY: NSPasteboardTypePNG / TIFF 是 AppKit 导出的常量。
            let (png, tiff) = unsafe {
                (
                    objc2_app_kit::NSPasteboardTypePNG,
                    objc2_app_kit::NSPasteboardTypeTIFF,
                )
            };
            // 截图直接就是 PNG。
            if let Some(data) = pb.dataForType(png) {
                return Ok(Some(data.to_vec()));
            }
            // 预览、浏览器等复制的图片常常只有 TIFF：转成 PNG。
            let Some(data) = pb.dataForType(tiff) else {
                return Ok(None);
            };
            let Some(rep) = NSBitmapImageRep::imageRepWithData(&data) else {
                return Ok(None);
            };
            // SAFETY: 属性字典为空，类型是 AppKit 定义的 PNG。
            let converted = unsafe {
                rep.representationUsingType_properties(
                    NSBitmapImageFileType::PNG,
                    &NSDictionary::new(),
                )
            };
            Ok(converted.map(|d| d.to_vec()))
        })
    }

    fn set_image(&mut self, image: &crate::ClipboardImage) -> Result<(), PlatformError> {
        objc2::rc::autoreleasepool(|_| {
            let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
            pb.clearContents();
            let data = objc2_foundation::NSData::with_bytes(image.png());
            // SAFETY: 同上。
            if pb.setData_forType(Some(&data), unsafe { objc2_app_kit::NSPasteboardTypePNG }) {
                Ok(())
            } else {
                Err(PlatformError::Os("写入剪贴板失败".into()))
            }
        })
    }

    fn change_count(&self) -> u64 {
        objc2::rc::autoreleasepool(|_| {
            objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as u64
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClipboardAccess;

    /// 会改写系统剪贴板，默认不运行：cargo test -p flow-platform -- --ignored
    #[test]
    #[ignore]
    fn clipboard_round_trip() {
        let mut cb = MacClipboard;
        let before = cb.get_text().unwrap();
        let count = cb.change_count();
        cb.set_text("流界剪贴板测试 ✓").unwrap();
        assert!(cb.change_count() > count);
        assert_eq!(cb.get_text().unwrap().as_deref(), Some("流界剪贴板测试 ✓"));
        if let Some(t) = before {
            cb.set_text(&t).unwrap();
        }
    }

    /// 会改写系统剪贴板，默认不运行：cargo test -p flow-platform -- --ignored
    #[test]
    #[ignore]
    fn clipboard_image_round_trip() {
        // 1×1 的红色 PNG。
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255, 0, 0]).unwrap();
        writer.finish().unwrap();

        let mut cb = MacClipboard;
        let count = cb.change_count();
        let image = crate::ClipboardImage::from_png(png.clone()).unwrap();
        cb.set_image(&image).unwrap();
        assert!(cb.change_count() > count);
        assert_eq!(cb.get_image().unwrap(), Some(png));
        // 图片不是文字也不是文件。
        assert_eq!(cb.get_text().unwrap(), None);
        assert_eq!(cb.get_files().unwrap(), None);
        assert!(crate::ClipboardImage::from_png(b"not png".to_vec()).is_none());
    }
}
