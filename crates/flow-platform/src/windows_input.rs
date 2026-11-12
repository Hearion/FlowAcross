//! Windows 键鼠捕获（低级键盘 / 鼠标钩子）与注入（SendInput）。
//!
//! - 钩子装在专用线程上，靠该线程的消息循环驱动；回调必须很快返回，
//!   否则系统会跳过钩子（默认超时约 300 毫秒）。
//! - 钩子里的坐标是物理像素，换算成和 `DisplayInfo` 一致的逻辑坐标后再上报。
//! - 控制远端时把光标停在离开处（屏幕边上），并拦截所有鼠标移动，只上报相对位移。
//! - 低级钩子按"后装先调用"的顺序执行，排在前面的钩子吃掉按键后我们就收不到了，
//!   所以开始控制远端时、以及前台窗口切换后，都把钩子重新装一次，保持在最前面。
//! - 进程注册了键盘 Raw Input 时，只要它自己的窗口在前台，Windows 就不再调用它的低级键盘钩子
//!   （实测）。Tauri 的窗口库 tao 默认会注册，所以外壳必须用 `DeviceEventFilter::Always` 关掉，
//!   否则流界窗口在前台时键盘到不了远端。
//! - 无法控制以管理员身份运行的窗口（系统的 UIPI 限制），除非流界也以管理员身份运行。

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
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
        // 停止期间错过的抬起（锁屏、重装钩子时按着键）不能留到下一次，否则那个键再按下会被当成连发。
        *PRESSED.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        let _ = ready.send(Ok(thread));
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            let reinstall = match msg.message {
                WM_REINSTALL => true,
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
        if !foreground.is_null() {
            UnhookWinEvent(foreground);
        }
        hooks.remove();
    }
}

// ---------------------------------------------------------------------------
// 光标锁定
// ---------------------------------------------------------------------------

/// 本机逻辑坐标 → 物理像素。落在显示器边界上时 contains 可能不成立，
/// 取距离最近的显示器（logical_to_phys 会夹到屏幕内）。
fn logical_to_phys_nearest(x: f64, y: f64) -> Option<(i32, i32)> {
    with_monitors(|list| {
        let p = flow_core::Point::new(x, y);
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
    })
}

