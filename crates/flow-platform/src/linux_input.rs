//! Linux（X11）键鼠捕获、光标锁定和注入。
//!
//! **捕获**：在根窗口上选 XInput2 的原始事件（`XI_Raw*`），不管焦点在哪个窗口都能收到。
//! X11 不能像 macOS / Windows 那样逐个事件决定放行还是拦截，所以回调返回的 [`Verdict`] 在这里不起作用，
//! 拦截靠"抓取"实现：开始控制远端时（[`set_cursor_captured`]）抓住主键盘和主指针并隐藏光标，
//! 此后本机程序收不到任何键鼠事件；回到本机时放开。
//! 抓取期间，根窗口上选的原始事件不再送给抓取者自己（在 X.Org 上实测），所以鼠标的原始事件要写进抓取掩码。
//!
//! 由此带来两点和另外两个系统不同：
//! - 在本机按 Ctrl+Alt+←/→ 切换时，这次按键本机的前台程序也会收到（抓取在按键之后才生效）。
//! - 别的程序正抓着键鼠时（打开着的菜单、全屏游戏）抓不到，这期间每来一个事件重试一次。
//!
//! **位移**：相对设备（鼠标、触摸板）用原始事件里加速后的位移，贴着屏幕边继续推也有读数；
//! 绝对设备（虚拟机的鼠标集成、数位板）没有位移，用前后两次光标位置的差。
//!
//! **按键**：原始事件没有长按连发。抓取期间改用抓取送来的普通按键事件（带连发标记），
//! 这样控制远端时长按照常连发。
//!
//! **注入**：XTest。流界自己注入的事件来自 X 服务的 XTEST 虚拟设备，捕获时按来源设备跳过
//! （其他用 XTest 的工具，如 xdotool，注入的输入也因此不会被转发到远端）。
//! 长按连发交给本机 X 服务按本机的重复速度产生，对方发来的连发事件不注入。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use flow_core::keys::{KeyCode, MouseButton};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::{
    self, ConnectionExt as _, DeviceClassData, DeviceType, Fp3232, KeyEventFlags, ValuatorMode,
    XIEventMask,
};
use x11rb::protocol::xproto::{
    self, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, GrabStatus,
    Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::linux::{self, os_err};
use crate::{CaptureHandler, CapturedEvent, InputCapture, InputInjector, PlatformError, keymap};

/// XInput2 里表示"所有主设备"的设备号。
const ALL_MASTER_DEVICES: u16 = 1;
/// X11 键码 = evdev 键码 + 8。
const X_KEYCODE_OFFSET: u32 = 8;
/// 滚轮一格（和 Windows 的 `WHEEL_DELTA` 一致）。
const WHEEL_NOTCH: i32 = 120;

// ---------------------------------------------------------------------------
// 捕获
// ---------------------------------------------------------------------------

struct Shared {
    conn: RustConnection,
    root: Window,
    /// 只用来接收"该停了"的消息，把事件线程从等待里叫醒。
    wake: Window,
    wake_atom: xproto::Atom,
    pointer: u16,
    keyboard: u16,
    /// 引擎要求锁定（正在控制远端）。
    captured: AtomicBool,
    /// 已经抓住键盘和指针。`captured` 为真而它为假：被别的程序占着，等下一个事件重试。
    grabbed: AtomicBool,
    cursor_hidden: AtomicBool,
    stop: AtomicBool,
    /// 抓取和放开可能来自不同线程（事件线程重试、引擎线程断线恢复），串行执行。
    grab_lock: Mutex<()>,
    /// 接受 XTEST 设备的输入。只有测试用：Xvfb 里没有真实的键鼠，只能用 XTest 模拟。
    accept_xtest: bool,
}

/// 正在运行的捕获。[`set_cursor_captured`] 通过它抓取和放开。
static ACTIVE: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

fn active() -> Option<Arc<Shared>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

impl Shared {
    fn grab_device(&self, device: u16, mask: XIEventMask) -> bool {
        self.conn
            .xinput_xi_grab_device(
                self.root,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                device,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                xinput::GrabOwner::NO_OWNER,
                &[u32::from(mask)],
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.status == GrabStatus::SUCCESS)
    }

    fn ungrab_devices(&self) {
        let _ = self
            .conn
            .xinput_xi_ungrab_device(x11rb::CURRENT_TIME, self.keyboard);
        let _ = self
            .conn
            .xinput_xi_ungrab_device(x11rb::CURRENT_TIME, self.pointer);
    }

    /// 抓住主键盘和主指针并隐藏光标。两样必须都抓到，只抓到一样就放开，等下次重试。
    fn try_grab(&self) -> bool {
        let _g = self.grab_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.captured.load(Ordering::SeqCst) {
            return false;
        }
        if self.grabbed.load(Ordering::SeqCst) {
            return true;
        }
        // 抓取期间，根窗口上选的原始事件不再送给抓取者自己（实测），要的事件必须写在抓取掩码里。
        let keys = XIEventMask::KEY_PRESS | XIEventMask::KEY_RELEASE;
        let pointer = XIEventMask::RAW_MOTION
            | XIEventMask::RAW_BUTTON_PRESS
            | XIEventMask::RAW_BUTTON_RELEASE;
        let ok = self.grab_device(self.keyboard, keys) && self.grab_device(self.pointer, pointer);
        if ok {
            if !self.cursor_hidden.swap(true, Ordering::SeqCst) {
                let _ = self.conn.xfixes_hide_cursor(self.root);
            }
            self.grabbed.store(true, Ordering::SeqCst);
        } else {
            self.ungrab_devices();
        }
        let _ = self.conn.flush();
        ok
    }

    /// 放开键鼠、恢复光标，可选把光标放到指定的根窗口像素位置。
    fn release(&self, warp_to: Option<(i32, i32)>) {
        let _g = self.grab_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.captured.store(false, Ordering::SeqCst);
        if let Some((x, y)) = warp_to {
            let _ = self.conn.warp_pointer(
                x11rb::NONE,
                self.root,
                0,
                0,
                0,
                0,
                x.clamp(0, i32::from(i16::MAX)) as i16,
                y.clamp(0, i32::from(i16::MAX)) as i16,
            );
        }
        if self.grabbed.swap(false, Ordering::SeqCst) {
            self.ungrab_devices();
        }
        if self.cursor_hidden.swap(false, Ordering::SeqCst) {
            let _ = self.conn.xfixes_show_cursor(self.root);
        }
        // 等 X 服务处理完再返回：返回时键鼠确实已经还给本机、光标已经在新位置上。
        let _ = self.conn.get_input_focus().map(|c| c.reply());
    }

    fn pointer_position(&self) -> Option<(f64, f64)> {
        let r = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((f64::from(r.root_x), f64::from(r.root_y)))
    }
}

/// 控制远端期间抓住键鼠、隐藏光标；结束时放开，并把光标放到 `restore_at`（本机系统坐标，逻辑点）。
/// 捕获没在运行时无操作。
pub fn set_cursor_captured(captured: bool, restore_at: Option<(f64, f64)>) {
    let Some(shared) = active() else {
        return;
    };
    if captured {
        shared.captured.store(true, Ordering::SeqCst);
        // 抓不到（别的程序占着）时事件线程会在后续事件里重试。
        shared.try_grab();
    } else {
        let at = restore_at.and_then(|(x, y)| linux::logical_to_phys_nearest(x, y));
        shared.release(at);
    }
}

#[derive(Default)]
pub struct X11Capture {
    shared: Option<Arc<Shared>>,
    thread: Option<JoinHandle<()>>,
    accept_xtest: bool,
}

impl X11Capture {
    /// 连 XTEST 设备的输入也上报。只给测试用。
    #[cfg(test)]
    fn accepting_xtest() -> Self {
        let mut capture = Self::default();
        capture.accept_xtest = true;
        capture
    }
}

impl InputCapture for X11Capture {
    fn start(&mut self, handler: CaptureHandler) -> Result<(), PlatformError> {
        self.stop();
        let shared = Arc::new(open(self.accept_xtest)?);
        if let Ok(list) = linux::monitors_on(&shared.conn, 0) {
            linux::update_monitors(list);
        }
        let for_thread = shared.clone();
        let thread = std::thread::Builder::new()
            .name("flowacross-x11-input".into())
            .spawn(move || run(for_thread, handler))
            .map_err(|e| os_err("无法创建输入线程", e))?;
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(shared.clone());
        self.shared = Some(shared);
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        let Some(shared) = self.shared.take() else {
            return;
        };
        {
            let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
            if active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &shared)) {
                *active = None;
            }
        }
        // 先恢复本机输入，再停线程。
        shared.release(None);
        shared.stop.store(true, Ordering::SeqCst);
        let msg = ClientMessageEvent::new(32, shared.wake, shared.wake_atom, [0u32; 5]);
        let _ = shared
            .conn
            .send_event(false, shared.wake, EventMask::NO_EVENT, msg);
        let _ = shared.conn.flush();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 连接 X 服务，确认需要的扩展都在，选好原始事件。
fn open(accept_xtest: bool) -> Result<Shared, PlatformError> {
    let (conn, screen) = linux::connect()?;
    let root = conn.setup().roots[screen].root;

    conn.xinput_xi_query_version(2, 2)
        .map_err(|e| os_err("XInput2 不可用", e))?
        .reply()
        .map_err(|e| os_err("XInput2 不可用（需要 X.Org 1.12 以上）", e))?;
    conn.xfixes_query_version(5, 0)
        .map_err(|e| os_err("XFixes 不可用", e))?
        .reply()
        .map_err(|e| os_err("XFixes 不可用", e))?;

    let masters = conn
        .xinput_xi_query_device(ALL_MASTER_DEVICES)
        .map_err(|e| os_err("查询输入设备失败", e))?
        .reply()
        .map_err(|e| os_err("查询输入设备失败", e))?;
    let master = masters
        .infos
        .iter()
        .find(|d| d.type_ == DeviceType::MASTER_POINTER)
        .ok_or_else(|| PlatformError::Os("X 服务没有主指针设备".into()))?;
    let (pointer, keyboard) = (master.deviceid, master.attachment);

    let wake = conn.generate_id().map_err(|e| os_err("创建窗口失败", e))?;
    conn.create_window(
        0,
        wake,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(|e| os_err("创建窗口失败", e))?;
    let wake_atom = conn
        .intern_atom(false, b"FLOWACROSS_WAKE")
        .map_err(|e| os_err("注册消息类型失败", e))?
        .reply()
        .map_err(|e| os_err("注册消息类型失败", e))?
        .atom;

    let raw = XIEventMask::RAW_MOTION
        | XIEventMask::RAW_BUTTON_PRESS
        | XIEventMask::RAW_BUTTON_RELEASE
        | XIEventMask::RAW_KEY_PRESS
        | XIEventMask::RAW_KEY_RELEASE;
    // 层级变化（插拔设备）事件只能在"所有设备"上选，原始事件选在主设备上，每个物理事件只收到一份。
    conn.xinput_xi_select_events(
        root,
        &[
            xinput::EventMask {
                deviceid: ALL_MASTER_DEVICES,
                mask: vec![raw],
            },
            xinput::EventMask {
                deviceid: 0,
                mask: vec![XIEventMask::HIERARCHY],
            },
        ],
    )
    .map_err(|e| os_err("监听键鼠失败", e))?
    .check()
    .map_err(|e| os_err("监听键鼠失败", e))?;
    conn.flush().map_err(|e| os_err("监听键鼠失败", e))?;

    Ok(Shared {
        conn,
        root,
        wake,
        wake_atom,
        pointer,
        keyboard,
        captured: AtomicBool::new(false),
        grabbed: AtomicBool::new(false),
        cursor_hidden: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        grab_lock: Mutex::new(()),
        accept_xtest,
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct Device {
    /// X 服务的 XTEST 虚拟设备：流界（和其他 XTest 工具）注入的输入从这里出来。
    xtest: bool,
    /// X / Y 轴上报绝对坐标（虚拟机鼠标集成、数位板、触摸屏）。
    absolute: bool,
}

fn query_devices(conn: &RustConnection) -> HashMap<u16, Device> {
    let Some(reply) = conn
        .xinput_xi_query_device(0u16)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return HashMap::new();
    };
    reply
        .infos
        .into_iter()
        .map(|d| {
            let name = String::from_utf8_lossy(&d.name).into_owned();
            let absolute = d.classes.iter().any(|c| match &c.data {
                DeviceClassData::Valuator(v) => v.number == 0 && v.mode == ValuatorMode::ABSOLUTE,
                _ => false,
            });
            let dev = Device {
                xtest: name.contains("XTEST"),
                absolute,
            };
            (d.deviceid, dev)
        })
        .collect()
}

/// X 服务里大写锁定是否打开（指针查询的修饰键掩码里带着锁定位）。
fn caps_lock_on(conn: &RustConnection, root: Window) -> Option<bool> {
    let reply = conn.query_pointer(root).ok()?.reply().ok()?;
    Some(reply.mask.contains(xproto::KeyButMask::LOCK))
}

fn fp3232(v: Fp3232) -> f64 {
    f64::from(v.integral) + f64::from(v.frac) / 4_294_967_296.0
}

/// 原始事件里 X（0 号轴）和 Y（1 号轴）的值；事件里没带的轴为 0。
fn xy_from_valuators(mask: &[u32], values: &[Fp3232]) -> (f64, f64) {
    let (mut x, mut y) = (0.0, 0.0);
    let mut next = values.iter();
    for (word, bits) in mask.iter().enumerate() {
        for bit in 0..32 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            let Some(v) = next.next() else {
                return (x, y);
            };
            match word * 32 + bit {
                0 => x = fp3232(*v),
                1 => y = fp3232(*v),
                _ => {}
            }
        }
    }
    (x, y)
}

/// X 的按钮号对应的事件：1/2/3 是左/中/右，8/9 是后退/前进，4–7 是滚轮（只在按下时算一格）。
fn button_event(detail: u32, down: bool) -> Option<CapturedEvent> {
    let button = match detail {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        8 => MouseButton::Back,
        9 => MouseButton::Forward,
        4..=7 => {
            if !down {
                return None;
            }
            let (dx, dy) = match detail {
                4 => (0, WHEEL_NOTCH),
                5 => (0, -WHEEL_NOTCH),
                6 => (-WHEEL_NOTCH, 0),
                _ => (WHEEL_NOTCH, 0),
            };
            return Some(CapturedEvent::Wheel { dx, dy });
        }
        _ => return None,
    };
    Some(CapturedEvent::Button { button, down })
}

fn key_code(detail: u32) -> Option<KeyCode> {
    let evdev = detail.checked_sub(X_KEYCODE_OFFSET)?;
    keymap::from_linux(u16::try_from(evdev).ok()?)
}

struct Loop {
    shared: Arc<Shared>,
    handler: CaptureHandler,
    devices: HashMap<u16, Device>,
    /// 上一次查询到的光标位置（根窗口像素）。
    last_pos: Option<(f64, f64)>,
    /// 最近一次已经上报的按键（X 键码、时间戳、是否按下）。同一次按键可能既有原始事件、
    /// 又有抓取送来的普通事件（抓取恰好在两者之间生效），靠它去重。
    last_key: Option<(u32, u32, bool)>,
    /// 抓住键盘后已经上报过的大写锁定状态；没抓住时为 `None`。
    caps_reported: Option<bool>,
    /// 大写锁定可能变了（刚抓住键盘，或按过 Caps Lock），下一轮查一次。
    caps_dirty: bool,
}

impl Loop {
    /// 抓着键盘（正在控制远端）时，把本机的大写锁定状态报给控制器，状态变了再报一次。
    fn sync_caps(&mut self) {
        if !self.shared.grabbed.load(Ordering::SeqCst) {
            self.caps_reported = None;
            self.caps_dirty = true;
            return;
        }
        if !self.caps_dirty {
            return;
        }
        self.caps_dirty = false;
        let Some(on) = caps_lock_on(&self.shared.conn, self.shared.root) else {
            return;
        };
        if self.caps_reported != Some(on) {
            self.caps_reported = Some(on);
            (self.handler)(CapturedEvent::CapsLock(on));
        }
    }

    fn device(&mut self, id: u16) -> Device {
        if !self.devices.contains_key(&id) {
            self.devices = query_devices(&self.shared.conn);
        }
        self.devices.get(&id).copied().unwrap_or_default()
    }

    /// 这个来源设备的输入要不要上报。
    fn wanted(&mut self, source: u16) -> Option<Device> {
        let dev = self.device(source);
        (!dev.xtest || self.shared.accept_xtest).then_some(dev)
    }

    fn motion(&mut self, source: u16, mask: &[u32], values: &[Fp3232]) {
        let Some(dev) = self.wanted(source) else {
            return;
        };
        let captured = self.shared.captured.load(Ordering::SeqCst);
        let scale = linux::scale();
        let (dx, dy, pos) = if dev.absolute {
            let Some(pos) = self.shared.pointer_position() else {
                return;
            };
            let (lx, ly) = self.last_pos.unwrap_or(pos);
            self.last_pos = Some(pos);
            (pos.0 - lx, pos.1 - ly, pos)
        } else {
            let (dx, dy) = xy_from_valuators(mask, values);
            // 锁定期间位置没有意义，省掉一次往返。
            let pos = if captured {
                self.last_pos.unwrap_or((0.0, 0.0))
            } else {
                let Some(pos) = self.shared.pointer_position() else {
                    return;
                };
                self.last_pos = Some(pos);
                pos
            };
            (dx, dy, pos)
        };
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let (dx, dy) = (dx / scale, dy / scale);
        // 查到的是移动后、被限制在屏幕内的位置，减去位移得到约定的"移动前的位置"（和 macOS 一样）。
        (self.handler)(CapturedEvent::MouseMove {
            x: pos.0 / scale - dx,
            y: pos.1 / scale - dy,
            dx,
            dy,
        });
    }

    fn button(&mut self, source: u16, detail: u32, down: bool) {
        if self.wanted(source).is_none() {
            return;
        }
        if let Some(ev) = button_event(detail, down) {
            (self.handler)(ev);
        }
    }

    fn key(&mut self, source: u16, detail: u32, time: u32, down: bool, repeat: bool) {
        if self.wanted(source).is_none() {
            return;
        }
        let stamp = (detail, time, down);
        if !repeat && self.last_key == Some(stamp) {
            return;
        }
        self.last_key = Some(stamp);
        if let Some(code) = key_code(detail) {
            if code == KeyCode::CAPS_LOCK {
                self.caps_dirty = true;
            }
            (self.handler)(CapturedEvent::Key { code, down, repeat });
        }
    }

    fn handle(&mut self, event: Event) {
        let grabbed = self.shared.grabbed.load(Ordering::SeqCst);
        match event {
            Event::XinputRawMotion(e) => self.motion(e.sourceid, &e.valuator_mask, &e.axisvalues),
            Event::XinputRawButtonPress(e) => self.button(e.sourceid, e.detail, true),
            Event::XinputRawButtonRelease(e) => self.button(e.sourceid, e.detail, false),
            // 没抓住键盘时按键看原始事件（没有连发，本机的按键也不需要连发）。
            Event::XinputRawKeyPress(e) if !grabbed => {
                self.key(e.sourceid, e.detail, e.time, true, false)
            }
            Event::XinputRawKeyRelease(e) if !grabbed => {
                self.key(e.sourceid, e.detail, e.time, false, false)
            }
            // 抓住键盘后看抓取送来的按键事件，它带长按连发标记。
            Event::XinputKeyPress(e) if grabbed => {
                let repeat = e.flags.contains(KeyEventFlags::KEY_REPEAT);
                self.key(e.sourceid, e.detail, e.time, true, repeat)
            }
            Event::XinputKeyRelease(e) if grabbed => {
                self.key(e.sourceid, e.detail, e.time, false, false)
            }
            Event::XinputHierarchy(_) => self.devices = query_devices(&self.shared.conn),
            _ => {}
        }
    }
}

fn run(shared: Arc<Shared>, handler: CaptureHandler) {
    let mut state = Loop {
        devices: query_devices(&shared.conn),
        last_pos: shared.pointer_position(),
        last_key: None,
        caps_reported: None,
        caps_dirty: true,
        handler,
        shared: shared.clone(),
    };
    // 连接断开（X 服务退出，通常意味着整个桌面会话结束）时结束。
    while let Ok(event) = shared.conn.wait_for_event() {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        state.handle(event);
        // 该锁定却没抓住（别的程序占着键鼠）：每来一个事件重试一次。
        if shared.captured.load(Ordering::SeqCst) && !shared.grabbed.load(Ordering::SeqCst) {
            shared.try_grab();
        }
        state.sync_caps();
    }
    shared.release(None);
}

// ---------------------------------------------------------------------------
// 注入
// ---------------------------------------------------------------------------

pub struct X11Injector {
    conn: RustConnection,
    root: Window,
    /// 还不够一格的滚动量（1/120 格）：横向、纵向。
    wheel_rest: (i32, i32),
}

impl X11Injector {
    pub fn new() -> Result<Self, PlatformError> {
        let (conn, screen) = linux::connect()?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)
            .map_err(|e| os_err("XTest 不可用", e))?
            .reply()
            .map_err(|e| os_err("XTest 不可用（X 服务没有开启 XTEST 扩展）", e))?;
        Ok(Self {
            conn,
            root,
            wheel_rest: (0, 0),
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), PlatformError> {
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map_err(|e| os_err("注入输入失败", e))?;
        Ok(())
    }

    fn flush(&self) -> Result<(), PlatformError> {
        self.conn.flush().map_err(|e| os_err("注入输入失败", e))?;
        // 这条连接不等任何回复：顺手把服务端发来的错误包读掉，免得积在缓冲区里。
        while let Ok(Some(_)) = self.conn.poll_for_event() {}
        Ok(())
    }

    fn click(&self, button: u8) -> Result<(), PlatformError> {
        self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
        self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)
    }
}

/// 把累计的滚动量拆成整格数和余量。
pub(crate) fn take_notches(rest: &mut i32, delta: i32) -> i32 {
    *rest = rest.saturating_add(delta);
    let notches = *rest / WHEEL_NOTCH;
    *rest -= notches * WHEEL_NOTCH;
    notches
}

impl InputInjector for X11Injector {
    fn set_caps_lock(&mut self, on: bool) -> Result<(), PlatformError> {
        if caps_lock_on(&self.conn, self.root) == Some(!on) {
            // 状态不一样：按一下 Caps Lock。
            self.key(KeyCode::CAPS_LOCK, true, false)?;
            self.key(KeyCode::CAPS_LOCK, false, false)?;
        }
        Ok(())
    }

    fn move_to(&mut self, display_local_id: &str, x: f64, y: f64) -> Result<(), PlatformError> {
        let find = |list: &[linux::Monitor]| {
            list.iter()
                .find(|m| m.id == display_local_id)
                .map(|m| m.logical_to_phys(m.logical.x + x, m.logical.y + y))
        };
        let mut target = linux::with_monitors(find);
        if target.is_none()
            && let Ok(list) = linux::x11_monitors()
        {
            // 显示器配置变了（插拔、改分辨率）：刷新后再找一次。
            target = find(&list);
            linux::update_monitors(list);
        }
        let Some((px, py)) = target else {
            return Ok(());
        };
        let clamp = |v: i32| v.clamp(0, i32::from(i16::MAX)) as i16;
        // detail 0 = 绝对坐标。
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, clamp(px), clamp(py))?;
        self.flush()
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let detail = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Back => 8,
            MouseButton::Forward => 9,
        };
        let kind = if down {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.fake(kind, detail, 0, 0)?;
        self.flush()
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        // X 的滚轮是按钮：4 上、5 下、6 左、7 右，一次点击一格。不足一格的先攒着。
        let h = take_notches(&mut self.wheel_rest.0, dx);
        let v = take_notches(&mut self.wheel_rest.1, dy);
        for _ in 0..v.unsigned_abs() {
            self.click(if v > 0 { 4 } else { 5 })?;
        }
        for _ in 0..h.unsigned_abs() {
            self.click(if h > 0 { 7 } else { 6 })?;
        }
        self.flush()
    }

    fn key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Result<(), PlatformError> {
        // 长按连发由本机 X 服务产生（注入的按键按住后同样会自动重复），再注入一遍会重复两次。
        if repeat {
            return Ok(());
        }
        let Some(evdev) = keymap::to_linux(code) else {
            return Ok(());
        };
        let Ok(detail) = u8::try_from(u32::from(evdev) + X_KEYCODE_OFFSET) else {
            return Ok(());
        };
        let kind = if down {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.fake(kind, detail, 0, 0)?;
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(v: f64) -> Fp3232 {
        let integral = v.floor();
        Fp3232 {
            integral: integral as i32,
            frac: ((v - integral) * 4_294_967_296.0) as u32,
        }
    }

    #[test]
    fn fixed_point_values_convert_both_signs() {
        assert_eq!(fp3232(fp(3.0)), 3.0);
        assert_eq!(fp3232(fp(-2.5)), -2.5);
        assert_eq!(fp3232(fp(0.25)), 0.25);
    }

    #[test]
    fn picks_x_and_y_out_of_the_valuator_mask() {
        // 只有 Y 轴和 3 号轴（滚轮）有值。
        assert_eq!(
            xy_from_valuators(&[0b1010], &[fp(-4.0), fp(15.0)]),
            (0.0, -4.0)
        );
        assert_eq!(xy_from_valuators(&[0b11], &[fp(2.0), fp(7.5)]), (2.0, 7.5));
        assert_eq!(xy_from_valuators(&[0b1], &[fp(-1.0)]), (-1.0, 0.0));
        // 掩码声明了值却没给：不越界，缺的当 0。
        assert_eq!(xy_from_valuators(&[0b11], &[fp(2.0)]), (2.0, 0.0));
        assert_eq!(xy_from_valuators(&[], &[]), (0.0, 0.0));
    }

    #[test]
    fn buttons_and_wheel_map_like_other_systems() {
        assert_eq!(
            button_event(1, true),
            Some(CapturedEvent::Button {
                button: MouseButton::Left,
                down: true
            })
        );
        assert_eq!(
            button_event(3, false),
            Some(CapturedEvent::Button {
                button: MouseButton::Right,
                down: false
            })
        );
        // 向上滚为正，向右滚为正；抬起不算。
        assert_eq!(
            button_event(4, true),
            Some(CapturedEvent::Wheel { dx: 0, dy: 120 })
        );
        assert_eq!(
            button_event(6, true),
            Some(CapturedEvent::Wheel { dx: -120, dy: 0 })
        );
        assert_eq!(button_event(5, false), None);
        assert_eq!(button_event(12, true), None);
    }

    #[test]
    fn x_keycodes_are_evdev_plus_eight() {
        assert_eq!(key_code(38), Some(KeyCode::A));
        assert_eq!(key_code(114), Some(KeyCode::RIGHT));
        assert_eq!(key_code(3), None);
    }

    #[test]
    fn partial_wheel_steps_accumulate() {
        let mut rest = 0;
        assert_eq!(take_notches(&mut rest, 60), 0);
        assert_eq!(take_notches(&mut rest, 60), 1);
        assert_eq!(rest, 0);
        assert_eq!(take_notches(&mut rest, -300), -2);
        assert_eq!(rest, -60);
        assert_eq!(take_notches(&mut rest, 60), 0);
        assert_eq!(rest, 0);
    }

    // 以下测试需要一个 X 服务（CI 和本机容器里用 Xvfb），没有 DISPLAY 时跳过。
    // 它们共用同一个光标和键盘，必须串行跑：`cargo test -p flow-platform -- --ignored --test-threads=1`。

    use crate::Verdict;
    use std::sync::mpsc;
    use std::time::Duration;

    fn recv_until(
        rx: &mpsc::Receiver<CapturedEvent>,
        mut want: impl FnMut(&CapturedEvent) -> bool,
    ) -> Option<CapturedEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ev) if want(&ev) => return Some(ev),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        None
    }

    fn start_capture() -> (X11Capture, mpsc::Receiver<CapturedEvent>) {
        let (tx, rx) = mpsc::channel();
        let mut cap = X11Capture::accepting_xtest();
        cap.start(Box::new(move |ev| {
            let _ = tx.send(ev);
            Verdict::Pass
        }))
        .expect("启动捕获");
        (cap, rx)
    }

    #[test]
    #[ignore = "需要 X 服务"]
    fn injected_pointer_lands_where_asked() {
        let displays = crate::display_provider().displays().expect("枚举显示器");
        let d = &displays[0];
        let mut inj = X11Injector::new().expect("注入器");
        inj.move_to(&d.local_id, 200.0, 120.0).unwrap();
        let (conn, screen) = linux::connect().unwrap();
        let root = conn.setup().roots[screen].root;
        let p = conn.query_pointer(root).unwrap().reply().unwrap();
        let scale = d.scale;
        assert_eq!(
            (f64::from(p.root_x), f64::from(p.root_y)),
            ((d.rect.x + 200.0) * scale, (d.rect.y + 120.0) * scale)
        );
    }

    #[test]
    #[ignore = "需要 X 服务"]
    fn capture_reports_keys_buttons_and_wheel() {
        let (mut cap, rx) = start_capture();
        let mut inj = X11Injector::new().unwrap();
        inj.key(KeyCode::A, true, false).unwrap();
        inj.key(KeyCode::A, false, false).unwrap();
        inj.button(MouseButton::Left, true).unwrap();
        inj.button(MouseButton::Left, false).unwrap();
        inj.wheel(0, -240).unwrap();

        let key = |down| CapturedEvent::Key {
            code: KeyCode::A,
            down,
            repeat: false,
        };
        assert!(recv_until(&rx, |e| *e == key(true)).is_some(), "按下 A");
        assert!(recv_until(&rx, |e| *e == key(false)).is_some(), "抬起 A");
        let left = |down| CapturedEvent::Button {
            button: MouseButton::Left,
            down,
        };
        assert!(recv_until(&rx, |e| *e == left(true)).is_some(), "按下左键");
        assert!(recv_until(&rx, |e| *e == left(false)).is_some(), "抬起左键");
        let down = CapturedEvent::Wheel { dx: 0, dy: -120 };
        assert!(recv_until(&rx, |e| *e == down).is_some(), "向下滚第一格");
        assert!(recv_until(&rx, |e| *e == down).is_some(), "向下滚第二格");
        cap.stop();
    }

    /// 用 XTest 注入一次相对移动（detail 1 = 相对坐标）。
    fn nudge_pointer(inj: &X11Injector, dx: i16, dy: i16) {
        inj.fake(xproto::MOTION_NOTIFY_EVENT, 1, dx, dy).unwrap();
        inj.flush().unwrap();
    }

    #[test]
    #[ignore = "需要 X 服务"]
    fn capture_reports_pointer_motion_locally_and_while_grabbed() {
        let displays = crate::display_provider().displays().expect("枚举显示器");
        let d = &displays[0];
        let scale = d.scale;
        let mut inj = X11Injector::new().unwrap();
        inj.move_to(&d.local_id, 400.0, 300.0).unwrap();
        let (mut cap, rx) = start_capture();
        // 等捕获线程记下起点，再丢掉启动前后残留的移动。
        std::thread::sleep(Duration::from_millis(200));
        while rx.try_recv().is_ok() {}

        nudge_pointer(&inj, 10, 6);
        let Some(CapturedEvent::MouseMove { x, y, dx, dy }) =
            recv_until(&rx, |e| matches!(e, CapturedEvent::MouseMove { .. }))
        else {
            panic!("本机移动没有上报");
        };
        assert_eq!((dx, dy), (10.0 / scale, 6.0 / scale));
        // 上报的是移动前的位置：加上位移就是光标现在的位置。
        assert_eq!(
            (x + dx, y + dy),
            (d.rect.x + 400.0 + dx, d.rect.y + 300.0 + dy)
        );

        // 锁定期间位置没有意义，位移照常上报。
        set_cursor_captured(true, None);
        nudge_pointer(&inj, -7, 3);
        let Some(CapturedEvent::MouseMove { dx, dy, .. }) =
            recv_until(&rx, |e| matches!(e, CapturedEvent::MouseMove { .. }))
        else {
            panic!("锁定期间的移动没有上报");
        };
        assert_eq!((dx, dy), (-7.0 / scale, 3.0 / scale));
        // 按钮和滚轮同样照常上报。
        inj.button(MouseButton::Left, true).unwrap();
        inj.button(MouseButton::Left, false).unwrap();
        inj.wheel(120, 0).unwrap();
        let left = |down| CapturedEvent::Button {
            button: MouseButton::Left,
            down,
        };
        assert!(
            recv_until(&rx, |e| *e == left(true)).is_some(),
            "锁定期间按下左键"
        );
        assert!(
            recv_until(&rx, |e| *e == left(false)).is_some(),
            "锁定期间抬起左键"
        );
        let right = CapturedEvent::Wheel { dx: 120, dy: 0 };
        assert!(recv_until(&rx, |e| *e == right).is_some(), "锁定期间向右滚");
        set_cursor_captured(false, None);
        cap.stop();
    }

    #[test]
    #[ignore = "需要 X 服务"]
    fn capture_ignores_injected_input_by_default() {
        let (tx, rx) = mpsc::channel();
        let mut cap = X11Capture::default();
        cap.start(Box::new(move |ev| {
            let _ = tx.send(ev);
            Verdict::Pass
        }))
        .unwrap();
        let mut inj = X11Injector::new().unwrap();
        inj.key(KeyCode(0x05), true, false).unwrap();
        inj.key(KeyCode(0x05), false, false).unwrap();
        inj.button(MouseButton::Right, true).unwrap();
        inj.button(MouseButton::Right, false).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "流界自己注入的输入不应该被当成本机输入"
        );
        cap.stop();
    }

    #[test]
    #[ignore = "需要 X 服务"]
    fn capturing_grabs_input_and_release_restores_cursor() {
        let displays = crate::display_provider().displays().expect("枚举显示器");
        let d = &displays[0];
        let (mut cap, rx) = start_capture();
        let shared = cap.shared.clone().unwrap();
        let mut inj = X11Injector::new().unwrap();

        set_cursor_captured(true, None);
        assert!(shared.grabbed.load(Ordering::SeqCst), "应该抓住键鼠");
        // 另一个客户端这时抓不到键盘，说明本机程序确实收不到输入。
        let (other, screen) = linux::connect().unwrap();
        let root = other.setup().roots[screen].root;
        let status = other
            .grab_keyboard(
                false,
                root,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .unwrap()
            .reply()
            .unwrap()
            .status;
        assert_eq!(status, GrabStatus::ALREADY_GRABBED);

        // 抓取期间按键仍然上报（走抓取送来的事件）。
        inj.key(KeyCode::C, true, false).unwrap();
        inj.key(KeyCode::C, false, false).unwrap();
        let key = |down| CapturedEvent::Key {
            code: KeyCode::C,
            down,
            repeat: false,
        };
        assert!(
            recv_until(&rx, |e| *e == key(true)).is_some(),
            "抓取期间按下 C"
        );
        assert!(
            recv_until(&rx, |e| *e == key(false)).is_some(),
            "抓取期间抬起 C"
        );
        // 同一次按键不能上报两遍。
        assert!(recv_until(&rx, |e| matches!(e, CapturedEvent::Key { .. })).is_none());

        // 大写锁定：对齐到指定状态（XTest 按一下 Caps Lock），捕获这边把新状态报出来。
        let caps = |on| move |e: &CapturedEvent| *e == CapturedEvent::CapsLock(on);
        inj.set_caps_lock(true).unwrap();
        assert!(
            recv_until(&rx, caps(true)).is_some(),
            "大写锁定打开后应该上报"
        );
        assert_eq!(caps_lock_on(&other, root), Some(true));
        inj.set_caps_lock(false).unwrap();
        assert!(
            recv_until(&rx, caps(false)).is_some(),
            "大写锁定关闭后应该上报"
        );
        assert_eq!(caps_lock_on(&other, root), Some(false));

        set_cursor_captured(false, Some((d.rect.x + 300.0, d.rect.y + 50.0)));
        assert!(!shared.grabbed.load(Ordering::SeqCst));
        let p = other.query_pointer(root).unwrap().reply().unwrap();
        assert_eq!(
            (f64::from(p.root_x), f64::from(p.root_y)),
            ((d.rect.x + 300.0) * d.scale, (d.rect.y + 50.0) * d.scale)
        );
        let status = other
            .grab_keyboard(
                false,
                root,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .unwrap()
            .reply()
            .unwrap()
            .status;
        assert_eq!(status, GrabStatus::SUCCESS, "放开后别的程序能拿到键盘");
        cap.stop();
    }

    #[test]
    #[ignore = "需要 X 服务"]
    fn stopping_capture_releases_the_grab() {
        let (mut cap, _rx) = start_capture();
        set_cursor_captured(true, None);
        cap.stop();
        assert!(active().is_none());
        let (other, screen) = linux::connect().unwrap();
        let root = other.setup().roots[screen].root;
        let status = other
            .grab_keyboard(
                false,
                root,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .unwrap()
            .reply()
            .unwrap()
            .status;
        assert_eq!(status, GrabStatus::SUCCESS);
    }
}
