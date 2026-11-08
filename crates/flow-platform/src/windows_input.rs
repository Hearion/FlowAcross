//! Windows 键鼠捕获（低级键盘 / 鼠标钩子）与注入（SendInput）。
//!
//! - 钩子装在专用线程上，靠该线程的消息循环驱动；回调必须很快返回，
//!   否则系统会跳过钩子（默认超时约 300 毫秒）。
//! - 钩子里的坐标是物理像素，换算成和 `DisplayInfo` 一致的逻辑坐标后再上报。
//! - 控制远端时把光标停在离开处附近（离屏幕边缘留一点余量），并拦截所有鼠标移动，只上报相对位移。
//! - 低级钩子按"后装先调用"的顺序执行，排在前面的钩子吃掉按键后我们就收不到了。
//!   流界窗口获得焦点时 WebView2 等组件会装自己的键盘钩子，所以开始控制远端时、
//!   前台窗口切换后、以及控制远端期间每 250 毫秒，都把钩子重新装一次，保持在最前面。
//! - 无法控制以管理员身份运行的窗口（系统的 UIPI 限制），除非流界也以管理员身份运行。

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use flow_core::keys::{KeyCode, MouseButton};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, EVENT_SYSTEM_FOREGROUND, GetCursorPos, GetMessageW, GetSystemMetrics, HHOOK,
    KBDLLHOOKSTRUCT, KillTimer, LLKHF_EXTENDED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
    PeekMessageW, PostThreadMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetCursorPos, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINEVENT_OUTOFCONTEXT, WM_APP, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER, WM_USER, WM_XBUTTONDOWN,
    WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

use crate::windows::{Monitor, monitors};
use crate::{
    CaptureHandler, CapturedEvent, INJECTED_MARK, InputCapture, InputInjector, PlatformError,
    Verdict, keymap,
};

static HANDLER: Mutex<Option<CaptureHandler>> = Mutex::new(None);
static MONITORS: Mutex<Vec<Monitor>> = Mutex::new(Vec::new());
static PRESSED: Mutex<Option<HashSet<KeyCode>>> = Mutex::new(None);
static CAPTURED: AtomicBool = AtomicBool::new(false);
static ANCHOR: Mutex<(i32, i32)> = Mutex::new((0, 0));
/// 钩子线程 ID（0 表示没有运行），用于通知它重新安装钩子。
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
/// 通知钩子线程重新安装钩子的线程消息。
const WM_REINSTALL: u32 = WM_APP + 1;
/// 前台窗口切换后延迟再装一次钩子的定时器（等新窗口的组件装好它们的钩子）。
const REINSTALL_DELAY_MS: u32 = 300;
/// 控制远端期间定期重装钩子的间隔。流界窗口在前台时，WebView2 会在窗口内焦点变化等时机
/// 装上自己的键盘钩子并排到我们前面，按键就到不了远端；定期重装保证我们始终排在最前面。
const KEEPALIVE_MS: u32 = 250;

/// 请求钩子线程重新安装钩子（异步，回调返回后才执行）。
fn request_reinstall() {
    let id = HOOK_THREAD.load(Ordering::Relaxed);
    if id != 0 {
        // SAFETY: 向钩子线程投递一条线程消息。
        unsafe { PostThreadMessageW(id, WM_REINSTALL, 0, 0) };
    }
}

pub(crate) fn update_monitors(list: Vec<Monitor>) {
    *MONITORS.lock().unwrap_or_else(|e| e.into_inner()) = list;
}

/// 在显示器快照上执行查询；快照为空时先刷新。
fn with_monitors<R>(f: impl FnOnce(&[Monitor]) -> R) -> R {
    let mut guard = MONITORS.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_empty()
        && let Ok(list) = monitors()
    {
        *guard = list;
    }
    f(&guard)
}

fn monitor_at(list: &[Monitor], x: i32, y: i32) -> Option<&Monitor> {
    list.iter().find(|m| m.contains_phys(x, y)).or_else(|| {
        // 不在任何显示器内（例如被推出边缘的坐标）时取最近的一块。
        list.iter().min_by_key(|m| {
            let cx = x.clamp(m.phys.0, m.phys.2 - 1);
            let cy = y.clamp(m.phys.1, m.phys.3 - 1);
            (cx - x).abs() as i64 + (cy - y).abs() as i64
        })
    })
}

