//! Linux（Wayland，wlroots 系：Sway、Hyprland 等）键鼠注入、捕获和光标锁定。
//!
//! **注入**：`zwlr_virtual_pointer`（每块显示器建一个绑定到该输出的虚拟指针，坐标就是这块屏幕内的逻辑点）
//! 和 `zwp_virtual_keyboard`。虚拟键盘要自己上传键盘映射、自己上报修饰键状态：映射取本机键盘当前用的
//! 那份（没有接键盘时用系统默认布局），修饰键状态用 xkbcommon 跟着按键算。这样注入的是物理键位，
//! 字符仍由本机的布局和输入法决定。长按连发由收到按键的程序自己产生，对方发来的连发事件不注入。
//!
//! **捕获**：Wayland 的普通程序看不到全局的键鼠事件，也不知道光标在哪。做法和同类工具一样：
//! 在可以切到别的电脑的那些屏幕边上各放一条 1 个逻辑点宽的透明 layer-shell 窗口，光标碰到它就等于
//! "推到了边上"，这时合成一次向外推的移动交给控制器判断。控制器决定切换后（[`set_cursor_captured`]）：
//! 把这条窗口铺满整块屏幕并独占键盘焦点（不依赖合成器肯不肯锁定指针，见 `Shared::engage`），再锁定指针
//! （`pointer-constraints`）、用 `relative-pointer` 读位移、请合成器不再处理快捷键
//! （`keyboard-shortcuts-inhibit`）。回到本机时全部放开，再用虚拟指针把光标放到跨回来的位置。
//!
//! 和其他系统不同的地方：
//! - 没切到远端时看不到键盘，所以「按住修饰键才切换」和本机上的 Ctrl+Alt+←/→ 不起作用
//!   （切到远端之后键盘归流界，Ctrl+Alt+←/→ 切回来是可以的）。
//! - 边上那 1 个点宽的窗口会挡住屏幕最外一圈像素的点击，所以只放在通往别的电脑的边上
//!   （引擎通过 [`set_switch_edges`] 告知）。
//! - 事件不带来源：流界刚注入过鼠标移动的一小段时间内，光标碰到边上不算（它是被远端推过去的）。

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flow_core::geometry::Edge;
use flow_core::keys::{KeyCode, MouseButton};
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
    zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

use crate::linux::{self, Monitor, os_err};
use crate::linux_input::take_notches;
use crate::linux_wl::{
    self, anonymous_file, globals_of, mark_injected_move, now_ms, read_fd, recently_injected_move,
};
use crate::{CaptureHandler, CapturedEvent, InputCapture, InputInjector, PlatformError, keymap};

/// evdev 的鼠标按钮码。
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
/// 修饰键掩码里的大写锁定位。xkb 的八个"真实修饰键"顺序固定，Lock 是第 2 个。
const LOCK_MASK: u32 = 1 << 1;
/// xkb 的键码 = evdev 键码 + 8。
const XKB_OFFSET: u32 = 8;
/// 滚轮一格：协议里连续滚动量的 15 个单位，流界约定的 120 个单位。
const AXIS_PER_NOTCH: f64 = 15.0;
const WHEEL_NOTCH: i32 = 120;
/// 绝对坐标按 1/16 逻辑点上报，保留分数缩放下的精度。
const SUBPIXEL: f64 = 16.0;

pub(crate) fn button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

pub(crate) fn button_from_code(code: u32) -> Option<MouseButton> {
    Some(match code {
        BTN_LEFT => MouseButton::Left,
        BTN_RIGHT => MouseButton::Right,
        BTN_MIDDLE => MouseButton::Middle,
        BTN_SIDE => MouseButton::Back,
        BTN_EXTRA => MouseButton::Forward,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// 键盘映射
// ---------------------------------------------------------------------------

/// 虚拟键盘用的键盘映射和按键状态。
struct Keys {
    state: xkb::State,
    text: String,
}

// SAFETY: xkb 的对象没有线程亲和性，只是不能同时从多个线程使用；`Keys` 只在持有它的注入器里用，
// 而注入器同一时间只被一个线程使用（`InputInjector: Send`，引擎放在互斥锁里）。
unsafe impl Send for Keys {}

impl Keys {
    /// `text`：本机键盘当前的映射（合成器发来的）；没有时用系统默认布局。
    fn new(text: Option<String>) -> Result<Self, PlatformError> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = text
            .and_then(|t| {
                xkb::Keymap::new_from_string(
                    &ctx,
                    t,
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                )
            })
            .or_else(|| {
                xkb::Keymap::new_from_names(
                    &ctx,
                    "",
                    "",
                    "",
                    "",
                    None,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                )
            })
            .ok_or_else(|| {
                PlatformError::Os("无法生成键盘映射（系统缺少 xkb 键盘布局数据）".into())
            })?;
        Ok(Self {
            text: keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1),
            state: xkb::State::new(&keymap),
        })
    }

    /// 这块虚拟键盘的大写锁定是否打开。
    fn caps_locked(&self) -> bool {
        self.state
            .mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_LOCKED)
    }

    /// 记下一次按键，返回要上报的修饰键状态：(按住的, 粘滞的, 锁定的, 布局组)。
    fn update(&mut self, evdev: u32, down: bool) -> (u32, u32, u32, u32) {
        let direction = if down {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        self.state
            .update_key(xkb::Keycode::new(evdev + XKB_OFFSET), direction);
        (
            self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.state.serialize_mods(xkb::STATE_MODS_LATCHED),
            self.state.serialize_mods(xkb::STATE_MODS_LOCKED),
            self.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        )
    }
}

// ---------------------------------------------------------------------------
// 注入
// ---------------------------------------------------------------------------

/// 注入器连接上的状态：只关心本机键盘的映射。
#[derive(Default)]
struct InjectState {
    has_keyboard: bool,
    keymap: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for InjectState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for InjectState {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            state.has_keyboard = caps.contains(wl_seat::Capability::Keyboard);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for InjectState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } = event
            && let Ok(bytes) = read_fd(fd, size as usize)
        {
            let text = String::from_utf8_lossy(&bytes);
            state.keymap = Some(text.trim_end_matches('\0').to_owned());
        }
    }
}

delegate_noop!(InjectState: ignore wl_output::WlOutput);
delegate_noop!(InjectState: ZwlrVirtualPointerManagerV1);
delegate_noop!(InjectState: ZwlrVirtualPointerV1);
delegate_noop!(InjectState: ZwpVirtualKeyboardManagerV1);
delegate_noop!(InjectState: ZwpVirtualKeyboardV1);

pub struct WlrInjector {
    conn: Connection,
    queue: EventQueue<InjectState>,
    state: InjectState,
    globals: GlobalList,
    seat: wl_seat::WlSeat,
    pointers: ZwlrVirtualPointerManagerV1,
    /// 能不能把虚拟指针绑定到某块输出（协议第 2 版）。
    per_output: bool,
    outputs: HashMap<u32, wl_output::WlOutput>,
    /// 绑定到各输出的虚拟指针，按输出的全局编号。
    bound: HashMap<u32, ZwlrVirtualPointerV1>,
    /// 没绑定输出的虚拟指针：按钮、滚轮，以及不能绑定输出时的移动。
    plain: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    keys: Keys,
    wheel_rest: (i32, i32),
    started: Instant,
    /// 距离上次读取合成器发来的数据已经发了多少个请求。
    unread: u32,
}

