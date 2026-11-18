//! Linux（Wayland，GNOME / KDE）：通过 xdg-desktop-portal 注入、捕获键鼠，以及 GNOME 上的剪贴板。
//!
//! 这两个桌面不提供 wlroots 的虚拟输入协议，对外的正式接口是 portal（D-Bus）加 libei（一条套接字上的
//! 输入事件协议，这里用纯 Rust 的 `reis`）：
//!
//! - **注入**：`RemoteDesktop` 会话。第一次要用户在系统弹窗里同意，之后凭保存的令牌恢复，不再弹窗。
//!   拿到 EIS 连接后作为发送方，用合成器给的绝对指针、按钮、滚轮、键盘设备发事件；坐标是合成器的
//!   全局逻辑坐标，和 `linux_wl.rs` 枚举出的显示器位置是同一个坐标系。
//! - **捕获**：`InputCapture` 会话。在通往别的电脑的屏幕边上设"屏障"，光标撞上屏障时合成器开始把键鼠
//!   交给流界（`Activated`），这时合成一次向外推的移动交给控制器；控制器不切换就立刻放开（`Release`）。
//!   之后的位移、按钮、滚轮、按键从 EIS 连接（接收方）读。这个 portal 没有记住授权的机制，每次启动都会弹窗。
//! - **剪贴板**：KDE 有 data-control（`linux_wl_clipboard.rs`）；GNOME 没有，用挂在 `RemoteDesktop`
//!   会话上的 `Clipboard` portal。
//!
//! 和其他系统不同的地方：
//! - 撞上屏障的那一刻合成器就已经开始捕获，等不了「停留一会儿才切换」，这项设置在这里不起作用；
//!   没切到远端时也看不到键盘，「按住修饰键才切换」和本机上的 Ctrl+Alt+←/→ 同样不起作用。
//! - 合成器可以单方面结束捕获（GNOME / KDE 都留了强制退出的快捷键），这时流界不会立刻知道，
//!   要等心跳超时或再次撞到屏幕边。
//!
//! portal 和 EIS 都是异步接口，各用一个专门的线程跑单线程的 tokio 运行时；对外仍是同步的句柄。
//! 弹窗可能很久没人点，所以启动只等几秒：很快失败的（没有 portal、接口太旧）当场报错，其余在后台继续。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::clipboard::Clipboard;
use ashpd::desktop::input_capture::{ActivatedBarrier, Barrier, Capabilities, InputCapture};
use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
use flow_core::geometry::Edge;
use flow_core::keys::{KeyCode, MouseButton};
use futures_util::{Stream, StreamExt};
use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::linux::{self, Monitor};
use crate::linux_wl_clipboard::Selection;
use crate::linux_wlr_input::{
    Repeat, SWITCH_EDGES, button_code, button_from_code, push_event, repeat_loop,
};
use crate::{
    CaptureHandler, CapturedEvent, InputCapture as CaptureTrait, InputInjector, PlatformError,
    keymap,
};

/// 启动时最多等这么久：很快失败的当场报错，等用户点弹窗的在后台继续。
const START_WAIT: Duration = Duration::from_secs(3);
/// 在 EIS 握手时报的程序名。
const EI_NAME: &str = "FlowAcross";
/// 滚轮一格的单位（和流界、libei 的约定相同）。
const WHEEL_NOTCH: f32 = 120.0;
/// 连续滚动量里相当于一格的距离（逻辑点）。
const SCROLL_PER_NOTCH: f32 = 15.0;

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 单调时钟的微秒数（EIS 的事件时间戳）。
fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(text)
}

/// 把 portal 的报错换成给用户看的话。
fn portal_error(what: &str, e: impl std::fmt::Display) -> String {
    format!(
        "{what}失败：{e}。请确认系统装有 xdg-desktop-portal 及桌面对应的后端，并在弹出的系统窗口里点了允许"
    )
}

// ---------------------------------------------------------------------------
// 恢复令牌
// ---------------------------------------------------------------------------

/// 保存 RemoteDesktop 恢复令牌的文件。有了它，下次启动不用再弹窗。
fn token_path(state_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = state_home
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".local/state")))?;
    Some(base.join("flowacross").join("remote-desktop.token"))
}