fn is_ours(flags: u32, extra: usize) -> bool {
    flags & LLMHF_INJECTED != 0 && extra == INJECTED_MARK
}

fn dispatch(ev: CapturedEvent) -> Verdict {
    catch_unwind(AssertUnwindSafe(|| {
        let mut guard = HANDLER.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(h) => h(ev),
            None => Verdict::Pass,
        }
    }))
    .unwrap_or_else(|_| {
        set_cursor_captured(false, None);
        Verdict::Pass
    })
}

// ---------------------------------------------------------------------------
// 捕获
// ---------------------------------------------------------------------------

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: 低级鼠标钩子的 lparam 指向系统提供的 MSLLHOOKSTRUCT，回调期间有效。
        let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if !is_ours(info.flags, info.dwExtraInfo)
            && let Some(ev) = translate_mouse(wparam as u32, info)
            && dispatch(ev) == Verdict::Suppress
        {
            return 1;
        }
    }
    // SAFETY: 按约定把事件交给下一个钩子。
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

fn translate_mouse(msg: u32, info: &MSLLHOOKSTRUCT) -> Option<CapturedEvent> {
    let wheel = || (info.mouseData >> 16) as u16 as i16 as i32;
    let xbutton = || match (info.mouseData >> 16) as u16 {
        XBUTTON1 => Some(MouseButton::Back),
        XBUTTON2 => Some(MouseButton::Forward),
        _ => None,
    };
    Some(match msg {
        WM_MOUSEMOVE => {
            let pt = info.pt;
            // 光标锁定时以锚点为基准；否则以移动前的光标位置为基准（钩子在系统移动光标之前调用）。
            let base = if CAPTURED.load(Ordering::Relaxed) {
                *ANCHOR.lock().unwrap_or_else(|e| e.into_inner())
            } else {
                let mut cur = POINT { x: pt.x, y: pt.y };
                // SAFETY: 输出指针指向本地变量。
                unsafe { GetCursorPos(&mut cur) };
                (cur.x, cur.y)
            };
            with_monitors(|list| {
                let m = monitor_at(list, base.0, base.1)?;
                let (x, y) = m.phys_to_logical(base.0 as f64, base.1 as f64);
                Some(CapturedEvent::MouseMove {
                    x,
                    y,
                    dx: (pt.x - base.0) as f64 / m.scale,
                    dy: (pt.y - base.1) as f64 / m.scale,
                })
            })?
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP => CapturedEvent::Button {
            button: MouseButton::Left,
            down: msg == WM_LBUTTONDOWN,
        },
        WM_RBUTTONDOWN | WM_RBUTTONUP => CapturedEvent::Button {
            button: MouseButton::Right,
            down: msg == WM_RBUTTONDOWN,
        },
        WM_MBUTTONDOWN | WM_MBUTTONUP => CapturedEvent::Button {
            button: MouseButton::Middle,
            down: msg == WM_MBUTTONDOWN,
        },
        WM_XBUTTONDOWN | WM_XBUTTONUP => CapturedEvent::Button {
            button: xbutton()?,
            down: msg == WM_XBUTTONDOWN,
        },
        WM_MOUSEWHEEL => CapturedEvent::Wheel { dx: 0, dy: wheel() },
        WM_MOUSEHWHEEL => CapturedEvent::Wheel { dx: wheel(), dy: 0 },
        _ => return None,
    })
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: 低级键盘钩子的 lparam 指向系统提供的 KBDLLHOOKSTRUCT，回调期间有效。
        let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let injected = info.flags & 0x10 != 0; // LLKHF_INJECTED
        if !(injected && info.dwExtraInfo == INJECTED_MARK) {
            let down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
            let extended = info.flags & LLKHF_EXTENDED != 0;
            if let Some(code) = keymap::from_win(info.scanCode as u16, extended) {
                let repeat = {
                    let mut guard = PRESSED.lock().unwrap_or_else(|e| e.into_inner());
                    let set = guard.get_or_insert_with(HashSet::new);
                    if down {
                        !set.insert(code)
                    } else {
                        set.remove(&code);
                        false
                    }
                };
                if dispatch(CapturedEvent::Key { code, down, repeat }) == Verdict::Suppress {
                    return 1;
                }
            }
        }
    }
    // SAFETY: 按约定把事件交给下一个钩子。
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[derive(Default)]
pub struct WinCapture {
    thread_id: Option<u32>,
    thread: Option<JoinHandle<()>>,
}

impl InputCapture for WinCapture {
    fn start(&mut self, handler: CaptureHandler) -> Result<(), PlatformError> {
        self.stop();
        if let Ok(list) = monitors() {
            update_monitors(list);
        }
        *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
        let (tx, rx) = mpsc::channel::<Result<u32, PlatformError>>();
        let thread = std::thread::Builder::new()
            .name("flowacross-input-hook".into())
            .spawn(move || run_hooks(tx))
            .map_err(|e| PlatformError::Os(format!("无法创建输入线程：{e}")))?;
        match rx.recv() {
            Ok(Ok(id)) => {
                self.thread_id = Some(id);
                self.thread = Some(thread);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = None;
                Err(e)
            }
            Err(_) => Err(PlatformError::Os("输入线程意外退出".into())),
        }
    }

    fn stop(&mut self) {
        if let Some(id) = self.thread_id.take() {
            // SAFETY: 向钩子线程投递退出消息，线程结束消息循环后卸载钩子。
            unsafe { PostThreadMessageW(id, WM_QUIT, 0, 0) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = None;
        set_cursor_captured(false, None);
    }
}

impl Drop for WinCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Hooks {
    mouse: HHOOK,
    keyboard: HHOOK,
}

impl Hooks {
    /// SAFETY: 必须在钩子线程上调用。
    unsafe fn install() -> Option<Hooks> {
        // SAFETY: 由调用方保证在钩子线程上。
        unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
            let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
            let hooks = Hooks { mouse, keyboard };
            if mouse.is_null() || keyboard.is_null() {
                hooks.remove();
                return None;
            }
            Some(hooks)
        }
    }

    /// SAFETY: 必须在安装钩子的线程上调用。
    unsafe fn remove(&self) {
        // SAFETY: 由调用方保证。
        unsafe {
            if !self.mouse.is_null() {
                UnhookWindowsHookEx(self.mouse);
            }
            if !self.keyboard.is_null() {
                UnhookWindowsHookEx(self.keyboard);
            }
        }
    }
}

unsafe extern "system" fn foreground_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: windows_sys::Win32::Foundation::HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    // 立即重装一次，再延迟重装一次（新窗口的组件可能稍后才装它们的钩子）。
    request_reinstall();
    // SAFETY: 线程定时器，到期时向本线程投递 WM_TIMER。
    unsafe { SetTimer(std::ptr::null_mut(), 0, REINSTALL_DELAY_MS, None) };
}

fn run_hooks(ready: mpsc::Sender<Result<u32, PlatformError>>) {
    // SAFETY: 钩子在本线程安装和卸载；消息循环驱动钩子回调。
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        // 先建立本线程的消息队列，保证之后的 PostThreadMessageW 能送达。
        PeekMessageW(
            &mut msg,
            std::ptr::null_mut(),
            WM_USER,
            WM_USER,
            PM_NOREMOVE,
        );
        let Some(mut hooks) = Hooks::install() else {
            let _ = ready.send(Err(PlatformError::Os("安装键鼠钩子失败".into())));
            return;
        };
        let foreground = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(foreground_changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let thread = GetCurrentThreadId();
        HOOK_THREAD.store(thread, Ordering::Relaxed);
        // 常驻定时器：只在控制远端期间触发重装。
        let keepalive = SetTimer(std::ptr::null_mut(), 0, KEEPALIVE_MS, None);
        let _ = ready.send(Ok(thread));
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            let reinstall = match msg.message {
                WM_REINSTALL => true,
                WM_TIMER if msg.hwnd.is_null() && keepalive != 0 && msg.wParam == keepalive => {
                    CAPTURED.load(Ordering::Relaxed)
                }
                WM_TIMER if msg.hwnd.is_null() => {
                    KillTimer(std::ptr::null_mut(), msg.wParam);
                    true
                }
                _ => false,
            };
            if reinstall {
                // 先装新的再卸旧的，中间不会漏掉事件。
                if let Some(new) = Hooks::install() {
                    hooks.remove();
                    hooks = new;
                }
            }
        }
        HOOK_THREAD.store(0, Ordering::Relaxed);
        if keepalive != 0 {
            KillTimer(std::ptr::null_mut(), keepalive);
        }
        if !foreground.is_null() {
            UnhookWinEvent(foreground);
        }
        hooks.remove();
    }
}