impl WlrInjector {
    pub fn new() -> Result<Self, PlatformError> {
        let fail = |e: &dyn std::fmt::Display| os_err("初始化键鼠注入失败", e);
        let conn = linux_wl::connect()?;
        let (globals, mut queue): (GlobalList, EventQueue<InjectState>) =
            registry_queue_init(&conn).map_err(|e| fail(&e))?;
        let qh = queue.handle();
        let mut state = InjectState::default();
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=7, ())
            .map_err(|e| os_err("合成器没有输入席位（wl_seat）", e))?;
        let pointers: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|e| os_err("合成器不支持虚拟指针协议", e))?;
        let keyboards: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| os_err("合成器不支持虚拟键盘协议", e))?;
        let per_output = pointers.version() >= 2;

        // 取本机键盘当前的映射：先问有没有键盘，有就要来它的映射。
        queue.roundtrip(&mut state).map_err(|e| fail(&e))?;
        if state.has_keyboard {
            let probe = seat.get_keyboard(&qh, ());
            queue.roundtrip(&mut state).map_err(|e| fail(&e))?;
            if probe.version() >= 3 {
                probe.release();
            }
        }
        let keys = Keys::new(state.keymap.take())?;

        let plain = pointers.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = keyboards.create_virtual_keyboard(&seat, &qh, ());
        // 键盘映射是一段文本，合成器把它当 C 字符串解析：结尾带上 NUL，长度也算上它。
        // 不带的话只是碰巧能用（映射到内存后页尾是零），文本长度正好是整页时合成器会读过头。
        let mut keymap = keys.text.clone().into_bytes();
        keymap.push(0);
        let file = anonymous_file(&keymap).map_err(|e| fail(&e))?;
        keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1 as u32,
            file.as_fd(),
            keymap.len() as u32,
        );
        queue.roundtrip(&mut state).map_err(|e| fail(&e))?;

        Ok(Self {
            conn,
            queue,
            state,
            globals,
            seat,
            pointers,
            per_output,
            outputs: HashMap::new(),
            bound: HashMap::new(),
            plain,
            keyboard,
            keys,
            wheel_rest: (0, 0),
            started: Instant::now(),
            unread: 0,
        })
    }

    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn flush(&mut self) -> Result<(), PlatformError> {
        self.conn.flush().map_err(|e| os_err("注入输入失败", e))?;
        // 这条连接平时只发不收：隔一阵做一次往返，把合成器发来的事件读掉，也能及时发现连接断了。
        self.unread += 1;
        if self.unread >= 256 {
            self.unread = 0;
            self.queue
                .roundtrip(&mut self.state)
                .map_err(|e| os_err("注入输入失败", e))?;
        }
        Ok(())
    }

    /// 绑定到这块显示器的虚拟指针；合成器不支持绑定时返回 `None`。
    /// 绑定到这块输出（全局对象编号 `wl_global`）的虚拟指针。
    fn pointer_for(&mut self, wl_global: u32) -> Option<ZwlrVirtualPointerV1> {
        if !self.per_output {
            return None;
        }
        if let Some(p) = self.bound.get(&wl_global) {
            return Some(p.clone());
        }
        let qh = self.queue.handle();
        if !self.outputs.contains_key(&wl_global) {
            let (_, version) = globals_of(&self.globals, "wl_output")
                .into_iter()
                .find(|(name, _)| *name == wl_global)?;
            let output: wl_output::WlOutput =
                self.globals
                    .registry()
                    .bind(wl_global, version.min(4), &qh, ());
            self.outputs.insert(wl_global, output);
        }
        let output = self.outputs.get(&wl_global)?;
        let pointer = self.pointers.create_virtual_pointer_with_output(
            Some(&self.seat),
            Some(output),
            &qh,
            (),
        );
        self.bound.insert(wl_global, pointer.clone());
        Some(pointer)
    }
}

/// 逻辑点换成协议里的整数坐标。
fn subpixel(v: f64) -> u32 {
    (v.max(0.0) * SUBPIXEL).round() as u32
}

/// 所有显示器的外接矩形：(左, 上, 宽, 高)。
fn bounding_box(list: &[Monitor]) -> (f64, f64, f64, f64) {
    let left = list.iter().map(|m| m.logical.x).fold(f64::MAX, f64::min);
    let top = list.iter().map(|m| m.logical.y).fold(f64::MAX, f64::min);
    let right = list
        .iter()
        .map(|m| m.logical.x + m.logical.width)
        .fold(f64::MIN, f64::max);
    let bottom = list
        .iter()
        .map(|m| m.logical.y + m.logical.height)
        .fold(f64::MIN, f64::max);
    (left, top, right - left, bottom - top)
}

impl InputInjector for WlrInjector {
    fn set_caps_lock(&mut self, on: bool) -> Result<(), PlatformError> {
        if self.keys.caps_locked() != on {
            // 状态不一样：按一下 Caps Lock。虚拟键盘的锁定状态由我们自己算、自己上报。
            self.key(KeyCode::CAPS_LOCK, true, false)?;
            self.key(KeyCode::CAPS_LOCK, false, false)?;
        }
        Ok(())
    }

    fn move_to(&mut self, display_local_id: &str, x: f64, y: f64) -> Result<(), PlatformError> {
        // 每次鼠标移动都走这里：只取用得到的两样（位置尺寸、输出编号），不复制整个显示器记录。
        let find = |list: &[Monitor]| {
            list.iter()
                .find(|m| m.id == display_local_id)
                .map(|m| (m.logical, m.wl_global))
        };
        let mut found = linux::with_monitors(find);
        if found.is_none()
            && let Ok(list) = linux::monitors()
        {
            // 显示器配置变了（插拔、改分辨率）：刷新后再找一次。
            found = find(&list);
            linux::update_monitors(list);
        }
        let Some((logical, wl_global)) = found else {
            return Ok(());
        };
        let (w, h) = (logical.width, logical.height);
        // 最后一个逻辑点的右边缘已经在屏幕外，收一点。
        let (x, y) = (
            x.clamp(0.0, w - 1.0 / SUBPIXEL),
            y.clamp(0.0, h - 1.0 / SUBPIXEL),
        );
        let time = self.time();
        mark_injected_move();
        match self.pointer_for(wl_global) {
            Some(pointer) => {
                pointer.motion_absolute(time, subpixel(x), subpixel(y), subpixel(w), subpixel(h));
                pointer.frame();
            }
            None => {
                // 不能绑定输出：坐标按全部显示器的外接矩形算。
                let (left, top, lw, lh) = linux::with_monitors(bounding_box);
                self.plain.motion_absolute(
                    time,
                    subpixel(logical.x - left + x),
                    subpixel(logical.y - top + y),
                    subpixel(lw),
                    subpixel(lh),
                );
                self.plain.frame();
            }
        }
        self.flush()
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let state = if down {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        self.plain.button(self.time(), button_code(button), state);
        self.plain.frame();
        self.flush()
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        // 协议里向下、向右为正；流界约定向上、向右为正。不足一格的先攒着。
        let h = take_notches(&mut self.wheel_rest.0, dx);
        let v = -take_notches(&mut self.wheel_rest.1, dy);
        let time = self.time();
        for (axis, steps) in [
            (wl_pointer::Axis::VerticalScroll, v),
            (wl_pointer::Axis::HorizontalScroll, h),
        ] {
            if steps != 0 {
                self.plain.axis_source(wl_pointer::AxisSource::Wheel);
                self.plain
                    .axis_discrete(time, axis, f64::from(steps) * AXIS_PER_NOTCH, steps);
            }
        }
        if h != 0 || v != 0 {
            self.plain.frame();
        }
        self.flush()
    }

    fn key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Result<(), PlatformError> {
        // 长按连发由收到按键的程序自己产生。
        if repeat {
            return Ok(());
        }
        let Some(evdev) = keymap::to_linux(code) else {
            return Ok(());
        };
        let evdev = u32::from(evdev);
        self.keyboard.key(self.time(), evdev, u32::from(down));
        let (depressed, latched, locked, group) = self.keys.update(evdev, down);
        self.keyboard.modifiers(depressed, latched, locked, group);
        self.flush()
    }
}

// ---------------------------------------------------------------------------
// 捕获
// ---------------------------------------------------------------------------

/// 可以切到别的电脑的屏幕边：(显示器, 边)。`None` 表示还没收到，先在所有边上都放。
pub(crate) static SWITCH_EDGES: Mutex<Option<Vec<(String, Edge)>>> = Mutex::new(None);

/// 正在运行的捕获。
static ACTIVE: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

fn active() -> Option<Arc<Shared>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 边缘窗口的宽度（逻辑点）。
const STRIP: u32 = 1;
/// 光标碰到边缘窗口时，合成的那次"向外推"的位移（逻辑点）。
const PUSH: f64 = 3.0;
/// 回到本机后光标离屏幕边留出的距离（逻辑点），免得落在边缘窗口上又被当成往外推。
const RESTORE_INSET: f64 = 4.0;
/// 回到本机后这么久（毫秒）以内碰到边不算。
const RESTORE_QUIET_MS: u64 = 250;
/// 合成器没说时的长按连发参数：每秒次数、首次延迟（毫秒）。
const DEFAULT_REPEAT: (i32, i32) = (25, 600);

/// 引擎告知哪些屏幕边通往别的电脑。返回设置是否有变化。
pub fn set_switch_edges(edges: &[(String, Edge)]) -> bool {
    let mut current = SWITCH_EDGES.lock().unwrap_or_else(|e| e.into_inner());
    if current.as_deref() == Some(edges) {
        return false;
    }
    *current = Some(edges.to_vec());
    drop(current);
    if let Some(shared) = active() {
        shared.wake();
    }
    true
}

/// 控制远端期间的那些对象，放开时逐个销毁。
struct Grab {
    locked: ZwpLockedPointerV1,
    relative: ZwpRelativePointerV1,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    edge: Edge,
}

/// 光标所在的那条边缘窗口。
#[derive(Clone)]
struct Focus {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    edge: Edge,
    serial: u32,
}

#[derive(Default)]
struct Inner {
    /// 引擎要求锁定（正在控制远端）。
    want: bool,
    grab: Option<Grab>,
    focus: Option<Focus>,
    pointer: Option<wl_pointer::WlPointer>,
    outputs: HashMap<u32, wl_output::WlOutput>,
    /// 这个时刻（毫秒）之前碰到边不算。
    quiet_until_ms: u64,
    /// 合成器确认指针已锁定。流界不依赖它（见 `engage`），只有测试把它打印出来供参考。
    #[cfg(test)]
    pointer_locked: bool,
    /// 要求锁定时光标不在任何边缘窗口上：边缘窗口应当临时铺满屏幕去接住它。
    /// 锁定成功或放开后清掉。
    summon: bool,
}

struct Shared {
    conn: Connection,
    qh: QueueHandle<Capture>,
    seat: wl_seat::WlSeat,
    constraints: ZwpPointerConstraintsV1,
    relative: ZwpRelativePointerManagerV1,
    inhibit: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    virtual_pointers: Option<ZwlrVirtualPointerManagerV1>,
    inner: Mutex<Inner>,
    handler: Mutex<CaptureHandler>,
    stop: AtomicBool,
    /// 流界自己注入的移动碰到边也算。只有测试用：无头合成器里没有真实的鼠标，只能用虚拟指针模拟。
    accept_injected: bool,
}

impl Shared {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn report(&self, event: CapturedEvent) {
        (self.handler.lock().unwrap_or_else(|e| e.into_inner()))(event);
    }