fn token_file() -> Option<PathBuf> {
    token_path(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn load_token() -> Option<String> {
    let token = std::fs::read_to_string(token_file()?).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

fn save_token(token: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(path) = token_file() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 令牌相当于"允许控制这台电脑"的凭据，只给自己读。
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path);
    if let Ok(mut f) = file {
        let _ = f.write_all(token.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// 远程桌面会话：注入 + GNOME 的剪贴板
// ---------------------------------------------------------------------------

type Offer = Arc<Vec<(String, Arc<Vec<u8>>)>>;

enum RemoteCmd {
    /// 全局逻辑坐标。
    Move(f64, f64),
    Button(u32, bool),
    /// 1/120 格，流界的方向（向上、向右为正）。
    Wheel(i32, i32),
    /// evdev 键码。
    Key(u32, bool),
    ClipSet(Vec<String>),
    ClipRead(String, mpsc::Sender<Option<Vec<u8>>>),
    /// 写给粘贴方的数据写完了：(序号, 是否成功)。
    ClipWritten(u32, bool),
}

#[derive(Clone, PartialEq)]
enum Status {
    Starting,
    Ready,
    Failed(String),
}

#[derive(Default)]
struct Clip {
    /// 会话带着剪贴板权限。
    available: bool,
    /// 当前选区提供的格式（别的程序复制的）。
    types: Vec<String>,
    /// 流界是提供者时，它提供的内容。
    owned: Option<Offer>,
}

struct Remote {
    tx: UnboundedSender<RemoteCmd>,
    status: Mutex<Status>,
    clip: Mutex<Clip>,
    count: AtomicU64,
}

impl Remote {
    fn check(&self) -> Result<(), PlatformError> {
        match &*lock(&self.status) {
            Status::Failed(e) => Err(PlatformError::Os(e.clone())),
            _ => Ok(()),
        }
    }

    fn send(&self, cmd: RemoteCmd) -> Result<(), PlatformError> {
        self.check()?;
        // 还没准备好（等用户点弹窗）时线程照样收着，轮到处理时没有设备就丢掉。
        let _ = self.tx.send(cmd);
        Ok(())
    }
}

/// 进程里唯一的远程桌面会话，第一次用到时启动。
fn remote() -> Arc<Remote> {
    static REMOTE: OnceLock<Arc<Remote>> = OnceLock::new();
    REMOTE
        .get_or_init(|| {
            let (tx, rx) = unbounded_channel();
            let remote = Arc::new(Remote {
                tx,
                status: Mutex::new(Status::Starting),
                clip: Mutex::new(Clip::default()),
                count: AtomicU64::new(1),
            });
            let for_thread = remote.clone();
            let spawned = std::thread::Builder::new()
                .name("flowacross-portal-remote".into())
                .spawn(move || {
                    let result = runtime().and_then(|rt| rt.block_on(remote_main(rx, &for_thread)));
                    let why = result.err().unwrap_or_else(|| "远程桌面会话已结束".into());
                    *lock(&for_thread.status) = Status::Failed(why);
                });
            if let Err(e) = spawned {
                *lock(&remote.status) = Status::Failed(format!("无法创建线程：{e}"));
            }
            remote
        })
        .clone()
}

/// 等会话启动：最多等 [`START_WAIT`]，已经失败就返回原因，还在等用户点弹窗的当作正常。
fn wait_started(remote: &Remote) -> Result<(), PlatformError> {
    let deadline = Instant::now() + START_WAIT;
    loop {
        match lock(&remote.status).clone() {
            Status::Ready => return Ok(()),
            Status::Failed(e) => return Err(PlatformError::Os(e)),
            Status::Starting if Instant::now() >= deadline => return Ok(()),
            Status::Starting => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// EIS 发送方：记着哪些设备现在可以用。
struct Sender {
    connection: reis::event::Connection,
    ready: Vec<Device>,
    sequence: u32,
}

impl Sender {
    fn on_event(&mut self, event: EiEvent) {
        match event {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button,
                );
            }
            EiEvent::DeviceResumed(e) => {
                // 发事件之前要先声明"开始模拟"。
                self.sequence = self.sequence.wrapping_add(1);
                e.device.device().start_emulating(e.serial, self.sequence);
                self.forget(&e.device);
                self.ready.push(e.device);
            }
            EiEvent::DevicePaused(e) => self.forget(&e.device),
            EiEvent::DeviceRemoved(e) => self.forget(&e.device),
            _ => {}
        }
        let _ = self.connection.flush();
    }

    fn forget(&mut self, device: &Device) {
        self.ready.retain(|d| d.device() != device.device());
    }

    /// 在第一个有这种接口的设备上发一个事件。
    fn emit<T: ei::Interface>(&self, send: impl FnOnce(&T)) {
        for device in &self.ready {
            if let Some(interface) = device.interface::<T>() {
                send(&interface);
                device.device().frame(self.connection.serial(), now_us());
                let _ = self.connection.flush();
                return;
            }
        }
    }

    fn apply(&self, cmd: &RemoteCmd) {
        match *cmd {
            RemoteCmd::Move(x, y) => {
                self.emit::<ei::PointerAbsolute>(|p| p.motion_absolute(x as f32, y as f32));
            }
            RemoteCmd::Button(code, down) => {
                let state = if down {
                    ei::button::ButtonState::Press
                } else {
                    ei::button::ButtonState::Released
                };
                self.emit::<ei::Button>(|b| b.button(code, state));
            }
            RemoteCmd::Wheel(dx, dy) => {
                // libei 里向下、向右为正，单位同样是 120 一格。
                self.emit::<ei::Scroll>(|s| s.scroll_discrete(dx, -dy));
            }
            RemoteCmd::Key(key, down) => {
                let state = if down {
                    ei::keyboard::KeyState::Press
                } else {
                    ei::keyboard::KeyState::Released
                };
                self.emit::<ei::Keyboard>(|k| k.key(key, state));
            }
            _ => {}
        }
    }
}

type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + 'a>>;

async fn remote_main(mut rx: UnboundedReceiver<RemoteCmd>, remote: &Remote) -> Result<(), String> {
    let proxy = RemoteDesktop::new()
        .await
        .map_err(|e| portal_error("连接远程桌面 portal ", e))?;
    let session = proxy
        .create_session()
        .await
        .map_err(|e| portal_error("创建远程桌面会话", e))?;
    // 剪贴板权限要在会话开始前申请；portal 太旧或桌面不支持时没有，不影响键鼠。
    let clipboard = Clipboard::new().await.ok();
    let clip_ok = match &clipboard {
        Some(c) => c.request(&session).await.is_ok(),
        None => false,
    };
    let devices = DeviceType::Keyboard | DeviceType::Pointer;
    let token = load_token();
    let selected = proxy
        .select_devices(
            &session,
            devices,
            token.as_deref(),
            PersistMode::ExplicitlyRevoked,
        )
        .await;
    if selected.and_then(|r| r.response()).is_err() {
        // 旧版 portal 不认识"记住授权"的选项：退回每次都问。
        proxy
            .select_devices(&session, devices, None, PersistMode::DoNot)
            .await
            .and_then(|r| r.response())
            .map_err(|e| portal_error("申请键鼠控制权限", e))?;
    }
    // 这一步会弹系统窗口（没有可用的令牌时），用户点了才返回。
    let started = proxy
        .start(&session, None)
        .await
        .and_then(|r| r.response())
        .map_err(|e| portal_error("申请键鼠控制权限", e))?;
    if let Some(token) = started.restore_token() {
        save_token(token);
    }
    let fd = proxy
        .connect_to_eis(&session)
        .await
        .map_err(|e| portal_error("建立输入通道（需要 GNOME 45 / KDE 6.1 以上）", e))?;
    let context = ei::Context::new(UnixStream::from(fd)).map_err(text)?;
    let (connection, mut events) = context
        .handshake_tokio(EI_NAME, ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| format!("输入通道握手失败：{e}"))?;
    let mut sender = Sender {
        connection,
        ready: Vec::new(),
        sequence: 0,
    };

    let clipboard = clipboard.filter(|_| clip_ok);
    let mut owner_changed: BoxStream<'_, _> = match &clipboard {
        Some(c) => match c.receive_selection_owner_changed().await {
            Ok(s) => Box::pin(s.map(|(_, change)| change)),
            Err(_) => Box::pin(futures_util::stream::pending()),
        },
        None => Box::pin(futures_util::stream::pending()),
    };
    let mut transfers: BoxStream<'_, (String, u32)> = match &clipboard {
        Some(c) => match c.receive_selection_transfer().await {
            Ok(s) => Box::pin(s.map(|(_, mime, serial)| (mime, serial))),
            Err(_) => Box::pin(futures_util::stream::pending()),
        },
        None => Box::pin(futures_util::stream::pending()),
    };
    lock(&remote.clip).available = clipboard.is_some();
    *lock(&remote.status) = Status::Ready;

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else {
                    return Ok(());
                };
                match cmd {
                    RemoteCmd::ClipSet(mimes) => {
                        if let Some(c) = &clipboard {
                            let mimes: Vec<&str> = mimes.iter().map(String::as_str).collect();
                            let _ = c.set_selection(&session, &mimes).await;
                        }
                    }
                    RemoteCmd::ClipRead(mime, reply) => {
                        let fd = match &clipboard {
                            Some(c) => c.selection_read(&session, &mime).await.ok(),
                            None => None,
                        };
                        // 对方可能写得很慢：在单独的线程里读到结束。
                        let _ = std::thread::Builder::new()
                            .name("flowacross-portal-copy".into())
                            .spawn(move || {
                                let data = fd.and_then(|fd| {
                                    let mut data = Vec::new();
                                    std::fs::File::from(std::os::fd::OwnedFd::from(fd)).read_to_end(&mut data).ok()?;
                                    Some(data)
                                });
                                let _ = reply.send(data);
                            });
                    }
                    RemoteCmd::ClipWritten(serial, ok) => {
                        if let Some(c) = &clipboard {
                            let _ = c.selection_write_done(&session, serial, ok).await;
                        }
                    }
                    input => sender.apply(&input),
                }
            }
            event = events.next() => {
                match event {
                    Some(Ok(EiEvent::Disconnected(_))) | None => {
                        return Err("输入通道已断开（桌面收回了键鼠控制权限）".into());
                    }
                    Some(Ok(event)) => sender.on_event(event),
                    Some(Err(e)) => return Err(format!("输入通道出错：{e}")),
                }
            }
            Some(change) = owner_changed.next() => {
                let mut clip = lock(&remote.clip);
                if change.session_is_owner() == Some(true) {
                    // 我们自己刚设的选区，写入时已经计过数。
                    continue;
                }
                clip.owned = None;
                clip.types = change.mime_types();
                remote.count.fetch_add(1, Ordering::SeqCst);
            }
            Some((mime, serial)) = transfers.next() => {
                // 别的程序要粘贴我们提供的内容。
                let data = lock(&remote.clip)
                    .owned
                    .as_ref()
                    .and_then(|o| o.iter().find(|(t, _)| *t == mime).map(|(_, d)| d.clone()));
                let fd = match (&clipboard, &data) {
                    (Some(c), Some(_)) => c.selection_write(&session, serial).await.ok(),
                    _ => None,
                };
                let done = remote.tx.clone();
                match (fd, data) {
                    (Some(fd), Some(data)) => {
                        let _ = std::thread::Builder::new()
                            .name("flowacross-portal-paste".into())
                            .spawn(move || {
                                let ok = std::fs::File::from(std::os::fd::OwnedFd::from(fd)).write_all(&data).is_ok();
                                let _ = done.send(RemoteCmd::ClipWritten(serial, ok));
                            });
                    }
                    _ => {
                        let _ = done.send(RemoteCmd::ClipWritten(serial, false));
                    }
                }
            }
        }
    }
}

pub struct PortalInjector {
    remote: Arc<Remote>,
}

impl PortalInjector {
    pub fn new() -> Result<Self, PlatformError> {
        let remote = remote();
        wait_started(&remote)?;
        Ok(Self { remote })
    }
}

impl InputInjector for PortalInjector {
    fn move_to(&mut self, display_local_id: &str, x: f64, y: f64) -> Result<(), PlatformError> {
        let find = |list: &[Monitor]| {
            list.iter()
                .find(|m| m.id == display_local_id)
                .map(|m| m.logical)
        };
        let mut rect = linux::with_monitors(find);
        if rect.is_none()
            && let Ok(list) = linux::monitors()
        {
            rect = find(&list);
            linux::update_monitors(list);
        }
        let Some(rect) = rect else {
            return Ok(());
        };
        self.remote.send(RemoteCmd::Move(
            rect.x + x.clamp(0.0, rect.width - 0.01),
            rect.y + y.clamp(0.0, rect.height - 0.01),
        ))
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        self.remote
            .send(RemoteCmd::Button(button_code(button), down))
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), PlatformError> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        self.remote.send(RemoteCmd::Wheel(dx, dy))
    }

    fn key(&mut self, code: KeyCode, down: bool, repeat: bool) -> Result<(), PlatformError> {
        // 长按连发由收到按键的程序自己产生。
        if repeat {
            return Ok(());
        }
        let Some(evdev) = keymap::to_linux(code) else {
            return Ok(());
        };
        self.remote.send(RemoteCmd::Key(u32::from(evdev), down))
    }
}