/// `captured` 为 true 时 `at` 是离开处（光标锁定在那里，`None` 表示留在原处）；
/// 为 false 时 `at` 是回到本机时光标要放的位置。都是本机逻辑坐标。
pub fn set_cursor_captured(captured: bool, at: Option<(f64, f64)>) {
    if captured {
        // 光标停在离开处（屏幕边上）。低级钩子上报的坐标不会被限制在屏幕内（实测：光标停在右边缘时
        // 继续往右推，钩子里的 x 会超出屏幕），所以停在边上也能测到往外的位移。
        let mut cur = POINT { x: 0, y: 0 };
        let anchor = at
            .and_then(|(x, y)| logical_to_phys_nearest(x, y))
            .or_else(|| {
                // SAFETY: 输出指针指向本地变量。
                (unsafe { GetCursorPos(&mut cur) } != 0).then_some((cur.x, cur.y))
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
        if let Some((px, py)) = at.and_then(|(x, y)| logical_to_phys_nearest(x, y)) {
            // SAFETY: 简单的系统调用。
            unsafe { SetCursorPos(px, py) };
        }
    }
}

// ---------------------------------------------------------------------------
// 注入
// ---------------------------------------------------------------------------

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

/// 像素位置 → `MOUSEEVENTF_ABSOLUTE` 的 0..65535 坐标。系统按 `n * size / 65536` 向下取整换回像素，
/// 所以这里向上取整，换回来才是同一个像素（向下取整会经常偏左、偏上 1 像素）。
fn normalize_abs(offset: i32, size: i32) -> i32 {
    let size = size.max(1) as i64;
    ((offset as i64 * 65536 + size - 1) / size) as i32
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
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            )
        };
        let (nx, ny) = (normalize_abs(px - vx, vw), normalize_abs(py - vy, vh));
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
        // Pause 和 Num Lock 在钩子里报告的扫描码（0x45 / E0 45）和硬件扫描码语义相反：按扫描码注入时
        // 0x45 是 Num Lock，Pause 是 E1 1D 45 序列。这两个键改按虚拟键码注入。
        let vk: u16 = match code.0 {
            0x48 => 0x13, // VK_PAUSE
            0x53 => 0x90, // VK_NUMLOCK
            _ => 0,
        };
        let (scan, mut flags) = if vk != 0 {
            (0, 0)
        } else {
            (scan, KEYEVENTF_SCANCODE)
        };
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
                    wVk: vk,
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

/// 系统剪贴板（Unicode 文本、`CF_HDROP` 文件列表和图片）。
pub struct WinClipboard;

impl crate::ClipboardAccess for WinClipboard {
    fn get_text(&mut self) -> Result<Option<String>, PlatformError> {
        use windows_sys::Win32::System::DataExchange::GetClipboardData;
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        let _guard = OpenClipboardGuard::open()?;
        // SAFETY: 剪贴板已打开；数据句柄归剪贴板所有，只锁定读取，不释放；按 GlobalSize 限定读取范围
        // （别的程序放进来的文本不一定带结尾的 0）。
        unsafe {
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                return Ok(None);
            }
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                return Ok(None);
            }
            let max = GlobalSize(handle) / 2;
            let mut len = 0;
            while len < max && *ptr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            GlobalUnlock(handle);
            Ok(Some(text))
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        use windows_sys::Win32::System::DataExchange::EmptyClipboard;
        let bytes: Vec<u8> = text
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        let _guard = OpenClipboardGuard::open()?;
        // SAFETY: 剪贴板已打开。
        unsafe {
            EmptyClipboard();
            set_clipboard_bytes(CF_UNICODETEXT, &bytes)
        }
    }

    fn get_files(&mut self) -> Result<Option<Vec<PathBuf>>, PlatformError> {
        use windows_sys::Win32::System::DataExchange::{
            GetClipboardData, IsClipboardFormatAvailable,
        };
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        // SAFETY: 只查询格式，不需要打开剪贴板。
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
            return Ok(None);
        }
        let _guard = OpenClipboardGuard::open()?;
        // SAFETY: 剪贴板已打开；数据句柄归剪贴板所有，只锁定读取，不释放；按 GlobalSize 限定读取范围。
        unsafe {
            let handle = GetClipboardData(CF_HDROP);
            if handle.is_null() {
                return Ok(None);
            }
            let size = GlobalSize(handle);
            let ptr = GlobalLock(handle) as *const u8;
            if ptr.is_null() {
                return Ok(None);
            }
            let paths = parse_drop_files(std::slice::from_raw_parts(ptr, size));
            GlobalUnlock(handle);
            Ok(paths.filter(|p| !p.is_empty()))
        }
    }

    fn set_files(&mut self, paths: &[PathBuf]) -> Result<(), PlatformError> {
        use windows_sys::Win32::System::DataExchange::{EmptyClipboard, RegisterClipboardFormatW};
        let data = build_drop_files(paths);
        let name: Vec<u16> = "Preferred DropEffect"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let _guard = OpenClipboardGuard::open()?;
        // SAFETY: 剪贴板已打开；格式名是以 0 结尾的 UTF-16。
        unsafe {
            EmptyClipboard();
            set_clipboard_bytes(CF_HDROP, &data)?;
            // 告诉资源管理器这是「复制」而不是「剪切」，粘贴后不会删除接收文件夹里的文件。
            let effect = RegisterClipboardFormatW(name.as_ptr());
            if effect != 0 {
                let _ = set_clipboard_bytes(effect, &DROPEFFECT_COPY.to_le_bytes());
            }
        }
        Ok(())
    }

    fn get_image(&mut self) -> Result<Option<Vec<u8>>, PlatformError> {
        use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;
        let png = png_format();
        // SAFETY: 只查询格式，不需要打开剪贴板。
        let (has_png, has_dib) = unsafe {
            (
                png != 0 && IsClipboardFormatAvailable(png) != 0,
                IsClipboardFormatAvailable(CF_DIB) != 0,
            )
        };
        if !has_png && !has_dib {
            return Ok(None);
        }
        // 先把数据复制出来就关闭剪贴板，转换不占着它。
        let (png_data, dib_data) = {
            let _guard = OpenClipboardGuard::open()?;
            // SAFETY: 剪贴板已打开。
            unsafe {
                let png_data = if has_png {
                    get_clipboard_bytes(png)
                } else {
                    None
                };
                // 浏览器、截图工具会同时放一份 PNG（保留透明度）；没有再读系统位图。
                let dib_data = if png_data.is_none() {
                    get_clipboard_bytes(CF_DIB)
                } else {
                    None
                };
                (png_data, dib_data)
            }
        };
        Ok(png_data.or_else(|| dib_data.and_then(|d| crate::dib::dib_to_png(&d))))
    }

    fn set_image(&mut self, image: &crate::ClipboardImage) -> Result<(), PlatformError> {
        use windows_sys::Win32::System::DataExchange::EmptyClipboard;
        let format = png_format();
        let _guard = OpenClipboardGuard::open()?;
        // SAFETY: 剪贴板已打开。
        unsafe {
            EmptyClipboard();
            let mut written = false;
            // 画图和多数聊天软件只认位图；浏览器、Office 优先用 PNG（保留透明度）。
            if let Some(dib) = &image.dib {
                set_clipboard_bytes(CF_DIB, dib)?;
                written = true;
            }
            if format != 0 && set_clipboard_bytes(format, image.png()).is_ok() {
                written = true;
            }
            if written {
                Ok(())
            } else {
                Err(PlatformError::Os("写入剪贴板失败".into()))
            }
        }
    }

    fn change_count(&self) -> u64 {
        // SAFETY: 无参数的系统查询。
        unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() as u64 }
    }
}