    /// 让事件线程醒来检查一遍（停止、边缘窗口要重建）。
    fn wake(&self) {
        self.conn.display().sync(&self.qh, ());
        let _ = self.conn.flush();
    }

    /// 光标在边缘窗口上时锁定指针、独占键盘。光标不在窗口上时什么都不做，等它下次碰到边。
    fn engage(&self, inner: &mut Inner) {
        if !inner.want || inner.grab.is_some() {
            return;
        }
        let (Some(focus), Some(pointer)) = (inner.focus.clone(), inner.pointer.clone()) else {
            return;
        };
        // 把这条边缘窗口铺满整块屏幕并独占键盘：之后光标无论怎么动都还在窗口里，本机的其他窗口
        // 收不到点击，相对位移也一直收得到。即使合成器不肯锁定指针（老版本的 sway 只给普通窗口锁），
        // 控制远端也照常工作，只是看不见的光标会在这块屏幕里跟着动。
        focus
            .layer
            .set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        focus.layer.set_size(0, 0);
        focus
            .layer
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        focus.surface.commit();
        // 顺序有讲究：有的合成器只在申请锁定时窗口已经有键盘焦点的情况下才激活锁定，所以锁定放在后面。
        let locked = self.constraints.lock_pointer(
            &focus.surface,
            &pointer,
            None,
            Lifetime::Persistent,
            &self.qh,
            (),
        );
        let relative = self.relative.get_relative_pointer(&pointer, &self.qh, ());
        let inhibitor = self
            .inhibit
            .as_ref()
            .map(|m| m.inhibit_shortcuts(&focus.surface, &self.seat, &self.qh, ()));
        // 隐藏光标。
        pointer.set_cursor(focus.serial, None, 0, 0);
        inner.grab = Some(Grab {
            locked,
            relative,
            inhibitor,
            surface: focus.surface,
            layer: focus.layer,
            edge: focus.edge,
        });
        inner.summon = false;
        let _ = self.conn.flush();
    }

    /// 让合成器重新算一遍光标在哪个窗口上：用一个临时的虚拟指针发一次位移为零的移动。
    /// 窗口在不动的光标底下变大时，有的合成器要等下一次鼠标移动才发"进入"事件。
    fn poke_pointer(&self) {
        let Some(manager) = self.virtual_pointers.as_ref() else {
            return;
        };
        let pointer = manager.create_virtual_pointer(Some(&self.seat), &self.qh, ());
        mark_injected_move();
        pointer.motion(0, 0.0, 0.0);
        pointer.frame();
        pointer.destroy();
    }

    /// 放开键鼠，可选把光标放到本机系统坐标（逻辑点）的某个位置。
    fn release(&self, restore_at: Option<(f64, f64)>) {
        let mut inner = self.inner();
        inner.want = false;
        inner.summon = false;
        #[cfg(test)]
        {
            inner.pointer_locked = false;
        }
        if let Some(grab) = inner.grab.take() {
            grab.locked.destroy();
            grab.relative.destroy();
            if let Some(i) = grab.inhibitor {
                i.destroy();
            }
            // 窗口缩回屏幕边上的那一条。
            let (anchor, w, h) = strip_placement(grab.edge);
            grab.layer.set_anchor(anchor);
            grab.layer.set_size(w, h);
            grab.layer
                .set_keyboard_interactivity(KeyboardInteractivity::None);
            grab.surface.commit();
        }
        inner.quiet_until_ms = now_ms() + RESTORE_QUIET_MS;
        if let Some((x, y)) = restore_at {
            self.warp(&inner, x, y);
        }
        drop(inner);
        // 让事件线程替还按着的键补上抬起。不能在这里直接报：放开常常就是在上报回调里触发的。
        self.wake();
    }

    /// 用一个临时的虚拟指针把光标放到指定位置（离屏幕边留一点距离）。
    fn warp(&self, inner: &Inner, x: f64, y: f64) {
        let Some(manager) = self.virtual_pointers.as_ref().filter(|m| m.version() >= 2) else {
            return;
        };
        let target = linux::with_monitors(|list| {
            list.iter()
                .min_by(|a, b| distance(a, x, y).total_cmp(&distance(b, x, y)))
                .cloned()
        });
        let Some(monitor) = target else {
            return;
        };
        let Some(output) = inner.outputs.get(&monitor.wl_global) else {
            return;
        };
        let (w, h) = (monitor.logical.width, monitor.logical.height);
        let (lx, ly) = inset(x - monitor.logical.x, y - monitor.logical.y, w, h);
        let pointer = manager.create_virtual_pointer_with_output(
            Some(&self.seat),
            Some(output),
            &self.qh,
            (),
        );
        mark_injected_move();
        pointer.motion_absolute(0, subpixel(lx), subpixel(ly), subpixel(w), subpixel(h));
        pointer.frame();
        pointer.destroy();
    }
}

fn distance(m: &Monitor, x: f64, y: f64) -> f64 {
    let r = m.logical;
    let cx = x.clamp(r.x, r.x + r.width);
    let cy = y.clamp(r.y, r.y + r.height);
    (cx - x).abs() + (cy - y).abs()
}

/// 显示器内的一个点往里收，离四条边至少 [`RESTORE_INSET`]。
fn inset(x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
    let pull = |v: f64, size: f64| {
        let margin = RESTORE_INSET.min(size / 2.0);
        v.clamp(margin, size - margin)
    };
    (pull(x, w), pull(y, h))
}

/// 控制远端期间锁定指针、独占键盘；结束时放开，并把光标放到 `restore_at`（本机系统坐标，逻辑点）。
pub fn set_cursor_captured(captured: bool, restore_at: Option<(f64, f64)>) {
    let Some(shared) = active() else {
        return;
    };
    if captured {
        let mut inner = shared.inner();
        inner.want = true;
        shared.engage(&mut inner);
        if inner.grab.is_none() {
            // 光标不在边缘窗口上（不是推过屏幕边切的：快捷键命令、托盘菜单）：请事件线程把
            // 边缘窗口铺满屏幕去"接住"光标，见 `Capture::apply_summon`。
            inner.summon = true;
            drop(inner);
            shared.wake();
        }
    } else {
        shared.release(restore_at);
    }
}

/// 一条边缘窗口。
struct Strip {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    buffer: Option<wl_buffer::WlBuffer>,
    /// 有它时缓冲区只要 1 个像素，由合成器拉伸到窗口大小。
    viewport: Option<WpViewport>,
    /// 窗口当前铺满了整块屏幕（控制远端期间，或刚放开还没缩回去）。这时光标在它上面不代表碰到了边。
    expanded: bool,
    /// 为了接住不在屏幕边上的光标而临时铺满了屏幕（见 `Capture::apply_summon`）。
    summoned: bool,
    monitor: Monitor,
    edge: Edge,
}

/// 边缘窗口平时的位置：锚点和请求的尺寸（0 表示沿这条边铺满）。
fn strip_placement(edge: Edge) -> (Anchor, u32, u32) {
    match edge {
        Edge::Left => (Anchor::Left | Anchor::Top | Anchor::Bottom, STRIP, 0),
        Edge::Right => (Anchor::Right | Anchor::Top | Anchor::Bottom, STRIP, 0),
        Edge::Top => (Anchor::Top | Anchor::Left | Anchor::Right, 0, STRIP),
        Edge::Bottom => (Anchor::Bottom | Anchor::Left | Anchor::Right, 0, STRIP),
    }
}

impl Strip {
    /// 窗口内的一点对应的本机系统坐标（逻辑点），落在这条屏幕边上。
    fn edge_point(&self, sx: f64, sy: f64) -> (f64, f64) {
        let r = self.monitor.logical;
        match self.edge {
            Edge::Left => (r.x, r.y + sy),
            Edge::Right => (r.x + r.width, r.y + sy),
            Edge::Top => (r.x + sx, r.y),
            Edge::Bottom => (r.x + sx, r.y + r.height),
        }
    }