/// portal 的剪贴板（GNOME）。
struct PortalSelection {
    remote: Arc<Remote>,
}

impl Selection for PortalSelection {
    fn types(&self) -> Vec<String> {
        let clip = lock(&self.remote.clip);
        match &clip.owned {
            Some(offer) => offer.iter().map(|(t, _)| t.clone()).collect(),
            None => clip.types.clone(),
        }
    }

    fn read(&self, mime: &str, timeout: Duration) -> Option<Vec<u8>> {
        {
            let clip = lock(&self.remote.clip);
            if let Some(offer) = &clip.owned {
                return offer
                    .iter()
                    .find(|(t, _)| t == mime)
                    .map(|(_, d)| d.to_vec());
            }
            if !clip.available || !clip.types.iter().any(|t| t == mime) {
                return None;
            }
        }
        let (tx, rx) = mpsc::channel();
        self.remote
            .send(RemoteCmd::ClipRead(mime.to_owned(), tx))
            .ok()?;
        rx.recv_timeout(timeout).ok().flatten()
    }

    fn offer(&self, items: Vec<(&str, Vec<u8>)>) -> Result<(), PlatformError> {
        let offer: Offer = Arc::new(
            items
                .into_iter()
                .map(|(t, d)| (t.to_owned(), Arc::new(d)))
                .collect(),
        );
        let mimes: Vec<String> = offer.iter().map(|(t, _)| t.clone()).collect();
        {
            let mut clip = lock(&self.remote.clip);
            if !clip.available {
                return Err(PlatformError::Os(
                    "写入剪贴板失败：还没有拿到桌面的剪贴板权限（请在系统弹窗里点允许）".into(),
                ));
            }
            clip.owned = Some(offer);
            self.remote.count.fetch_add(1, Ordering::SeqCst);
        }
        self.remote.send(RemoteCmd::ClipSet(mimes))
    }