const CF_DIB: u32 = 8;

/// 浏览器、Office、截图工具约定俗成的 "PNG" 剪贴板格式；注册失败时返回 0。
fn png_format() -> u32 {
    let name: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: 格式名是以 0 结尾的 UTF-16。
    unsafe { windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW(name.as_ptr()) }
}

/// 读出剪贴板里某种格式的全部字节。调用前必须已打开剪贴板。
///
/// # Safety
/// 调用方持有 [`OpenClipboardGuard`]。
unsafe fn get_clipboard_bytes(format: u32) -> Option<Vec<u8>> {
    use windows_sys::Win32::System::DataExchange::GetClipboardData;
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    // SAFETY: 数据句柄归剪贴板所有，只锁定读取，不释放；按 GlobalSize 限定读取范围。
    unsafe {
        let handle = GetClipboardData(format);
        if handle.is_null() {
            return None;
        }
        let size = GlobalSize(handle);
        let ptr = GlobalLock(handle) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
        GlobalUnlock(handle);
        Some(bytes)
    }
}

const CF_HDROP: u32 = 15;
const DROPEFFECT_COPY: u32 = 1;
/// `DROPFILES` 结构（shlobj_core.h）的大小：pFiles(4) + pt(8) + fNC(4) + fWide(4)。
const DROPFILES_LEN: usize = 20;

/// 把数据复制到新分配的可移动内存并放进剪贴板。调用前必须已打开剪贴板。
///
/// # Safety
/// 调用方持有 [`OpenClipboardGuard`]。
unsafe fn set_clipboard_bytes(format: u32, data: &[u8]) -> Result<(), PlatformError> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::SetClipboardData;
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    // SAFETY: SetClipboardData 成功后内存归系统所有，失败时自行释放。
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len());
        if mem.is_null() {
            return Err(PlatformError::Os("分配剪贴板内存失败".into()));
        }
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            GlobalFree(mem);
            return Err(PlatformError::Os("分配剪贴板内存失败".into()));
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        GlobalUnlock(mem);
        if SetClipboardData(format, mem).is_null() {
            GlobalFree(mem);
            return Err(PlatformError::Os("写入剪贴板失败".into()));
        }
    }
    Ok(())
}

/// 生成 `CF_HDROP` 数据：`DROPFILES` 头 + 每个路径的 UTF-16（以 0 结尾）+ 末尾再一个 0。
fn build_drop_files(paths: &[PathBuf]) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut out = Vec::new();
    out.extend_from_slice(&(DROPFILES_LEN as u32).to_le_bytes()); // pFiles
    out.extend_from_slice(&[0; 8]); // pt
    out.extend_from_slice(&0u32.to_le_bytes()); // fNC
    out.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        for u in p.as_os_str().encode_wide().chain(std::iter::once(0)) {
            out.extend_from_slice(&u.to_le_bytes());
        }
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// 解析 `CF_HDROP` 数据，格式不对时返回 `None`。
fn parse_drop_files(bytes: &[u8]) -> Option<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let offset = word(0)? as usize;
    let wide = word(16)? != 0;
    let list = bytes.get(offset..)?;
    let mut paths = Vec::new();
    if wide {
        let units: Vec<u16> = list
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        for s in units.split(|&u| u == 0) {
            if s.is_empty() {
                break;
            }
            paths.push(PathBuf::from(std::ffi::OsString::from_wide(s)));
        }
    } else {
        // 很旧的程序才会放 ANSI 路径，这里按 UTF-8 尽量解析。
        for s in list.split(|&b| b == 0) {
            if s.is_empty() {
                break;
            }
            paths.push(PathBuf::from(String::from_utf8_lossy(s).into_owned()));
        }
    }
    Some(paths)
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;
    use crate::ClipboardAccess;

    #[test]
    fn drop_files_round_trip() {
        let paths = vec![
            PathBuf::from(r"C:\Users\小严\Downloads\流界\报告 (1).pdf"),
            PathBuf::from(r"D:\照片"),
        ];
        let data = build_drop_files(&paths);
        assert_eq!(parse_drop_files(&data), Some(paths));
        assert_eq!(parse_drop_files(&[1, 2, 3]), None);
        // 偏移量越界
        let mut bad = data.clone();
        bad[0] = 0xFF;
        bad[1] = 0xFF;
        assert_eq!(parse_drop_files(&bad), None);
    }

    /// 会改写系统剪贴板，默认不运行：cargo test -p flow-platform -- --ignored
    #[test]
    #[ignore]
    fn clipboard_files_round_trip() {
        let mut cb = WinClipboard;
        let dir = std::env::temp_dir();
        let count = cb.change_count();
        cb.set_files(std::slice::from_ref(&dir)).unwrap();
        assert!(cb.change_count() > count);
        assert_eq!(cb.get_files().unwrap(), Some(vec![dir]));
        cb.set_text("流界").unwrap();
        assert_eq!(cb.get_files().unwrap(), None);
    }
}