    fn destroy(self) {
        if let Some(v) = self.viewport {
            v.destroy();
        }
        self.layer.destroy();
        self.surface.destroy();
        if let Some(b) = self.buffer {
            b.destroy();
        }
    }
}

/// 向外推的方向。
fn outward(edge: Edge) -> (f64, f64) {
    match edge {
        Edge::Left => (-PUSH, 0.0),
        Edge::Right => (PUSH, 0.0),
        Edge::Top => (0.0, -PUSH),
        Edge::Bottom => (0.0, PUSH),
    }
}

/// 光标在屏幕边上的 `(x, y)` 处往外推：合成一次移动事件（约定见 [`CapturedEvent::MouseMove`]）。
pub(crate) fn push_event(edge: Edge, at: (f64, f64)) -> CapturedEvent {
    let (dx, dy) = outward(edge);
    CapturedEvent::MouseMove {
        x: at.0 - dx,
        y: at.1 - dy,
        dx,
        dy,
    }
}

/// 要放边缘窗口的 (显示器, 边)。`wanted` 为 `None` 时所有显示器的四条边都放。
fn strip_plan(monitors: &[Monitor], wanted: Option<&[(String, Edge)]>) -> Vec<(Monitor, Edge)> {
    let mut plan = Vec::new();
    for m in monitors {
        for edge in Edge::ALL {
            let on = wanted.is_none_or(|w| w.iter().any(|(id, e)| *id == m.id && *e == edge));
            if on {
                plan.push((m.clone(), edge));
            }
        }
    }
    plan
}

/// 一帧里攒起来的滚动量。
#[derive(Default)]
struct Scroll {
    /// 连续滚动量：横向、纵向（协议单位，15 为一格）。
    value: (f64, f64),
    /// 滚轮的格数：横向、纵向。
    steps: (i32, i32),
}

impl Scroll {
    /// 换成流界的滚动事件（1/120 格，向上、向右为正）。有格数用格数，没有（触摸板）用连续量。
    fn take(&mut self) -> Option<CapturedEvent> {
        let this = std::mem::take(self);
        let pick = |steps: i32, value: f64| {
            if steps != 0 {
                steps * WHEEL_NOTCH
            } else {
                (value / AXIS_PER_NOTCH * f64::from(WHEEL_NOTCH)).round() as i32
            }
        };
        let dx = pick(this.steps.0, this.value.0);
        let dy = -pick(this.steps.1, this.value.1);
        (dx != 0 || dy != 0).then_some(CapturedEvent::Wheel { dx, dy })
    }
}

/// 发给长按连发线程的消息。
pub(crate) enum Repeat {
    Down(KeyCode),
    Up(KeyCode),
    /// 合成器给的连发参数：每秒次数（0 表示不连发）、首次延迟（毫秒）。
    Info(i32, i32),
    Stop,
}

fn is_modifier(code: KeyCode) -> bool {
    (0xE0..=0xE7).contains(&code.0) || code.0 == 0x39
}

/// 长按连发：Wayland 的按键事件没有连发，由客户端自己按合成器给的参数产生。
/// `wanted`：是否还在控制远端；`report`：上报连发的按键。
pub(crate) fn repeat_loop(
    rx: mpsc::Receiver<Repeat>,
    wanted: impl Fn() -> bool,
    report: impl Fn(CapturedEvent),
) {
    let (mut rate, mut delay) = DEFAULT_REPEAT;
    // 正在连发的键和下一次的时间。
    let mut held: Option<(KeyCode, Instant)> = None;
    loop {
        let msg = match held {
            Some((_, at)) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            },
        };
        match msg {
            Some(Repeat::Stop) => return,
            Some(Repeat::Info(r, d)) => (rate, delay) = (r, d),
            Some(Repeat::Down(code)) => {
                held = (rate > 0 && !is_modifier(code)).then(|| {
                    (
                        code,
                        Instant::now() + Duration::from_millis(delay.max(1) as u64),
                    )
                });
            }
            Some(Repeat::Up(code)) => {
                if held.is_some_and(|(c, _)| c == code) {
                    held = None;
                }
            }
            None => {
                let Some((code, _)) = held else {
                    continue;
                };
                // 已经回到本机就不再连发。
                if !wanted() || rate <= 0 {
                    held = None;
                    continue;
                }
                report(CapturedEvent::Key {
                    code,
                    down: true,
                    repeat: true,
                });
                held = Some((
                    code,
                    Instant::now() + Duration::from_micros(1_000_000 / rate as u64),
                ));
            }
        }
    }
}

/// 事件线程上的状态。
struct Capture {
    shared: Arc<Shared>,
    globals: GlobalList,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    layer_shell: ZwlrLayerShellV1,
    viewporter: Option<WpViewporter>,
    strips: Vec<Strip>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// 光标在哪条边缘窗口上，以及窗口内的位置。
    on: Option<(wl_surface::WlSurface, f64, f64)>,
    scroll: Scroll,
    repeat: mpsc::Sender<Repeat>,
    /// 已经上报了按下、还没上报抬起的键。
    pressed: Vec<KeyCode>,
    /// 这次控制远端期间已经上报过的大写锁定状态。
    caps_reported: Option<bool>,
    /// 当前这批边缘窗口是按哪份设置建的。
    built_for: Option<Vec<(String, Edge)>>,
    rebuild: bool,
}

impl Capture {
    fn captured(&self) -> bool {
        self.shared.inner().grab.is_some()
    }

    /// 已经回到本机时，替还按着的键补上抬起。键盘一还给本机，真正的抬起就收不到了；
    /// 不补的话控制器会一直以为这些键按着（比如切换用的方向键会被一直"吞掉"，下次要按两下才生效）。
    fn release_held_keys(&mut self) {
        if self.pressed.is_empty() || self.shared.inner().want {
            return;
        }
        // 下次再控制远端时重新报一次大写锁定。
        self.caps_reported = None;
        for code in std::mem::take(&mut self.pressed) {
            let _ = self.repeat.send(Repeat::Up(code));
            self.shared.report(CapturedEvent::Key {
                code,
                down: false,
                repeat: false,
            });
        }
    }

    /// 按当前的显示器和可切换的边重建边缘窗口。
    fn rebuild_strips(&mut self) {
        self.rebuild = false;
        let wanted = SWITCH_EDGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // 正在控制远端时不动窗口（锁定和键盘焦点都在其中一条上），回到本机后再重建。
        if self.captured() {
            self.rebuild = true;
            return;
        }
        let Ok(monitors) = linux_wl::monitors() else {
            return;
        };
        linux::update_monitors(monitors.clone());
        for s in self.strips.drain(..) {
            s.destroy();
        }
        self.on = None;
        let qh = &self.shared.qh;
        let mut outputs = HashMap::new();
        for (global, version) in globals_of(&self.globals, "wl_output") {
            let output: wl_output::WlOutput =
                self.globals.registry().bind(global, version.min(4), qh, ());
            outputs.insert(global, output);
        }
        for (monitor, edge) in strip_plan(&monitors, wanted.as_deref()) {
            let Some(output) = outputs.get(&monitor.wl_global) else {
                continue;
            };
            let surface = self.compositor.create_surface(qh, ());
            let layer = self.layer_shell.get_layer_surface(
                &surface,
                Some(output),
                Layer::Overlay,
                "flowacross-edge".to_owned(),
                qh,
                (),
            );
            let (anchor, w, h) = strip_placement(edge);
            layer.set_anchor(anchor);
            layer.set_size(w, h);
            // -1：不理会面板等留出的区域，贴到真正的屏幕边上。
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            surface.commit();
            let viewport = self
                .viewporter
                .as_ref()
                .map(|v| v.get_viewport(&surface, qh, ()));
            self.strips.push(Strip {
                surface,
                layer,
                buffer: None,
                viewport,
                expanded: false,
                summoned: false,
                monitor,
                edge,
            });
        }
        {
            let mut inner = self.shared.inner();
            for (_, old) in std::mem::replace(&mut inner.outputs, outputs) {
                if old.version() >= 3 {
                    old.release();
                }
            }
            inner.focus = None;
        }
        self.built_for = wanted;
    }