    fn change_count(&self) -> u64 {
        self.remote.count.load(Ordering::SeqCst)
    }
}

/// GNOME 上的剪贴板后端。
pub(crate) fn selection() -> Result<Arc<dyn Selection>, PlatformError> {
    let remote = remote();
    wait_started(&remote)?;
    Ok(Arc::new(PortalSelection { remote }))
}

// ---------------------------------------------------------------------------
// 捕获
// ---------------------------------------------------------------------------

enum CaptureCmd {
    /// 放开键鼠，可选把光标放到指定位置（全局逻辑坐标）。
    Release(Option<(f64, f64)>),
    /// 可切换的屏幕边变了，重设屏障。
    Edges,
    Stop,
}

struct CaptureHandle {
    tx: UnboundedSender<CaptureCmd>,
    /// 引擎要求锁定（正在控制远端）。
    want: Arc<AtomicBool>,
}

/// 正在运行的捕获。
static ACTIVE: Mutex<Option<CaptureHandle>> = Mutex::new(None);

pub fn set_cursor_captured(captured: bool, restore_at: Option<(f64, f64)>) {
    let active = lock(&ACTIVE);
    let Some(handle) = active.as_ref() else {
        return;
    };
    handle.want.store(captured, Ordering::SeqCst);
    if !captured {
        let _ = handle.tx.send(CaptureCmd::Release(restore_at));
    }
}