// ---------------------------------------------------------------------------
// 光标锁定
// ---------------------------------------------------------------------------

/// 光标锁定点离屏幕边缘至少保留的距离（逻辑点）。
///
/// 低级钩子上报的坐标会被限制在屏幕内，锁定点贴着边缘时，往外的位移就测不到了。
/// 每次移动事件的位移都是相对锁定点计算的（移动被拦截，光标不会真的动），
/// 单个事件的位移远小于这个距离，所以留出这段余量就够了。
const ANCHOR_MARGIN: f64 = 120.0;

/// 在显示器 `phys`（左、上、右、下，物理像素，右下不含）内找离 `(x, y)` 最近、
/// 且离四边至少 `margin` 像素的点；显示器太小时取该方向的中点。
fn anchor_near(phys: (i32, i32, i32, i32), x: i32, y: i32, margin: i32) -> (i32, i32) {
    let axis = |lo: i32, hi: i32, v: i32| {
        let (min, max) = (lo + margin, hi - 1 - margin);
        if min > max {
            (lo + hi) / 2
        } else {
            v.clamp(min, max)
        }
    };
    (axis(phys.0, phys.2, x), axis(phys.1, phys.3, y))
}

pub fn set_cursor_captured(captured: bool, restore_at: Option<(f64, f64)>) {
    if captured {
        // 停在光标离开时的位置附近（往屏幕内收一点），而不是跳到屏幕中央。
        let mut cur = POINT { x: 0, y: 0 };
        // SAFETY: 输出指针指向本地变量。
        let have_cursor = unsafe { GetCursorPos(&mut cur) } != 0;
        let anchor = with_monitors(|list| {
            let m = if have_cursor {
                monitor_at(list, cur.x, cur.y)
            } else {
                list.iter().find(|m| m.primary).or(list.first())
            }?;
            let (x, y) = if have_cursor {
                (cur.x, cur.y)
            } else {
                ((m.phys.0 + m.phys.2) / 2, (m.phys.1 + m.phys.3) / 2)
            };
            let margin = (ANCHOR_MARGIN * m.scale).round() as i32;
            Some(anchor_near(m.phys, x, y, margin))
        });
        if let Some((x, y)) = anchor {
            *ANCHOR.lock().unwrap_or_else(|e| e.into_inner()) = (x, y);
            CAPTURED.store(true, Ordering::Relaxed);
            // SAFETY: 简单的系统调用。
            unsafe { SetCursorPos(x, y) };
            // 开始控制远端：确保我们的钩子排在最前面，按键不会被其他钩子截走。
            request_reinstall();
        }
    } else {
        CAPTURED.store(false, Ordering::Relaxed);
        if let Some((x, y)) = restore_at {
            let target = with_monitors(|list| {
                let p = flow_core::Point::new(x, y);
                // 落在显示器边界上时 contains 可能不成立，取距离最近的显示器（logical_to_phys 会夹到屏幕内）。
                list.iter()
                    .find(|m| m.logical.contains(p))
                    .or_else(|| {
                        list.iter().min_by(|a, b| {
                            let da = a.logical.clamp(p);
                            let db = b.logical.clamp(p);
                            let d = |q: flow_core::Point| (q.x - x).abs() + (q.y - y).abs();
                            d(da).total_cmp(&d(db))
                        })
                    })
                    .map(|m| m.logical_to_phys(x, y))
            });
            if let Some((px, py)) = target {
                // SAFETY: 简单的系统调用。
                unsafe { SetCursorPos(px, py) };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 注入
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct WinInjector;

fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_MARK,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), PlatformError> {
    // SAFETY: inputs 是有效的 INPUT 数组，cbSize 为单个结构体大小。
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(PlatformError::Os(
            "SendInput 被系统拦截（目标窗口可能以管理员身份运行）".into(),
        ))
    }
}

impl InputInjector for WinInjector {
    fn move_to(&mut self, display_local_id: &str, x: f64, y: f64) -> Result<(), PlatformError> {
        let mut target = with_monitors(|list| {
            list.iter()
                .find(|m| m.id == display_local_id)
                .map(|m| m.logical_to_phys(m.logical.x + x, m.logical.y + y))
        });
        if target.is_none()
            && let Ok(list) = monitors()
        {
            // 显示器可能刚插拔过，刷新一次再找。
            target = list
                .iter()
                .find(|m| m.id == display_local_id)
                .map(|m| m.logical_to_phys(m.logical.x + x, m.logical.y + y));
            update_monitors(list);
        }
        let Some((px, py)) = target else {
            return Ok(());
        };
        // SAFETY: 简单的系统查询。
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        // 绝对坐标归一化到 0..65535（覆盖整个虚拟桌面）。
        let nx = ((px - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
        let ny = ((py - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
        send(&[mouse_input(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
        };
        send(&[mouse_input(0, 0, data, flags)])
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        let mut inputs = Vec::new();
        if dy != 0 {
            inputs.push(mouse_input(0, 0, dy as u32, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            inputs.push(mouse_input(0, 0, dx as u32, MOUSEEVENTF_HWHEEL));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        send(&inputs)
    }

    fn key(&mut self, code: KeyCode, down: bool, _repeat: bool) -> Result<(), PlatformError> {
        let Some((scan, extended)) = keymap::to_win(code) else {
            return Ok(());
        };
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_MARK,
                },
            },
        }])
    }
}

// ---------------------------------------------------------------------------
// 剪贴板
// ---------------------------------------------------------------------------

const CF_UNICODETEXT: u32 = 13;

/// 打开剪贴板期间持有，离开作用域时关闭。
struct OpenClipboardGuard;

impl OpenClipboardGuard {
    /// 剪贴板可能正被其他程序占用，短暂重试几次。
    fn open() -> Result<Self, PlatformError> {
        use windows_sys::Win32::System::DataExchange::OpenClipboard;
        for _ in 0..10 {
            // SAFETY: 不指定所有者窗口打开剪贴板。
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(OpenClipboardGuard);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(PlatformError::Os("剪贴板正被其他程序占用".into()))
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: open() 成功后才会构造。
        unsafe { windows_sys::Win32::System::DataExchange::CloseClipboard() };
    }
}

/// 系统剪贴板（只处理 Unicode 文本）。
pub struct WinClipboard;

impl crate::ClipboardAccess for WinClipboard {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError> {
        use windows_sys::Win32::System::DataExchange::GetClipboardData;
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
        let _guard = OpenClipboardGuard::open()?;
        // SAFETY: 剪贴板已打开；数据句柄归剪贴板所有，只锁定读取，不释放。
        unsafe {
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                return Ok(None);
            }
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                return Ok(None);
            }
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            GlobalUnlock(handle);
            Ok(Some(text))
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        use windows_sys::Win32::Foundation::GlobalFree;
        use windows_sys::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
        use windows_sys::Win32::System::Memory::{
            GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
        };
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let _guard = OpenClipboardGuard::open()?;
        // SAFETY: 分配可移动内存并写入文本；SetClipboardData 成功后内存归系统所有，失败时自行释放。
        unsafe {
            EmptyClipboard();
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if mem.is_null() {
                return Err(PlatformError::Os("分配剪贴板内存失败".into()));
            }
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                GlobalFree(mem);
                return Err(PlatformError::Os("分配剪贴板内存失败".into()));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            GlobalUnlock(mem);
            if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                GlobalFree(mem);
                return Err(PlatformError::Os("写入剪贴板失败".into()));
            }
        }
        Ok(())
    }

    fn change_count(&self) -> u64 {
        // SAFETY: 无参数的系统查询。
        unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() as u64 }
    }
}

#[cfg(test)]
mod tests {
    use super::anchor_near;

    const SCREEN: (i32, i32, i32, i32) = (0, 0, 1920, 1080);

    #[test]
    fn anchor_stays_where_cursor_left_but_off_the_edge() {
        // 从右边缘离开：横向收回 120，纵向不动。
        assert_eq!(anchor_near(SCREEN, 1919, 500, 120), (1799, 500));
        // 从左上角离开：两个方向都收回。
        assert_eq!(anchor_near(SCREEN, 0, 0, 120), (120, 120));
        // 本来就离边缘够远：原地不动。
        assert_eq!(anchor_near(SCREEN, 800, 400, 120), (800, 400));
    }

    #[test]
    fn anchor_respects_monitor_offset() {
        // 主屏左侧的副屏，坐标为负。
        let left = (-2560, -200, 0, 1240);
        assert_eq!(anchor_near(left, -2560, 600, 180), (-2380, 600));
        assert_eq!(anchor_near(left, -1, 1239, 180), (-181, 1059));
    }

    #[test]
    fn tiny_monitor_falls_back_to_center() {
        assert_eq!(anchor_near((0, 0, 200, 1080), 0, 500, 120), (100, 500));
    }
}