    /// 按 `Inner::summon` 铺开或收回边缘窗口。
    ///
    /// 不是推过屏幕边切过去的时候（快捷键命令、托盘菜单），光标不在任何边缘窗口上，流界拿不到
    /// 指针，也就锁不住它。这时每块屏幕挑一条边缘窗口铺满整屏：光标所在的那块屏幕上，合成器会
    /// 报"进入"，`touch` 里随即完成锁定，其余屏幕上的再缩回去。没有边缘窗口的屏幕（它的哪条边
    /// 都不通往别的电脑）接不住，光标在那里时要等它碰到一条边。
    fn apply_summon(&mut self) {
        let (summon, grabbed) = {
            let inner = self.shared.inner();
            (
                inner.summon && inner.want && inner.grab.is_none(),
                inner.grab.as_ref().map(|g| g.surface.clone()),
            )
        };
        let mut covered: Vec<u32> = Vec::new();
        let mut changed = false;
        for strip in &mut self.strips {
            let first_on_monitor = !covered.contains(&strip.monitor.wl_global);
            if summon && first_on_monitor && !strip.summoned {
                covered.push(strip.monitor.wl_global);
                strip
                    .layer
                    .set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
                strip.layer.set_size(0, 0);
                strip.surface.commit();
                strip.summoned = true;
                changed = true;
            } else if summon && strip.summoned {
                covered.push(strip.monitor.wl_global);
            } else if !summon && strip.summoned {
                strip.summoned = false;
                // 正锁着的那条由 `release` 负责缩回去。
                if grabbed.as_ref() != Some(&strip.surface) {
                    let (anchor, w, h) = strip_placement(strip.edge);
                    strip.layer.set_anchor(anchor);
                    strip.layer.set_size(w, h);
                    strip.surface.commit();
                    changed = true;
                }
            }
        }
        // 铺开后光标进窗口的事件，要等合成器确认了新尺寸（`Configure`）才会有，见那里的处理。
        if changed {
            let _ = self.shared.conn.flush();
        }
    }

    fn strip_of(&self, surface: &wl_surface::WlSurface) -> Option<&Strip> {
        self.strips.iter().find(|s| s.surface == *surface)
    }