/// 可切换的屏幕边变了。
pub fn edges_changed() {
    if let Some(handle) = lock(&ACTIVE).as_ref() {
        let _ = handle.tx.send(CaptureCmd::Edges);
    }
}

/// 屏障的两个端点：x1, y1, x2, y2。
type Line = (i32, i32, i32, i32);

/// 一块区域（显示器）某条边上的屏障坐标。portal 的约定：竖线 x1 = x2，横线 y1 = y2；
/// 右边和下边的屏障在区域外侧一格，沿边方向两端都算在内。
fn barrier_line(zone: (i32, i32, i32, i32), edge: Edge) -> (i32, i32, i32, i32) {
    let (x, y, w, h) = zone;
    match edge {
        Edge::Left => (x, y, x, y + h - 1),
        Edge::Right => (x + w, y, x + w, y + h - 1),
        Edge::Top => (x, y, x + w - 1, y),
        Edge::Bottom => (x, y + h, x + w - 1, y + h),
    }
}

/// 要设的屏障：(编号, 边, 坐标)。`zones` 是合成器给的各区域 (x, y, 宽, 高)；
/// `wanted` 为 `None` 时每个区域的四条边都设（区域之间的那些合成器会拒绝，无妨）。
fn barrier_plan(
    zones: &[(i32, i32, i32, i32)],
    monitors: &[Monitor],
    wanted: Option<&[(String, Edge)]>,
) -> Vec<(u32, Edge, Line)> {
    let mut plan = Vec::new();
    for &zone in zones {
        // 区域和显示器按位置、尺寸对上号。
        let monitor = monitors.iter().find(|m| {
            let r = m.logical;
            (r.x.round() as i32, r.y.round() as i32) == (zone.0, zone.1)
                && (r.width.round() as i32, r.height.round() as i32) == (zone.2, zone.3)
        });
        for edge in Edge::ALL {
            let on = match (wanted, monitor) {
                (None, _) => true,
                (Some(w), Some(m)) => w.iter().any(|(id, e)| *id == m.id && *e == edge),
                (Some(_), None) => false,
            };
            if on {
                plan.push((plan.len() as u32 + 1, edge, barrier_line(zone, edge)));
            }
        }
    }
    plan
}

/// 一帧里攒起来的滚动量，换成流界的滚动事件（1/120 格，向上、向右为正）。
#[derive(Default)]
struct Scroll {
    /// 滚轮：1/120 格（横向、纵向，libei 的方向）。
    discrete: (i32, i32),
    /// 触摸板的连续滚动量（逻辑点）。
    smooth: (f32, f32),
}

impl Scroll {
    fn take(&mut self) -> Option<CapturedEvent> {
        let this = std::mem::take(self);
        let pick = |discrete: i32, smooth: f32| {
            if discrete != 0 {
                discrete
            } else {
                (smooth / SCROLL_PER_NOTCH * WHEEL_NOTCH).round() as i32
            }
        };
        let dx = pick(this.discrete.0, this.smooth.0);
        let dy = -pick(this.discrete.1, this.smooth.1);
        (dx != 0 || dy != 0).then_some(CapturedEvent::Wheel { dx, dy })
    }
}

/// 读区域、设屏障、启用。返回屏障编号对应的边。
async fn arm(
    portal: &InputCapture<'_>,
    session: &ashpd::desktop::Session<'_, InputCapture<'_>>,
) -> Result<HashMap<u32, Edge>, String> {
    let zones = portal
        .zones(session)
        .await
        .and_then(|r| r.response())
        .map_err(|e| portal_error("读取屏幕区域", e))?;
    let regions: Vec<(i32, i32, i32, i32)> = zones
        .regions()
        .iter()
        .map(|r| {
            (
                r.x_offset(),
                r.y_offset(),
                r.width() as i32,
                r.height() as i32,
            )
        })
        .collect();
    let monitors = linux::monitors().unwrap_or_default();
    linux::update_monitors(monitors.clone());
    let wanted = lock(&SWITCH_EDGES).clone();
    let plan = barrier_plan(&regions, &monitors, wanted.as_deref());
    if plan.is_empty() {
        // 没有可以切过去的边：不捕获。
        let _ = portal.disable(session).await;
        return Ok(HashMap::new());
    }
    let barriers: Vec<Barrier> = plan
        .iter()
        .filter_map(|(id, _, line)| Some(Barrier::new(NonZeroU32::new(*id)?, *line)))
        .collect();
    let failed: Vec<u32> = portal
        .set_pointer_barriers(session, &barriers, zones.zone_set())
        .await
        .and_then(|r| r.response())
        .map_err(|e| portal_error("设置屏幕边的捕获屏障", e))?
        .failed_barriers()
        .iter()
        .map(|id| id.get())
        .collect();
    portal
        .enable(session)
        .await
        .map_err(|e| portal_error("启用键鼠捕获", e))?;
    Ok(plan
        .into_iter()
        .filter(|(id, _, _)| !failed.contains(id))
        .map(|(id, edge, _)| (id, edge))
        .collect())
}

struct CaptureShared {
    handler: Mutex<CaptureHandler>,
    want: Arc<AtomicBool>,
}

impl CaptureShared {
    fn report(&self, event: CapturedEvent) {
        (lock(&self.handler))(event);
    }

    fn wanted(&self) -> bool {
        self.want.load(Ordering::SeqCst)
    }
}