    /// 光标在边缘窗口上（刚进来或在上面移动）：没在控制远端时，当作往外推了一下。
    fn touch(&mut self) {
        let Some((surface, sx, sy)) = self.on.clone() else {
            return;
        };
        let Some(strip) = self.strip_of(&surface) else {
            return;
        };
        let (expanded, edge, point) = (strip.expanded, strip.edge, strip.edge_point(sx, sy));
        let event = push_event(edge, point);
        {
            let mut inner = self.shared.inner();
            if inner.grab.is_some() {
                return;
            }
            if inner.want {
                // 之前要求锁定时光标不在边上，现在碰到了边、或进了为它铺满屏幕的窗口：补上。
                self.shared.engage(&mut inner);
                drop(inner);
                // 接住了：其他屏幕上铺开的窗口缩回去。
                self.apply_summon();
                return;
            }
            // 铺满屏幕的窗口（刚放开还没缩回去）上的移动不代表碰到了边。
            if expanded {
                return;
            }
            // 流界自己把光标推到边上（被远端控制，或刚回到本机）不算。
            let injected = recently_injected_move() && !self.shared.accept_injected;
            if injected || now_ms() < inner.quiet_until_ms {
                return;
            }
        }
        // 控制器在回调里决定切换时会调用 `set_cursor_captured`，这里不能拿着锁。
        self.shared.report(event);
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Capture {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 显示器插拔：边缘窗口要重建。
        match event {
            wl_registry::Event::Global { interface, .. } if interface == "wl_output" => {
                state.rebuild = true;
            }
            wl_registry::Event::GlobalRemove { .. } => state.rebuild = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Capture {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 别的线程叫醒：刚放开键盘的话补上按键的抬起，要不要铺开窗口去接住光标，
        // 再看看可切换的边是不是变了。
        state.release_held_keys();
        state.apply_summon();
        let wanted = SWITCH_EDGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if wanted != state.built_for {
            state.rebuild = true;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Capture {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        let mut inner = state.shared.inner();
        if caps.contains(wl_seat::Capability::Pointer) && inner.pointer.is_none() {
            inner.pointer = Some(seat.get_pointer(qh, ()));
        }
        if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Capture {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                let Some(strip) = state.strip_of(&surface) else {
                    return;
                };
                let focus = Focus {
                    surface: strip.surface.clone(),
                    layer: strip.layer.clone(),
                    edge: strip.edge,
                    serial,
                };
                state.shared.inner().focus = Some(focus);
                state.on = Some((surface, surface_x, surface_y));
                state.touch();
            }
            wl_pointer::Event::Leave { .. } => {
                state.on = None;
                state.shared.inner().focus = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(on) = state.on.as_mut() {
                    (on.1, on.2) = (surface_x, surface_y);
                }
                state.touch();
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(pressed),
                ..
            } => {
                if state.captured()
                    && let Some(button) = button_from_code(button)
                {
                    state.shared.report(CapturedEvent::Button {
                        button,
                        down: pressed == wl_pointer::ButtonState::Pressed,
                    });
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => match axis {
                wl_pointer::Axis::HorizontalScroll => state.scroll.value.0 += value,
                wl_pointer::Axis::VerticalScroll => state.scroll.value.1 += value,
                _ => {}
            },
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } => match axis {
                wl_pointer::Axis::HorizontalScroll => state.scroll.steps.0 += discrete,
                wl_pointer::Axis::VerticalScroll => state.scroll.steps.1 += discrete,
                _ => {}
            },
            wl_pointer::Event::Frame => {
                if let Some(event) = state.scroll.take()
                    && state.captured()
                {
                    state.shared.report(event);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for Capture {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event else {
            return;
        };
        if (dx != 0.0 || dy != 0.0) && state.captured() {
            // 锁定期间位置没有意义，只有位移（逻辑点，带合成器的指针加速）。
            state.shared.report(CapturedEvent::MouseMove {
                x: 0.0,
                y: 0.0,
                dx,
                dy,
            });
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Capture {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                let down = key_state == wl_keyboard::KeyState::Pressed;
                let Some(code) = u16::try_from(key).ok().and_then(keymap::from_linux) else {
                    return;
                };
                // 键盘焦点只有控制远端时才在边缘窗口上；放开的过程中收到的不算。
                if !state.shared.inner().want {
                    return;
                }
                let _ = state.repeat.send(if down {
                    Repeat::Down(code)
                } else {
                    Repeat::Up(code)
                });
                state.pressed.retain(|c| *c != code);
                if down {
                    state.pressed.push(code);
                }
                state.shared.report(CapturedEvent::Key {
                    code,
                    down,
                    repeat: false,
                });
                // 这个按键可能正好让控制器切回了本机（快捷键）：马上把还按着的键抬起来。
                state.release_held_keys();
            }
            wl_keyboard::Event::Modifiers { mods_locked, .. } => {
                // 拿到键盘焦点（开始控制远端）时合成器会报一次修饰键状态，之后有变化再报。
                // 把大写锁定的状态转告控制器，让对方和这边一致。
                if !state.shared.inner().want {
                    state.caps_reported = None;
                    return;
                }
                let on = mods_locked & LOCK_MASK != 0;
                if state.caps_reported != Some(on) {
                    state.caps_reported = Some(on);
                    state.shared.report(CapturedEvent::CapsLock(on));
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                let _ = state.repeat.send(Repeat::Info(rate, delay));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Capture {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                let shm = state.shm.clone();
                let Some(strip) = state.strips.iter_mut().find(|s| s.layer == *layer) else {
                    return;
                };
                strip.expanded = width > STRIP && height > STRIP;
                // 全透明的缓冲区：窗口要有内容才会显示出来，才收得到指针事件。
                // 有 viewporter 时只用 1 个像素，让合成器拉伸；没有就老老实实分配整块。
                let (w, h) = (width.max(1) as i32, height.max(1) as i32);
                let (bw, bh) = if strip.viewport.is_some() {
                    (1, 1)
                } else {
                    (w, h)
                };
                if let Some(viewport) = &strip.viewport {
                    viewport.set_destination(w, h);
                }
                if strip.buffer.is_none() || strip.viewport.is_none() {
                    let size = (bw * bh * 4) as usize;
                    let Ok(file) = anonymous_file(&vec![0u8; size]) else {
                        return;
                    };
                    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
                    let buffer =
                        pool.create_buffer(0, bw, bh, bw * 4, wl_shm::Format::Argb8888, qh, ());
                    pool.destroy();
                    if let Some(old) = strip.buffer.replace(buffer) {
                        old.destroy();
                    }
                }
                strip.surface.attach(strip.buffer.as_ref(), 0, 0);
                strip.surface.commit();
                // 为了接住光标而铺开的窗口现在才真的变大：光标要是没动，合成器不会主动报"进入"，
                // 推它一下。
                if strip.summoned && strip.expanded {
                    state.shared.poke_pointer();
                    let _ = state.shared.conn.flush();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // 所在的显示器没了；由显示器变化触发的重建来清理。
                state.rebuild = true;
            }
            _ => {}
        }
    }
}

delegate_noop!(Capture: wl_compositor::WlCompositor);
delegate_noop!(Capture: ignore wl_surface::WlSurface);
delegate_noop!(Capture: ignore wl_shm::WlShm);
delegate_noop!(Capture: wl_shm_pool::WlShmPool);
delegate_noop!(Capture: ignore wl_buffer::WlBuffer);
delegate_noop!(Capture: ignore wl_output::WlOutput);
delegate_noop!(Capture: ZwlrLayerShellV1);
delegate_noop!(Capture: WpViewporter);
delegate_noop!(Capture: WpViewport);
delegate_noop!(Capture: ZwpPointerConstraintsV1);
impl Dispatch<ZwpLockedPointerV1, ()> for Capture {
    fn event(
        state: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 合成器有没有真的锁定不影响工作（见 `engage`），只有测试记下来打印。
        #[cfg(test)]
        {
            state.shared.inner().pointer_locked =
                matches!(event, zwp_locked_pointer_v1::Event::Locked);
        }
        #[cfg(not(test))]
        let _ = (state, event);
    }
}
delegate_noop!(Capture: ZwpRelativePointerManagerV1);
delegate_noop!(Capture: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(Capture: ignore ZwpKeyboardShortcutsInhibitorV1);
delegate_noop!(Capture: ZwlrVirtualPointerManagerV1);
delegate_noop!(Capture: ZwlrVirtualPointerV1);

#[derive(Default)]
pub struct WlrCapture {
    shared: Option<Arc<Shared>>,
    thread: Option<JoinHandle<()>>,
    repeat: Option<(mpsc::Sender<Repeat>, JoinHandle<()>)>,
    accept_injected: bool,
}

impl WlrCapture {
    /// 流界自己注入的移动碰到边也上报。只给测试用。
    #[cfg(test)]
    fn accepting_injected() -> Self {
        let mut capture = Self::default();
        capture.accept_injected = true;
        capture
    }
}

impl InputCapture for WlrCapture {
    fn start(&mut self, handler: CaptureHandler) -> Result<(), PlatformError> {
        self.stop();
        let fail = |e: &dyn std::fmt::Display| os_err("监听键鼠失败", e);
        let conn = linux_wl::connect()?;
        let (globals, mut queue): (GlobalList, EventQueue<Capture>) =
            registry_queue_init(&conn).map_err(|e| fail(&e))?;
        let qh = queue.handle();
        let need =
            |what: &str| PlatformError::Os(format!("合成器不支持{what}，无法把键鼠切到其他电脑"));
        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=4, ()).map_err(|e| fail(&e))?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|e| fail(&e))?;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).map_err(|e| fail(&e))?;
        let layer_shell: ZwlrLayerShellV1 = globals
            .bind(&qh, 1..=4, ())
            .map_err(|_| need(" layer-shell 协议"))?;
        let constraints: ZwpPointerConstraintsV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| need("指针锁定协议（pointer-constraints）"))?;
        let relative: ZwpRelativePointerManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| need("相对指针协议（relative-pointer）"))?;
        let inhibit: Option<ZwpKeyboardShortcutsInhibitManagerV1> =
            globals.bind(&qh, 1..=1, ()).ok();
        let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
        let virtual_pointers: Option<ZwlrVirtualPointerManagerV1> =
            globals.bind(&qh, 1..=2, ()).ok();

        let shared = Arc::new(Shared {
            conn: conn.clone(),
            qh: qh.clone(),
            seat,
            constraints,
            relative,
            inhibit,
            virtual_pointers,
            inner: Mutex::new(Inner::default()),
            handler: Mutex::new(handler),
            stop: AtomicBool::new(false),
            accept_injected: self.accept_injected,
        });
        let (repeat_tx, repeat_rx) = mpsc::channel();
        let mut state = Capture {
            shared: shared.clone(),
            globals,
            compositor,
            shm,
            layer_shell,
            viewporter,
            strips: Vec::new(),
            keyboard: None,
            on: None,
            scroll: Scroll::default(),
            repeat: repeat_tx.clone(),
            pressed: Vec::new(),
            caps_reported: None,
            built_for: None,
            rebuild: false,
        };
        // 先拿到指针和键盘，再建边缘窗口并等它们显示出来，有问题在这里就报出来。
        queue.roundtrip(&mut state).map_err(|e| fail(&e))?;
        state.rebuild_strips();
        queue.roundtrip(&mut state).map_err(|e| fail(&e))?;
        queue.roundtrip(&mut state).map_err(|e| fail(&e))?;

        let for_repeat = shared.clone();
        let repeat_thread = std::thread::Builder::new()
            .name("flowacross-wl-repeat".into())
            .spawn(move || {
                repeat_loop(
                    repeat_rx,
                    || for_repeat.inner().want,
                    |event| for_repeat.report(event),
                )
            })
            .map_err(|e| os_err("无法创建输入线程", e))?;
        let thread = std::thread::Builder::new()
            .name("flowacross-wl-input".into())
            .spawn(move || run(queue, state))
            .map_err(|e| os_err("无法创建输入线程", e))?;
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(shared.clone());
        self.shared = Some(shared);
        self.thread = Some(thread);
        self.repeat = Some((repeat_tx, repeat_thread));
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
        shared.wake();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some((tx, t)) = self.repeat.take() {
            let _ = tx.send(Repeat::Stop);
            let _ = t.join();
        }
    }
}

impl Drop for WlrCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(mut queue: EventQueue<Capture>, mut state: Capture) {
    let shared = state.shared.clone();
    // 连接断开（合成器退出，通常意味着整个桌面会话结束）时结束。
    while queue.blocking_dispatch(&mut state).is_ok() {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        if state.rebuild {
            state.rebuild_strips();
            let _ = shared.conn.flush();
        }
    }
    shared.release(None);
    for s in state.strips.drain(..) {
        s.destroy();
    }
    let _ = shared.conn.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_core::Rect;

    fn monitor(id: &str, rect: Rect) -> Monitor {
        Monitor {
            id: id.into(),
            index: 0,
            phys: (0, 0, rect.width as i32, rect.height as i32),
            scale: 1.0,
            logical: rect,
            primary: false,
            wl_global: 1,
        }
    }

    #[test]
    fn buttons_use_evdev_codes_both_ways() {
        for b in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
        ] {
            assert_eq!(button_from_code(button_code(b)), Some(b));
        }
        assert_eq!(button_code(MouseButton::Left), 0x110);
        assert_eq!(button_from_code(0x120), None);
    }

    #[test]
    fn touching_an_edge_reports_an_outward_push_from_inside() {
        let strip_at = |edge| {
            let m = monitor("DP-1", Rect::new(100.0, 50.0, 1920.0, 1080.0));
            // 只用到显示器和边，窗口对象在这个计算里不参与，所以单独算。
            let r = m.logical;
            match edge {
                Edge::Left => (r.x, r.y + 300.0),
                Edge::Right => (r.x + r.width, r.y + 300.0),
                Edge::Top => (r.x + 300.0, r.y),
                Edge::Bottom => (r.x + 300.0, r.y + r.height),
            }
        };
        // 右边：移动前在屏幕内 3 个点处，移动后正好到右边缘。
        assert_eq!(
            push_event(Edge::Right, strip_at(Edge::Right)),
            CapturedEvent::MouseMove {
                x: 2017.0,
                y: 350.0,
                dx: 3.0,
                dy: 0.0
            }
        );
        assert_eq!(
            push_event(Edge::Left, strip_at(Edge::Left)),
            CapturedEvent::MouseMove {
                x: 103.0,
                y: 350.0,
                dx: -3.0,
                dy: 0.0
            }
        );
        assert_eq!(
            push_event(Edge::Top, strip_at(Edge::Top)),
            CapturedEvent::MouseMove {
                x: 400.0,
                y: 53.0,
                dx: 0.0,
                dy: -3.0
            }
        );
        assert_eq!(
            push_event(Edge::Bottom, strip_at(Edge::Bottom)),
            CapturedEvent::MouseMove {
                x: 400.0,
                y: 1127.0,
                dx: 0.0,
                dy: 3.0
            }
        );
    }

    #[test]
    fn strips_only_go_on_edges_that_lead_somewhere() {
        let list = [
            monitor("A", Rect::new(0.0, 0.0, 100.0, 100.0)),
            monitor("B", Rect::new(100.0, 0.0, 100.0, 100.0)),
        ];
        // 还没收到设置：全部都放。
        assert_eq!(strip_plan(&list, None).len(), 8);
        let wanted = vec![("B".to_owned(), Edge::Right), ("A".to_owned(), Edge::Top)];
        let plan: Vec<(String, Edge)> = strip_plan(&list, Some(&wanted))
            .into_iter()
            .map(|(m, e)| (m.id, e))
            .collect();
        assert_eq!(
            plan,
            vec![("A".to_owned(), Edge::Top), ("B".to_owned(), Edge::Right)]
        );
        assert!(strip_plan(&list, Some(&[])).is_empty());
    }

    #[test]
    fn scroll_prefers_wheel_steps_and_flips_vertical() {
        // 滚轮向下一格：协议里是 +1 格、+15，流界里是 -120。
        let mut s = Scroll {
            value: (0.0, 15.0),
            steps: (0, 1),
        };
        assert_eq!(s.take(), Some(CapturedEvent::Wheel { dx: 0, dy: -120 }));
        // 取走后清零。
        assert_eq!(s.take(), None);
        // 触摸板：只有连续量。
        let mut s = Scroll {
            value: (7.5, -3.75),
            steps: (0, 0),
        };
        assert_eq!(s.take(), Some(CapturedEvent::Wheel { dx: 60, dy: 30 }));
    }

    #[test]
    fn restored_cursor_stays_off_the_edges() {
        assert_eq!(inset(0.0, 500.0, 1920.0, 1080.0), (4.0, 500.0));
        assert_eq!(inset(1920.0, 1080.0, 1920.0, 1080.0), (1916.0, 1076.0));
        assert_eq!(inset(800.0, 0.0, 1920.0, 1080.0), (800.0, 4.0));
        // 屏幕极小时放在中间。
        assert_eq!(inset(0.0, 0.0, 4.0, 4.0), (2.0, 2.0));
    }

    #[test]
    fn modifiers_and_caps_lock_do_not_repeat() {
        assert!(is_modifier(KeyCode::LEFT_CTRL));
        assert!(is_modifier(KeyCode(0xE7)));
        assert!(is_modifier(KeyCode(0x39)));
        assert!(!is_modifier(KeyCode::A));
    }

    // 以下测试需要一个 wlroots 系的合成器（CI 和本机容器里用无头的 sway），和 X11 的测试一样要串行跑：
    // `cargo test -p flow-platform -- --ignored --test-threads=1 linux_wl`。

    use crate::Verdict;

    fn recv_until(
        rx: &mpsc::Receiver<CapturedEvent>,
        mut want: impl FnMut(&CapturedEvent) -> bool,
    ) -> Option<CapturedEvent> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ev) if want(&ev) => return Some(ev),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        None
    }

    fn is_move(e: &CapturedEvent) -> bool {
        matches!(e, CapturedEvent::MouseMove { .. })
    }

    /// 用虚拟指针注入一次相对移动（注入器对外只有绝对移动）。
    fn nudge(inj: &mut WlrInjector, dx: f64, dy: f64) {
        let time = inj.time();
        inj.plain.motion(time, dx, dy);
        inj.plain.frame();
        inj.flush().unwrap();
    }

    #[test]
    #[ignore = "需要 wlroots 系合成器"]
    fn edge_push_capture_and_release_round_trip() {
        assert_eq!(
            linux_wl::backend(),
            linux_wl::Backend::Wlr,
            "要在 sway 里跑"
        );
        let displays = crate::display_provider().displays().expect("枚举显示器");
        let d = displays[0].clone();
        let (w, h) = (d.rect.width, d.rect.height);
        // 先建注入器：无头合成器里要有了虚拟指针和键盘，席位才有指针和键盘。
        let mut inj = WlrInjector::new().expect("注入器");
        inj.move_to(&d.local_id, w / 2.0, h / 2.0).unwrap();

        // 模拟控制器：`armed` 时一碰到边就切到远端。
        let armed = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let mut cap = WlrCapture::accepting_injected();
        let arm = armed.clone();
        cap.start(Box::new(move |ev| {
            if arm.swap(false, Ordering::SeqCst) && is_move(&ev) {
                set_cursor_captured(true, None);
            }
            let _ = tx.send(ev);
            Verdict::Pass
        }))
        .expect("启动捕获");
        std::thread::sleep(Duration::from_millis(200));
        while rx.try_recv().is_ok() {}

        // 1. 屏幕中间移动不上报（看不到）；推到右边缘时上报一次向外推，位置对得上。
        inj.move_to(&d.local_id, w / 2.0 + 50.0, 300.0).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        armed.store(true, Ordering::SeqCst);
        inj.move_to(&d.local_id, w - 0.25, 300.0).unwrap();
        let Some(CapturedEvent::MouseMove { x, y, dx, dy }) = recv_until(&rx, is_move) else {
            panic!("推到右边缘没有上报");
        };
        assert_eq!((dx, dy), (PUSH, 0.0));
        assert_eq!(x + dx, d.rect.x + w, "移动后正好在右边缘");
        assert!((y - (d.rect.y + 300.0)).abs() <= 1.0, "沿边位置 {y}");

        // 2. 回调里已经要求锁定：之后的移动按位移上报，光标不再动。
        std::thread::sleep(Duration::from_millis(200));
        while rx.try_recv().is_ok() {}
        assert!(active().unwrap().inner().grab.is_some(), "应该已经锁定");
        // 合成器肯不肯真的锁定指针因版本而异，两种情况都要能工作；这里只打印出来供参考。
        eprintln!(
            "合成器是否激活了指针锁定：{}",
            active().unwrap().inner().pointer_locked
        );
        nudge(&mut inj, -12.0, 7.0);
        let Some(CapturedEvent::MouseMove { dx, dy, .. }) = recv_until(&rx, is_move) else {
            panic!("锁定期间的移动没有上报");
        };
        assert_eq!((dx, dy), (-12.0, 7.0));

        // 3. 键盘、按钮、滚轮都归流界。
        inj.key(KeyCode::A, true, false).unwrap();
        inj.key(KeyCode::A, false, false).unwrap();
        let key = |down| CapturedEvent::Key {
            code: KeyCode::A,
            down,
            repeat: false,
        };
        assert!(recv_until(&rx, |e| *e == key(true)).is_some(), "按下 A");
        assert!(recv_until(&rx, |e| *e == key(false)).is_some(), "抬起 A");
        inj.button(MouseButton::Left, true).unwrap();
        inj.button(MouseButton::Left, false).unwrap();
        let left = |down| CapturedEvent::Button {
            button: MouseButton::Left,
            down,
        };
        assert!(recv_until(&rx, |e| *e == left(true)).is_some(), "按下左键");
        assert!(recv_until(&rx, |e| *e == left(false)).is_some(), "抬起左键");
        inj.wheel(0, -240).unwrap();
        let down2 = CapturedEvent::Wheel { dx: 0, dy: -240 };
        let down1 = CapturedEvent::Wheel { dx: 0, dy: -120 };
        let first = recv_until(&rx, |e| matches!(e, CapturedEvent::Wheel { .. })).expect("滚轮");
        // 两格可能合在一帧里，也可能分两帧。
        if first == down1 {
            assert_eq!(
                recv_until(&rx, |e| matches!(e, CapturedEvent::Wheel { .. })),
                Some(down1)
            );
        } else {
            assert_eq!(first, down2);
        }

        // 大写锁定：对齐到指定状态（虚拟键盘按一下 Caps Lock），捕获这边把新状态报出来。
        let caps = |on| move |e: &CapturedEvent| *e == CapturedEvent::CapsLock(on);
        inj.set_caps_lock(true).unwrap();
        assert!(
            recv_until(&rx, caps(true)).is_some(),
            "大写锁定打开后应该上报"
        );
        // 已经是这个状态：不再按键，也不重复上报。
        inj.set_caps_lock(true).unwrap();
        assert!(
            recv_until(&rx, |e| matches!(e, CapturedEvent::CapsLock(_))).is_none(),
            "状态没变不应该重复上报"
        );
        inj.set_caps_lock(false).unwrap();
        assert!(
            recv_until(&rx, caps(false)).is_some(),
            "大写锁定关闭后应该上报"
        );
        while rx.try_recv().is_ok() {}

        // 4. 长按连发由流界按合成器给的参数产生。
        inj.key(KeyCode::A, true, false).unwrap();
        let repeat = CapturedEvent::Key {
            code: KeyCode::A,
            down: true,
            repeat: true,
        };
        assert!(recv_until(&rx, |e| *e == repeat).is_some(), "长按连发");
        inj.key(KeyCode::A, false, false).unwrap();
        assert!(recv_until(&rx, |e| *e == key(false)).is_some());

        // 5. 放开：键盘还给本机，光标放到指定位置（离边留一点），之后一小段时间内碰边不算。
        // 放开时还按着的键要补一个抬起：键盘一还给本机，真正的抬起就收不到了，
        // 不补的话控制器会一直以为这个键按着（用快捷键切回来后，下次要按两下才生效就是这么来的）。
        let held = KeyCode(0x05);
        inj.key(held, true, false).unwrap();
        let held_event = |down| CapturedEvent::Key {
            code: held,
            down,
            repeat: false,
        };
        assert!(recv_until(&rx, |e| *e == held_event(true)).is_some());
        set_cursor_captured(false, Some((d.rect.x + 500.0, d.rect.y + 400.0)));
        assert!(active().unwrap().inner().grab.is_none());
        assert!(
            recv_until(&rx, |e| *e == held_event(false)).is_some(),
            "放开时还按着的键应该补一个抬起"
        );
        inj.key(held, false, false).unwrap();
        std::thread::sleep(Duration::from_millis(RESTORE_QUIET_MS + 150));
        while rx.try_recv().is_ok() {}
        inj.key(KeyCode::A, true, false).unwrap();
        inj.key(KeyCode::A, false, false).unwrap();
        assert!(
            recv_until(&rx, |e| matches!(e, CapturedEvent::Key { .. })).is_none(),
            "放开后不应该再收到按键"
        );
        // 从 (500, 400) 往左推到头：碰到左边缘时沿边位置应该是 400，说明光标确实被放到了那里。
        nudge(&mut inj, -(w + 100.0), 0.0);
        let Some(CapturedEvent::MouseMove { x, y, dx, .. }) = recv_until(&rx, is_move) else {
            panic!("放开后推到左边缘没有上报");
        };
        assert_eq!(dx, -PUSH);
        assert_eq!(x + dx, d.rect.x);
        assert!((y - (d.rect.y + 400.0)).abs() <= 1.0, "沿边位置 {y}");
        assert!(active().unwrap().inner().grab.is_none(), "没有要求就不锁定");

        cap.stop();
        assert!(active().is_none());
    }

    /// 不是推过屏幕边切过去的（快捷键命令、托盘菜单）：光标在屏幕中间也要能接管键鼠，
    /// 放开后边缘窗口缩回去，屏幕中间不再拦输入。
    #[test]
    #[ignore = "需要 wlroots 系合成器"]
    fn capture_on_request_without_touching_an_edge() {
        assert_eq!(
            linux_wl::backend(),
            linux_wl::Backend::Wlr,
            "要在 sway 里跑"
        );
        let displays = crate::display_provider().displays().expect("枚举显示器");
        let d = displays[0].clone();
        let (w, h) = (d.rect.width, d.rect.height);
        let mut inj = WlrInjector::new().expect("注入器");
        inj.move_to(&d.local_id, w / 2.0, h / 2.0).unwrap();

        let (tx, rx) = mpsc::channel();
        let mut cap = WlrCapture::accepting_injected();
        cap.start(Box::new(move |ev| {
            let _ = tx.send(ev);
            Verdict::Pass
        }))
        .expect("启动捕获");
        std::thread::sleep(Duration::from_millis(200));
        while rx.try_recv().is_ok() {}

        // 光标在屏幕中间，没有碰任何边：直接要求接管。
        set_cursor_captured(true, None);
        let grabbed = || active().unwrap().inner().grab.is_some();
        for _ in 0..40 {
            if grabbed() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(grabbed(), "光标不在屏幕边上时也应该能接管键鼠");
        std::thread::sleep(Duration::from_millis(200));
        while rx.try_recv().is_ok() {}

        // 之后和推过屏幕边切过去一样：移动按位移上报，键盘归流界。
        nudge(&mut inj, 9.0, -4.0);
        let Some(CapturedEvent::MouseMove { dx, dy, .. }) = recv_until(&rx, is_move) else {
            panic!("接管后的移动没有上报");
        };
        assert_eq!((dx, dy), (9.0, -4.0));
        inj.key(KeyCode::A, true, false).unwrap();
        inj.key(KeyCode::A, false, false).unwrap();
        let key = |down| CapturedEvent::Key {
            code: KeyCode::A,
            down,
            repeat: false,
        };
        assert!(recv_until(&rx, |e| *e == key(true)).is_some(), "按下 A");
        assert!(recv_until(&rx, |e| *e == key(false)).is_some(), "抬起 A");

        // 放开：窗口缩回屏幕边上，屏幕中间的移动和按键不再归流界。
        set_cursor_captured(false, Some((d.rect.x + w / 2.0, d.rect.y + h / 2.0)));
        assert!(!grabbed());
        std::thread::sleep(Duration::from_millis(RESTORE_QUIET_MS + 150));
        while rx.try_recv().is_ok() {}
        inj.move_to(&d.local_id, w / 2.0 + 40.0, h / 2.0).unwrap();
        inj.key(KeyCode::A, true, false).unwrap();
        inj.key(KeyCode::A, false, false).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "放开后屏幕中间不应该再有上报"
        );
        // 推到屏幕边照常上报，说明边缘窗口回到了原来的样子。
        inj.move_to(&d.local_id, w - 0.25, 300.0).unwrap();
        let Some(CapturedEvent::MouseMove { dx, .. }) = recv_until(&rx, is_move) else {
            panic!("放开后推到右边缘没有上报");
        };
        assert_eq!(dx, PUSH);
        assert!(!grabbed(), "没有要求就不锁定");

        // 要求接管后马上又放开（还没等到光标进窗口）：不能留下铺满屏幕的窗口。
        inj.move_to(&d.local_id, w / 2.0, h / 2.0).unwrap();
        std::thread::sleep(Duration::from_millis(RESTORE_QUIET_MS + 150));
        set_cursor_captured(true, None);
        set_cursor_captured(false, None);
        std::thread::sleep(Duration::from_millis(RESTORE_QUIET_MS + 300));
        assert!(!grabbed());
        while rx.try_recv().is_ok() {}
        inj.move_to(&d.local_id, w / 2.0 - 40.0, h / 2.0).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "马上放开后屏幕中间不应该有上报"
        );

        cap.stop();
        assert!(active().is_none());
    }

    #[test]
    #[ignore = "需要 wlroots 系合成器"]
    fn only_requested_edges_react_and_injected_moves_are_ignored() {
        assert_eq!(
            linux_wl::backend(),
            linux_wl::Backend::Wlr,
            "要在 sway 里跑"
        );
        let displays = crate::display_provider().displays().expect("枚举显示器");
        let d = displays[0].clone();
        let (w, h) = (d.rect.width, d.rect.height);
        let mut inj = WlrInjector::new().expect("注入器");
        inj.move_to(&d.local_id, w / 2.0, h / 2.0).unwrap();
        set_switch_edges(&[(d.local_id.clone(), Edge::Left)]);

        let (tx, rx) = mpsc::channel();
        let mut cap = WlrCapture::accepting_injected();
        cap.start(Box::new(move |ev| {
            let _ = tx.send(ev);
            Verdict::Pass
        }))
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        while rx.try_recv().is_ok() {}

        // 右边不通往别的电脑：推过去没有反应。
        inj.move_to(&d.local_id, w - 0.25, 300.0).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err());
        // 左边有反应。
        inj.move_to(&d.local_id, 0.0, 300.0).unwrap();
        assert!(recv_until(&rx, is_move).is_some(), "左边缘应该上报");

        // 改成只有上边：窗口跟着重建。
        inj.move_to(&d.local_id, w / 2.0, h / 2.0).unwrap();
        set_switch_edges(&[(d.local_id.clone(), Edge::Top)]);
        std::thread::sleep(Duration::from_millis(300));
        while rx.try_recv().is_ok() {}
        inj.move_to(&d.local_id, 0.0, 300.0).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err());
        inj.move_to(&d.local_id, 700.0, 0.0).unwrap();
        let Some(CapturedEvent::MouseMove { x, dy, .. }) = recv_until(&rx, is_move) else {
            panic!("上边缘应该上报");
        };
        assert_eq!(dy, -PUSH);
        assert!((x - (d.rect.x + 700.0)).abs() <= 1.0);
        cap.stop();

        // 正式使用时（不接受注入的移动）：被远端推到边上不算往外推。
        let (tx, rx) = mpsc::channel();
        let mut cap = WlrCapture::default();
        cap.start(Box::new(move |ev| {
            let _ = tx.send(ev);
            Verdict::Pass
        }))
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        inj.move_to(&d.local_id, w / 2.0, h / 2.0).unwrap();
        inj.move_to(&d.local_id, 700.0, 0.0).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "流界自己注入的移动不应该被当成本机输入"
        );
        cap.stop();
        *SWITCH_EDGES.lock().unwrap() = None;
    }

    #[test]
    fn layout_bounding_box_covers_all_monitors() {
        let list = [
            monitor("A", Rect::new(-1920.0, 0.0, 1920.0, 1080.0)),
            monitor("B", Rect::new(0.0, -200.0, 2560.0, 1440.0)),
        ];
        assert_eq!(bounding_box(&list), (-1920.0, -200.0, 4480.0, 1440.0));
        assert_eq!(subpixel(1.5), 24);
        assert_eq!(subpixel(-3.0), 0);
    }
}