async fn capture_main(
    mut rx: UnboundedReceiver<CaptureCmd>,
    shared: Arc<CaptureShared>,
    repeat: mpsc::Sender<Repeat>,
    started: mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let portal = InputCapture::new()
        .await
        .map_err(|e| portal_error("连接输入捕获 portal（需要 GNOME 45 / KDE 6.1 以上）", e))?;
    // 这一步可能弹系统窗口。
    let (session, _) = portal
        .create_session(None, Capabilities::Keyboard | Capabilities::Pointer)
        .await
        .map_err(|e| portal_error("申请键鼠捕获权限", e))?;
    let fd = portal
        .connect_to_eis(&session)
        .await
        .map_err(|e| portal_error("建立输入通道", e))?;
    let context = ei::Context::new(UnixStream::from(fd)).map_err(text)?;
    let (connection, mut events) = context
        .handshake_tokio(EI_NAME, ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| format!("输入通道握手失败：{e}"))?;
    let fail = |e: ashpd::Error| portal_error("订阅键鼠捕获的通知", e);
    let mut activated = Box::pin(portal.receive_activated().await.map_err(fail)?);
    let mut deactivated = Box::pin(portal.receive_deactivated().await.map_err(fail)?);
    let mut zones_changed = Box::pin(portal.receive_zones_changed().await.map_err(fail)?);
    let mut disabled = Box::pin(portal.receive_disabled().await.map_err(fail)?);
    let mut barriers = arm(&portal, &session).await?;
    let _ = started.send(Ok(()));

    // 合成器正在把键鼠交给我们，以及这次的编号。
    let mut active: Option<Option<u32>> = None;
    let mut scroll = Scroll::default();
    // 已经上报了按下、还没上报抬起的键。
    let mut pressed: Vec<KeyCode> = Vec::new();
    loop {
        // 已经回到本机时，替还按着的键补上抬起：键盘一还给本机，真正的抬起就收不到了，
        // 不补的话控制器会一直以为这些键按着（切换用的方向键会被一直"吞掉"，下次要按两下才生效）。
        if !pressed.is_empty() && !shared.wanted() {
            for code in std::mem::take(&mut pressed) {
                let _ = repeat.send(Repeat::Up(code));
                shared.report(CapturedEvent::Key {
                    code,
                    down: false,
                    repeat: false,
                });
            }
        }
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None | Some(CaptureCmd::Stop) => break,
                Some(CaptureCmd::Release(at)) => {
                    if let Some(id) = active.take() {
                        let _ = portal.release(&session, id, at).await;
                    }
                }
                Some(CaptureCmd::Edges) => {
                    // 正在控制远端时不动屏障，回到本机后引擎还会再同步一次。
                    if active.is_none() {
                        barriers = arm(&portal, &session).await.unwrap_or_default();
                    }
                }
            },
            Some(event) = activated.next() => {
                let id = event.activation_id();
                active = Some(id);
                let edge = match event.barrier_id() {
                    Some(ActivatedBarrier::Barrier(b)) => barriers.get(&b.get()).copied(),
                    _ => None,
                };
                let at = event.cursor_position();
                if let (Some(edge), Some((x, y))) = (edge, at) {
                    // 控制器在回调里决定切换时会把 `want` 置为真。
                    shared.report(push_event(edge, (f64::from(x), f64::from(y))));
                }
                if !shared.wanted() {
                    // 这次不切换（没有可去的屏幕、被锁定、防误触没通过）：马上把键鼠还给本机。
                    let at = at.map(|(x, y)| (f64::from(x), f64::from(y)));
                    let _ = portal.release(&session, id, at).await;
                    active = None;
                }
            }
            Some(_) = deactivated.next() => active = None,
            Some(_) = zones_changed.next() => {
                // 显示器配置变了，屏障作废，重新设。
                active = None;
                barriers = arm(&portal, &session).await.unwrap_or_default();
            }
            Some(_) = disabled.next() => {
                active = None;
                barriers = arm(&portal, &session).await.unwrap_or_default();
            }
            event = events.next() => {
                let event = match event {
                    Some(Ok(EiEvent::Disconnected(_))) | None => {
                        return Err("输入通道已断开".into());
                    }
                    Some(Ok(event)) => event,
                    Some(Err(e)) => return Err(format!("输入通道出错：{e}")),
                };
                if let EiEvent::SeatAdded(e) = &event {
                    e.seat.bind_capabilities(
                        DeviceCapability::Pointer
                            | DeviceCapability::Keyboard
                            | DeviceCapability::Scroll
                            | DeviceCapability::Button,
                    );
                    let _ = connection.flush();
                    continue;
                }
                // D-Bus 的"开始捕获"和这条通道上的事件先后不定，是否上报只看控制器要不要。
                if !shared.wanted() {
                    scroll = Scroll::default();
                    continue;
                }
                match event {
                    EiEvent::PointerMotion(m) if m.dx != 0.0 || m.dy != 0.0 => {
                        shared.report(CapturedEvent::MouseMove {
                            x: 0.0,
                            y: 0.0,
                            dx: f64::from(m.dx),
                            dy: f64::from(m.dy),
                        });
                    }
                    EiEvent::Button(b) => {
                        if let Some(button) = button_from_code(b.button) {
                            shared.report(CapturedEvent::Button {
                                button,
                                down: b.state == ei::button::ButtonState::Press,
                            });
                        }
                    }
                    EiEvent::ScrollDiscrete(s) => {
                        scroll.discrete.0 += s.discrete_dx;
                        scroll.discrete.1 += s.discrete_dy;
                    }
                    EiEvent::ScrollDelta(s) => {
                        scroll.smooth.0 += s.dx;
                        scroll.smooth.1 += s.dy;
                    }
                    EiEvent::Frame(_) => {
                        if let Some(event) = scroll.take() {
                            shared.report(event);
                        }
                    }
                    EiEvent::KeyboardKey(k) => {
                        let down = k.state == ei::keyboard::KeyState::Press;
                        if let Some(code) = u16::try_from(k.key).ok().and_then(keymap::from_linux) {
                            let _ = repeat.send(if down {
                                Repeat::Down(code)
                            } else {
                                Repeat::Up(code)
                            });
                            pressed.retain(|c| *c != code);
                            if down {
                                pressed.push(code);
                            }
                            shared.report(CapturedEvent::Key {
                                code,
                                down,
                                repeat: false,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(id) = active {
        let _ = portal.release(&session, id, None).await;
    }
    let _ = session.close().await;
    Ok(())
}

#[derive(Default)]
pub struct PortalCapture {
    tx: Option<UnboundedSender<CaptureCmd>>,
    thread: Option<JoinHandle<()>>,
    repeat: Option<(mpsc::Sender<Repeat>, JoinHandle<()>)>,
}

impl CaptureTrait for PortalCapture {
    fn start(&mut self, handler: CaptureHandler) -> Result<(), PlatformError> {
        self.stop();
        let spawn_error = |e: std::io::Error| PlatformError::Os(format!("无法创建输入线程：{e}"));
        let want = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(CaptureShared {
            handler: Mutex::new(handler),
            want: want.clone(),
        });
        let (tx, rx) = unbounded_channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (repeat_tx, repeat_rx) = mpsc::channel();

        let for_repeat = shared.clone();
        let repeat_thread = std::thread::Builder::new()
            .name("flowacross-portal-repeat".into())
            .spawn(move || {
                repeat_loop(
                    repeat_rx,
                    || for_repeat.wanted(),
                    |event| for_repeat.report(event),
                )
            })
            .map_err(spawn_error)?;
        let for_main = shared.clone();
        let repeat_for_main = repeat_tx.clone();
        let thread = std::thread::Builder::new()
            .name("flowacross-portal-capture".into())
            .spawn(move || {
                let failed = started_tx.clone();
                let result = runtime().and_then(|rt| {
                    rt.block_on(capture_main(rx, for_main, repeat_for_main, started_tx))
                });
                if let Err(e) = result {
                    let _ = failed.send(Err(e));
                }
            })
            .map_err(spawn_error)?;
        self.repeat = Some((repeat_tx, repeat_thread));
        self.thread = Some(thread);
        self.tx = Some(tx.clone());

        // 很快失败的当场报错；等用户点弹窗的在后台继续。
        if let Ok(Err(e)) = started_rx.recv_timeout(START_WAIT) {
            self.stop();
            return Err(PlatformError::Os(e));
        }
        *lock(&ACTIVE) = Some(CaptureHandle { tx, want });
        // 顺便把注入用的会话也启动起来：它的授权弹窗最好在用户还坐在这台电脑前时出现，
        // 而不是等别的电脑第一次把鼠标移过来的时候。
        let _ = remote();
        Ok(())
    }

    fn stop(&mut self) {
        *lock(&ACTIVE) = None;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(CaptureCmd::Stop);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some((tx, t)) = self.repeat.take() {
            let _ = tx.send(Repeat::Stop);
            let _ = t.join();
        }
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop();
    }
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
    fn barriers_sit_on_the_zone_edges() {
        let zone = (1920, 0, 2560, 1440);
        assert_eq!(barrier_line(zone, Edge::Left), (1920, 0, 1920, 1439));
        assert_eq!(barrier_line(zone, Edge::Right), (4480, 0, 4480, 1439));
        assert_eq!(barrier_line(zone, Edge::Top), (1920, 0, 4479, 0));
        assert_eq!(barrier_line(zone, Edge::Bottom), (1920, 1440, 4479, 1440));
    }

    #[test]
    fn barriers_only_go_on_edges_that_lead_somewhere() {
        let zones = [(0, 0, 1920, 1080), (1920, 0, 2560, 1440)];
        let monitors = [
            monitor("eDP-1", Rect::new(0.0, 0.0, 1920.0, 1080.0)),
            monitor("DP-1", Rect::new(1920.0, 0.0, 2560.0, 1440.0)),
        ];
        // 还没收到设置：每个区域四条边都设。
        assert_eq!(barrier_plan(&zones, &monitors, None).len(), 8);
        let wanted = vec![("DP-1".to_owned(), Edge::Right)];
        assert_eq!(
            barrier_plan(&zones, &monitors, Some(&wanted)),
            vec![(1, Edge::Right, (4480, 0, 4480, 1439))]
        );
        // 编号从 1 开始连续（0 不是合法的屏障编号）。
        let wanted = vec![
            ("eDP-1".to_owned(), Edge::Left),
            ("DP-1".to_owned(), Edge::Top),
        ];
        let ids: Vec<u32> = barrier_plan(&zones, &monitors, Some(&wanted))
            .iter()
            .map(|b| b.0)
            .collect();
        assert_eq!(ids, vec![1, 2]);
        // 区域和显示器对不上号时不乱设。
        let moved = [monitor("DP-1", Rect::new(5.0, 5.0, 2560.0, 1440.0))];
        assert!(barrier_plan(&zones, &moved, Some(&wanted)).is_empty());
        assert!(barrier_plan(&zones, &monitors, Some(&[])).is_empty());
    }

    #[test]
    fn scroll_prefers_wheel_clicks_and_flips_vertical() {
        // 滚轮向下一格：libei 里是 +120，流界里是 -120。
        let mut s = Scroll {
            discrete: (0, 120),
            smooth: (0.0, 15.0),
        };
        assert_eq!(s.take(), Some(CapturedEvent::Wheel { dx: 0, dy: -120 }));
        assert_eq!(s.take(), None);
        let mut s = Scroll {
            discrete: (0, 0),
            smooth: (7.5, -3.75),
        };
        assert_eq!(s.take(), Some(CapturedEvent::Wheel { dx: 60, dy: 30 }));
    }

    #[test]
    fn token_lives_under_the_state_directory() {
        assert_eq!(
            token_path(Some("/data/state".into()), Some("/home/me".into())),
            Some(PathBuf::from("/data/state/flowacross/remote-desktop.token"))
        );
        assert_eq!(
            token_path(None, Some("/home/me".into())),
            Some(PathBuf::from(
                "/home/me/.local/state/flowacross/remote-desktop.token"
            ))
        );
        // 相对路径的 XDG_STATE_HOME 按规范忽略。
        assert_eq!(
            token_path(Some("relative".into()), Some("/home/me".into())),
            Some(PathBuf::from(
                "/home/me/.local/state/flowacross/remote-desktop.token"
            ))
        );
        assert_eq!(token_path(None, None), None);
    }
}
