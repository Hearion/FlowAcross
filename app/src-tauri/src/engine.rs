//! 引擎：管理本机身份、配置、设备发现、连接、配对与自动重连、跨屏控制，并把状态推给界面。
//!
//! 并发模型：所有可变状态在一把 `std::sync::Mutex` 里，**持锁期间不 await**；
//! 每条连接一个 tokio 任务（握手 → 配对或会话），写方向再单独一个任务。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use flow_core::Rect;
use flow_core::clipboard::ClipboardSync;
use flow_core::files::{FileOffer, OfferBook, peer_supports_files};
use flow_core::keys::{InputEvent, KeyCode, Modifiers, PressedState};
use flow_core::layout::{DeviceId, Display, DisplayId, Layout};
use flow_core::pairing::{PairingCode, device_id, pairing_code};
use flow_core::protocol::{ByeReason, DisplayInfo, Message, Os, PROTOCOL_VERSION};
use flow_core::switching::SwitchPolicy;
use flow_core::throttle::MoveThrottle;
use flow_net::discovery::{Advert, Discovery, DiscoveryEvent, Found};
use flow_net::quinn;
use flow_net::{Identity, Link, MoveReceiver, MoveSender, peer_fingerprint};
use flow_platform::{InputCapture, InputInjector, Permission, PermissionState, PlatformError};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{Config, HoldKey, PeerRecord, Settings};
use crate::input::{ControlEvent, Controller, ControllerConfig};
use crate::transfer::{self, TransferView, Transfers};

const SNAP_THRESHOLD: f64 = 48.0;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);
const TICK: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// 设备离线后自动重连的次数（约 30 秒内）。用完后停止，等用户手动连接或对方主动连过来。
const MAX_AUTO_ATTEMPTS: u32 = 5;
/// 被控制期间超过这么久没收到对方任何消息（含心跳），就自行释放按键、结束被控制。
const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
/// 两次发送鼠标位置的最小间隔（毫秒），即最多 250 次/秒，高于常见屏幕刷新率。
const MOUSE_SEND_INTERVAL_MS: u64 = 4;

/// 握手时得到的对方信息。
#[derive(Debug, Clone)]
struct PeerInfo {
    id: DeviceId,
    name: String,
    os: Os,
    /// 对方的程序版本，用来判断它支持哪些功能（例如数据报鼠标位置）。
    version: String,
}
const INPUT_TICK: Duration = Duration::from_millis(40);
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);
/// 传文件期间推送进度的最小间隔。
const PROGRESS_EMIT: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// 推给界面的状态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct StateView {
    pub local: LocalView,
    pub onboarded: bool,
    pub port: u16,
    pub layout: Layout,
    pub devices: Vec<DeviceView>,
    pub nearby: Vec<NearbyView>,
    pub pairings: Vec<PairingView>,
    pub settings: Settings,
    pub locked: bool,
    pub permissions: Vec<PermissionView>,
    /// 权限相关的额外说明（例如开发模式下权限归属于终端）。
    pub permission_hint: Option<String>,
    /// 本机正在被哪台设备控制。
    pub controlled_by: Option<DeviceId>,
    pub low_latency: LowLatencyView,
    pub about: AboutView,
    /// 正在进行和最近结束的文件传输，最新的在前。
    pub transfers: Vec<TransferView>,
    /// 其他设备复制了文件、等待用户手动接收（太大没有自动接收）。
    pub file_offer: Option<FileOfferView>,
    /// 收到的文件保存在这里；`None` 表示找不到下载文件夹，无法接收文件。
    pub receive_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileOfferView {
    pub peer: DeviceId,
    pub peer_name: String,
    pub label: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LowLatencyView {
    /// 本机系统是否支持（只有 macOS）。
    pub supported: bool,
    /// 后台服务是否已安装。
    pub installed: bool,
    /// 现在是否正在关闭 AWDL（开启了且有设备连着）。
    pub active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AboutView {
    pub version: &'static str,
    pub protocol: u16,
    pub data_dir: String,
    /// 本机证书指纹，分组显示，用于人工核对身份。
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalView {
    pub id: DeviceId,
    pub name: String,
    pub os: Os,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Local,
    Connected,
    Connecting,
    Offline,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    pub id: DeviceId,
    pub name: String,
    pub os: Os,
    pub status: DeviceStatus,
    /// 自动重连已停止，需要手动连接。
    pub manual: bool,
    /// 本机的键鼠当前是否在这台设备上。
    pub active: bool,
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NearbyView {
    pub id: DeviceId,
    pub name: String,
    pub os: Os,
    /// 正在连接或配对中。
    pub connecting: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairingView {
    pub id: DeviceId,
    pub name: String,
    pub os: Os,
    /// "123 456"
    pub code: String,
    /// 本机发起的配对。
    pub outgoing: bool,
    pub local_confirmed: bool,
    pub remote_confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionView {
    pub kind: &'static str,
    pub state: &'static str,
}

pub enum EngineEvent {
    State(Box<StateView>),
    Notice(String),
}

pub type Notify = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// 把一个闭包派发到主线程执行（macOS 上部分系统接口只能在主线程调用）。
pub type MainThread = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

pub struct EngineOptions {
    pub data_dir: PathBuf,
    pub port: u16,
    /// 是否启用 mDNS（测试中关闭）。
    pub discovery: bool,
    /// 主线程调度器；没有时（测试）跳过需要主线程的刷新。
    pub main_thread: Option<MainThread>,
    /// 是否启用键鼠捕获与注入（测试中关闭，避免动到真实的鼠标键盘）。同时也控制是否读写系统剪贴板。
    pub input: bool,
    /// 从其他设备收到的文件保存在这里（下载文件夹里的「流界」）；`None` 时不能接收文件。
    pub receive_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    /// 启动时所在的 tokio 运行时。界面命令可能在主线程调用，派生任务一律用它。
    rt: tokio::runtime::Handle,
    identity: Identity,
    data_dir: PathBuf,
    endpoint: quinn::Endpoint,
    port: u16,
    notify: Notify,
    main_thread: Option<MainThread>,
    discovery: Mutex<Option<Discovery>>,
    state: Mutex<State>,
    input: bool,
    /// 跨屏控制器。锁顺序：先 `state` 后 `controller`；捕获回调只拿 `controller`。
    controller: Arc<Mutex<Controller>>,
    capture: Mutex<Option<Box<dyn InputCapture>>>,
    capture_warned: AtomicBool,
    /// 系统接口失败后，下次重试的时间（避免频繁调用系统接口）。
    capture_retry_at: Mutex<Option<Instant>>,
    /// 被控制方：当前控制本机的设备，以及它按下的键（失败时据此释放）。
    controlled: Mutex<Option<Controlled>>,
    injector: Mutex<Option<Box<dyn InputInjector>>>,
    accessibility_warned: AtomicBool,
    /// 锁顺序：`state` → `clipboard` → `files`，持有后两者时不再拿 `state`。
    clipboard: Mutex<ClipboardState>,
    files: Mutex<FilesState>,
    receive_dir: Option<PathBuf>,
    /// 引擎启动时刻，给 `flow_core` 里需要毫秒时间的逻辑用。
    started: Instant,
    /// 上次推送传输进度的时间。
    progress_emitted: Mutex<Instant>,
    /// 低延迟模式当前是否在生效（已通知后台服务关闭 AWDL）；`None` 表示还没同步过。
    low_latency_active: Mutex<Option<bool>>,
}

/// 剪贴板同步状态。系统剪贴板没有变化通知，按变化计数轮询。
struct ClipboardState {
    sync: ClipboardSync,
    access: Option<Box<dyn flow_platform::ClipboardAccess>>,
    /// 上次看到的变化计数；`None` 表示还没读过（第一次读取只记录，不发送启动前就有的内容）。
    last_count: Option<u64>,
}

/// 跨设备复制文件的状态。
#[derive(Default)]
struct FilesState {
    /// 本机剪贴板里的文件：发给其他设备的清单和对应的本机路径。
    local: Option<(FileOffer, Vec<PathBuf>)>,
    next_offer: u64,
    /// 其他设备复制的文件，决定什么时候去取。
    book: OfferBook,
    transfers: Transfers,
    /// 剪贴板内容的"代"：本机复制了东西或写入了其他设备的文字时加一。
    /// 文件收完时代已经变了，说明用户又复制了别的，就不再覆盖剪贴板。
    epoch: u64,
}

struct Controlled {
    by: DeviceId,
    by_os: Os,
    session: u32,
    pressed: PressedState,
    last_seen: Instant,
}

/// Mac 与 Windows 之间互相控制时，⌘ 与 Ctrl 互换：在 Mac 上按 ⌘C，Windows 收到 Ctrl+C，反之亦然。
fn map_key(from: Os, code: KeyCode) -> KeyCode {
    let local = Os::current();
    if from != local && (from == Os::MacOs || local == Os::MacOs) {
        code.swap_meta_ctrl()
    } else {
        code
    }
}

fn switch_policy(settings: &Settings, locked: bool) -> SwitchPolicy {
    SwitchPolicy {
        require_modifiers: match settings.hold_key {
            HoldKey::None => Modifiers::NONE,
            HoldKey::Ctrl => Modifiers::CTRL,
            HoldKey::Alt => Modifiers::ALT,
            HoldKey::Shift => Modifiers::SHIFT,
        },
        dwell_ms: settings.dwell_ms,
        corner_deadzone: if settings.corner_guard { 10.0 } else { 0.0 },
        locked,
    }
}

#[derive(Default)]
struct State {
    config: Config,
    local_displays: Vec<DisplayInfo>,
    nearby: HashMap<DeviceId, Found>,
    sessions: HashMap<DeviceId, Session>,
    pairings: HashMap<DeviceId, Pairing>,
    connecting: HashSet<DeviceId>,
    backoff: HashMap<DeviceId, Backoff>,
    /// 自动重连次数已用完、等待手动连接的设备。
    manual: HashSet<DeviceId>,
    locked: bool,
}

struct Session {
    tx: mpsc::UnboundedSender<Message>,
    conn: quinn::Connection,
    /// 发起这条连接的设备。两边同时连上对方时，双方都保留"发起方 ID 较小"的那条，保证结果一致。
    initiator: DeviceId,
    /// 对方支持跨设备复制文件（看版本）。
    files: bool,
}

struct Pairing {
    name: String,
    os: Os,
    code: PairingCode,
    outgoing: bool,
    local: bool,
    remote: bool,
    cmd: mpsc::UnboundedSender<bool>,
}

struct Backoff {
    next: Instant,
    delay: Duration,
    attempts: u32,
}

/// 默认设备名：macOS 用系统设置里的电脑名称（如"小严的 MacBook Pro"），其他系统用主机名并去掉 `.local`。
pub fn system_device_name() -> String {
    #[cfg(target_os = "macos")]
    {
        let name = std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        if let Some(n) = name {
            return n;
        }
    }
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    host.strip_suffix(".local").unwrap_or(&host).to_owned()
}

fn os_tag(os: Os) -> &'static str {
    match os {
        Os::MacOs => "macos",
        Os::Windows => "windows",
        Os::Linux => "linux",
        Os::Other => "other",
    }
}

fn os_from_tag(tag: &str) -> Os {
    match tag {
        "macos" => Os::MacOs,
        "windows" => Os::Windows,
        "linux" => Os::Linux,
        _ => Os::Other,
    }
}

fn permission_kind(p: Permission) -> &'static str {
    match p {
        Permission::Accessibility => "accessibility",
        Permission::InputMonitoring => "input_monitoring",
        Permission::LocalNetwork => "local_network",
    }
}

/// macOS 按"负责进程"记录权限：没有打包成 .app 运行时（`npm run tauri dev`），
/// 权限会算在启动它的终端或编辑器上，系统设置的列表里看不到流界。
/// 平台错误里给用户看的部分（去掉"系统调用失败："之类的前缀）。
fn platform_reason(e: &PlatformError) -> String {
    match e {
        PlatformError::Os(m) => m.clone(),
        other => other.to_string(),
    }
}

fn permission_hint() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    if exe.to_string_lossy().contains(".app/Contents/MacOS/") {
        return None;
    }
    Some(
        "当前以开发模式运行，系统会把权限算在启动流界的终端上（列表里显示为「终端」或你使用的编辑器），\
         而不是流界本身。请在系统设置的「辅助功能」和「输入监控」里打开它的开关，然后重新运行 npm run tauri dev。"
            .to_owned(),
    )
}

pub fn parse_permission(kind: &str) -> Option<Permission> {
    match kind {
        "accessibility" => Some(Permission::Accessibility),
        "input_monitoring" => Some(Permission::InputMonitoring),
        "local_network" => Some(Permission::LocalNetwork),
        _ => None,
    }
}

/// 发出最后一条消息（拒绝、Bye）后再关闭：QUIC 立即关闭会丢弃尚未送达的数据，
/// 所以先等对方收到后主动断开（最多 0.8 秒），再由本端关闭。
async fn linger_close(conn: &quinn::Connection, reason: &'static [u8]) {
    let _ = tokio::time::timeout(Duration::from_millis(800), conn.closed()).await;
    conn.close(0u32.into(), reason);
}

/// 日志里只打印设备 ID 前 8 位。
fn short(id: &DeviceId) -> &str {
    &id.0[..id.0.len().min(8)]
}

fn timed_out(what: &'static str) -> impl FnOnce(tokio::time::error::Elapsed) -> String {
    move |_| what.to_owned()
}

fn to_display(device: &DeviceId, d: &DisplayInfo, (ox, oy): (f64, f64)) -> Display {
    Display {
        id: DisplayId {
            device: device.clone(),
            local: d.local_id.clone(),
        },
        name: d.name.clone(),
        rect: Rect::new(d.rect.x + ox, d.rect.y + oy, d.rect.width, d.rect.height),
        scale: d.scale,
        native: d.native,
    }
}

fn bounds(displays: &[DisplayInfo]) -> Option<(f64, f64)> {
    let min_x = displays.iter().map(|d| d.rect.x).reduce(f64::min)?;
    let min_y = displays.iter().map(|d| d.rect.y).reduce(f64::min)?;
    Some((min_x, min_y))
}

enum Remote {
    Move(String, f64, f64),
    Button(flow_core::keys::MouseButton, bool),
    Wheel(i32, i32),
    Key(KeyCode, bool, bool),
    Release,
    ReleaseKeys,
}

fn capture_error_text(e: &PlatformError) -> String {
    match e {
        PlatformError::PermissionDenied(_) => {
            "缺少「输入监控」或「辅助功能」权限，键鼠暂时无法切到其他电脑。\
             请在 设置 › 系统权限 中授权，授权后重新打开流界。"
                .to_owned()
        }
        other => {
            format!("无法监听键鼠（{other}），键鼠暂时无法切到其他电脑。请重新打开流界后再试。")
        }
    }
}

impl State {
    /// 是否可以和这台设备互相复制文件（设置开着、没有暂停）。
    fn files_allowed(&self, peer: &DeviceId) -> bool {
        let s = &self.config.settings;
        s.clipboard
            && s.clipboard_files
            && !s.sharing_paused
            && self.config.peer(peer).is_some_and(|p| !p.paused)
    }

    fn layout(&self, local: &DeviceId) -> Layout {
        let mut displays: Vec<Display> = self
            .local_displays
            .iter()
            .map(|d| to_display(local, d, self.config.local_offset))
            .collect();
        for p in &self.config.peers {
            if let Some(off) = p.offset {
                displays.extend(p.displays.iter().map(|d| to_display(&p.id, d, off)));
            }
        }
        Layout::new(displays)
    }

    /// 新设备第一次上报显示器时，放到现有布局的右侧、顶边与本机对齐。用户之后可以拖动调整。
    fn place_new_peer(&mut self, local: &DeviceId, id: &DeviceId) {
        let layout = self.layout(local);
        let local_top = layout
            .displays_of(local)
            .map(|d| d.rect.y)
            .reduce(f64::min)
            .unwrap_or(0.0);
        let right = layout
            .displays
            .iter()
            .map(|d| d.rect.right())
            .reduce(f64::max)
            .unwrap_or(0.0);
        let Some(peer) = self.config.peer_mut(id) else {
            return;
        };
        if peer.offset.is_some() {
            return;
        }
        if let Some((min_x, min_y)) = bounds(&peer.displays) {
            peer.offset = Some((right - min_x, local_top - min_y));
        }
    }
}

impl Engine {
    pub async fn start(opts: EngineOptions, notify: Notify) -> Result<Engine, String> {
        let identity = Identity::load_or_create(&opts.data_dir).map_err(|e| e.to_string())?;
        let config = Config::load(&opts.data_dir);
        let endpoint = flow_net::bind_endpoint(&identity, opts.port).map_err(|e| e.to_string())?;
        let port = endpoint.local_addr().map_err(|e| e.to_string())?.port();
        let local_displays = flow_platform::display_provider()
            .displays()
            .unwrap_or_default();

        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let cursor: crate::input::CursorControl = if opts.input {
            Box::new(flow_platform::set_cursor_captured)
        } else {
            Box::new(|_, _| {})
        };
        let controller = Controller::new(identity.device_id.clone(), ctl_tx, cursor);
        let clipboard_sync = ClipboardSync::new(identity.device_id.clone());
        let engine = Engine {
            inner: Arc::new(Inner {
                rt: tokio::runtime::Handle::current(),
                identity,
                data_dir: opts.data_dir,
                endpoint,
                port,
                notify,
                main_thread: opts.main_thread,
                discovery: Mutex::new(None),
                state: Mutex::new(State {
                    config,
                    local_displays,
                    ..Default::default()
                }),
                input: opts.input,
                controller: Arc::new(Mutex::new(controller)),
                capture: Mutex::new(None),
                capture_warned: AtomicBool::new(false),
                capture_retry_at: Mutex::new(None),
                controlled: Mutex::new(None),
                injector: Mutex::new(None),
                accessibility_warned: AtomicBool::new(false),
                clipboard: Mutex::new(ClipboardState {
                    sync: clipboard_sync,
                    access: None,
                    last_count: None,
                }),
                files: Mutex::new(FilesState::default()),
                receive_dir: opts.receive_dir,
                started: Instant::now(),
                progress_emitted: Mutex::new(Instant::now()),
                low_latency_active: Mutex::new(None),
            }),
        };
        if let Some(dir) = &engine.inner.receive_dir {
            transfer::clean_staging(dir);
        }

        if opts.discovery {
            let (tx, rx) = mpsc::unbounded_channel();
            let advert = Advert {
                device: engine.inner.identity.device_id.clone(),
                name: engine.name(),
                os: os_tag(Os::current()).to_owned(),
                port,
            };
            match Discovery::start(advert, tx) {
                Ok(d) => *engine.inner.discovery.lock().expect("discovery") = Some(d),
                Err(e) => engine.notice(format!(
                    "无法在局域网中广播本机（{e}），其他电脑将找不到这台设备。"
                )),
            }
            tokio::spawn(engine.clone().discovery_loop(rx));
        }
        tokio::spawn(engine.clone().accept_loop());
        tokio::spawn(engine.clone().maintenance_loop());
        tokio::spawn(engine.clone().control_event_loop(ctl_rx));
        tokio::spawn(engine.clone().input_loop());
        tokio::spawn(engine.clone().clipboard_loop());
        engine.sync_input();
        Ok(engine)
    }

    /// 退出前调用：立即通知所有对端断开，让对方马上释放按键、显示离线。
    pub fn shutdown(&self) {
        self.controller().force_local();
        if let Some(mut c) = self.inner.capture.lock().expect("capture").take() {
            c.stop();
        }
        self.release_controlled(None);
        self.inner
            .files
            .lock()
            .expect("files")
            .transfers
            .cancel_all();
        if self.inner.input {
            // 退出前恢复 AWDL。
            flow_platform::low_latency::set_active(false);
        }
        self.inner.endpoint.close(0u32.into(), b"shutdown");
    }

    fn controller(&self) -> MutexGuard<'_, Controller> {
        self.inner
            .controller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.inner.data_dir
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().expect("engine state poisoned")
    }

    fn local_id(&self) -> &DeviceId {
        &self.inner.identity.device_id
    }

    fn name_of(&self, st: &State) -> String {
        st.config
            .device_name
            .clone()
            .unwrap_or_else(system_device_name)
    }

    fn name(&self) -> String {
        let st = self.lock();
        self.name_of(&st)
    }

    fn emit(&self) {
        self.sync_input();
        self.sync_low_latency();
        let view = self.view();
        (self.inner.notify)(EngineEvent::State(Box::new(view)));
    }

    /// 低延迟模式：开启了且有设备连着时通知后台服务关闭 AWDL，否则恢复。状态变化时才动作。
    fn sync_low_latency(&self) {
        if !self.inner.input || !flow_platform::low_latency::supported() {
            return;
        }
        let want = {
            let st = self.lock();
            st.config.settings.low_latency && !st.sessions.is_empty()
        };
        let mut cur = self.inner.low_latency_active.lock().expect("low latency");
        if *cur != Some(want) {
            flow_platform::low_latency::set_active(want);
            info!(active = want, "低延迟模式");
            *cur = Some(want);
        }
    }

    /// 开启或关闭低延迟模式。第一次开启时安装后台服务，会弹出系统的管理员授权窗口并阻塞到用户操作完，
    /// 所以不要在主线程或异步任务里直接调用。失败时返回给用户看的原因。
    pub fn set_low_latency(&self, enabled: bool) -> Result<(), String> {
        use flow_platform::low_latency;
        if enabled {
            if !low_latency::supported() {
                return Err("低延迟模式只在 Mac 上需要。".into());
            }
            if !low_latency::installed() {
                low_latency::install().map_err(|e| {
                    format!(
                        "没有开启低延迟模式：{}。关闭 AWDL 需要管理员授权，可以再试一次。",
                        platform_reason(&e)
                    )
                })?;
            }
        }
        let mut st = self.lock();
        st.config.settings.low_latency = enabled;
        self.save(&st);
        drop(st);
        self.emit();
        Ok(())
    }

    /// 关闭低延迟模式并移除后台服务（弹出管理员授权窗口，同样会阻塞）。
    pub fn remove_low_latency(&self) -> Result<(), String> {
        self.set_low_latency(false)?;
        if flow_platform::low_latency::installed() {
            flow_platform::low_latency::uninstall().map_err(|e| {
                format!(
                    "没有移除后台服务：{}。低延迟模式已关闭，不影响使用。",
                    platform_reason(&e)
                )
            })?;
        }
        self.emit();
        Ok(())
    }

    /// 把当前可控制的设备、布局和防误触设置同步给跨屏控制器。
    fn sync_input(&self) {
        let st = self.lock();
        let local = self.local_id().clone();
        let outputs: HashMap<DeviceId, mpsc::UnboundedSender<Message>> = st
            .sessions
            .iter()
            .filter(|(id, _)| st.config.peer(id).is_some_and(|p| !p.paused))
            .map(|(id, s)| (id.clone(), s.tx.clone()))
            .collect();
        let mut layout = st.layout(&local);
        layout
            .displays
            .retain(|d| d.id.device == local || outputs.contains_key(&d.id.device));
        let cfg = ControllerConfig {
            layout,
            local_offset: st.config.local_offset,
            outputs,
            policy: switch_policy(&st.config.settings, st.locked),
            mapping: st.config.settings.mapping,
            enabled: !st.config.settings.sharing_paused,
        };
        self.controller().configure(cfg);
    }

    fn notice(&self, msg: impl Into<String>) {
        (self.inner.notify)(EngineEvent::Notice(msg.into()));
    }

    fn save(&self, st: &State) {
        if let Err(e) = st.config.save(&self.inner.data_dir) {
            self.notice(format!("保存设置失败：{e}"));
        }
    }

    pub fn view(&self) -> StateView {
        let st = self.lock();
        let local = self.local_id().clone();
        let perms = flow_platform::permissions();
        let active = self.controller().active_device().cloned();
        let controlled_by = self
            .inner
            .controlled
            .lock()
            .expect("controlled")
            .as_ref()
            .map(|c| c.by.clone());
        let mut devices = vec![DeviceView {
            id: local.clone(),
            name: self.name_of(&st),
            os: Os::current(),
            status: DeviceStatus::Local,
            manual: false,
            active: active.is_none(),
            paused: false,
        }];
        devices.extend(st.config.peers.iter().map(|p| DeviceView {
            id: p.id.clone(),
            name: p.name.clone(),
            os: p.os,
            status: if st.sessions.contains_key(&p.id) {
                DeviceStatus::Connected
            } else if st.connecting.contains(&p.id) || st.pairings.contains_key(&p.id) {
                DeviceStatus::Connecting
            } else {
                DeviceStatus::Offline
            },
            manual: st.manual.contains(&p.id) && !st.sessions.contains_key(&p.id),
            active: active.as_ref() == Some(&p.id),
            paused: p.paused,
        }));
        let mut nearby: Vec<NearbyView> = st
            .nearby
            .values()
            .filter(|f| !st.config.is_trusted(&f.device))
            .map(|f| NearbyView {
                id: f.device.clone(),
                name: f.name.clone(),
                os: os_from_tag(&f.os),
                connecting: st.connecting.contains(&f.device)
                    || st.pairings.contains_key(&f.device),
            })
            .collect();
        nearby.sort_by(|a, b| a.name.cmp(&b.name));
        let peer_name = |id: &DeviceId| {
            st.config
                .peer(id)
                .map(|p| p.name.clone())
                .unwrap_or_default()
        };
        let (transfers, file_offer) = {
            let files = self.inner.files.lock().expect("files");
            (
                files.transfers.views(peer_name),
                files.book.pending().map(|(id, o)| FileOfferView {
                    peer: id.clone(),
                    peer_name: peer_name(id),
                    label: o.label(),
                    files: o.files,
                    bytes: o.bytes,
                }),
            )
        };
        StateView {
            local: LocalView {
                id: local.clone(),
                name: self.name_of(&st),
                os: Os::current(),
            },
            onboarded: st.config.onboarded,
            port: self.inner.port,
            layout: st.layout(&local),
            devices,
            nearby,
            pairings: st
                .pairings
                .iter()
                .map(|(id, p)| PairingView {
                    id: id.clone(),
                    name: p.name.clone(),
                    os: p.os,
                    code: p.code.to_string(),
                    outgoing: p.outgoing,
                    local_confirmed: p.local,
                    remote_confirmed: p.remote,
                })
                .collect(),
            settings: st.config.settings.clone(),
            locked: st.locked,
            permissions: flow_platform::required_permissions()
                .iter()
                .map(|&p| PermissionView {
                    kind: permission_kind(p),
                    state: match perms.state(p) {
                        PermissionState::Granted => "granted",
                        PermissionState::Denied => "denied",
                        PermissionState::NotDetermined => "not_determined",
                        PermissionState::NotApplicable => "not_applicable",
                    },
                })
                .collect(),
            permission_hint: permission_hint(),
            controlled_by,
            low_latency: LowLatencyView {
                supported: flow_platform::low_latency::supported(),
                installed: flow_platform::low_latency::installed(),
                active: self
                    .inner
                    .low_latency_active
                    .lock()
                    .expect("low latency")
                    .unwrap_or(false),
            },
            about: AboutView {
                version: env!("CARGO_PKG_VERSION"),
                protocol: PROTOCOL_VERSION,
                data_dir: self.inner.data_dir.display().to_string(),
                fingerprint: local
                    .0
                    .as_bytes()
                    .chunks(4)
                    .take(8)
                    .map(|c| String::from_utf8_lossy(c).to_uppercase())
                    .collect::<Vec<_>>()
                    .join(" "),
            },
            transfers,
            file_offer,
            receive_dir: self
                .inner
                .receive_dir
                .as_ref()
                .map(|d| d.display().to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // 界面命令
    // -----------------------------------------------------------------------

    pub fn set_device_name(&self, name: &str) {
        let name = name.trim();
        let new_name = {
            let mut st = self.lock();
            st.config.device_name = (!name.is_empty()).then(|| name.chars().take(40).collect());
            self.save(&st);
            let n = self.name_of(&st);
            for s in st.sessions.values() {
                let _ = s.tx.send(Message::Profile { name: n.clone() });
            }
            n
        };
        if let Some(d) = self.inner.discovery.lock().expect("discovery").as_mut() {
            let _ = d.update_name(&new_name);
        }
        self.emit();
    }

    pub fn finish_onboarding(&self) {
        let mut st = self.lock();
        st.config.onboarded = true;
        self.save(&st);
        drop(st);
        self.emit();
    }

    pub fn update_settings(&self, settings: Settings) {
        let mut st = self.lock();
        st.config.settings = settings;
        self.save(&st);
        drop(st);
        self.emit();
    }

    pub fn settings(&self) -> Settings {
        self.lock().config.settings.clone()
    }

    pub fn locked(&self) -> bool {
        self.lock().locked
    }

    pub fn set_locked(&self, locked: bool) {
        self.lock().locked = locked;
        self.emit();
    }

    pub fn set_device_paused(&self, id: &DeviceId, paused: bool) {
        let mut st = self.lock();
        if let Some(p) = st.config.peer_mut(id) {
            p.paused = paused;
        }
        self.save(&st);
        drop(st);
        self.emit();
    }

    /// 拖动设备：吸附、重叠时推到贴边，校验后保存新位置。
    /// `snap` 为吸附距离（逻辑点），界面按布局图的缩放换算成固定的屏幕像素后传入。
    pub fn move_device(
        &self,
        id: &DeviceId,
        dx: f64,
        dy: f64,
        snap: Option<f64>,
    ) -> Result<(), String> {
        let mut st = self.lock();
        let local = self.local_id().clone();
        let layout = st.layout(&local);
        let threshold = snap.unwrap_or(SNAP_THRESHOLD).clamp(SNAP_THRESHOLD, 600.0);
        // 先取整再吸附：吸附得到的是精确贴边的位移，之后不能再取整，否则可能又压上一点。
        let (sx, sy) = layout
            .place_device(id, dx.round(), dy.round(), threshold)
            .ok_or_else(|| "这里放不下，请把屏幕放在其他屏幕旁边的空位".to_owned())?;
        let mut moved = layout;
        moved.move_device(id, sx, sy);
        moved
            .validate()
            .map_err(|_| "屏幕不能互相重叠，请放在其他屏幕旁边".to_owned())?;
        if id == &local {
            let (x, y) = st.config.local_offset;
            st.config.local_offset = (x + sx, y + sy);
        } else if let Some(p) = st.config.peer_mut(id) {
            let (x, y) = p.offset.unwrap_or((0.0, 0.0));
            p.offset = Some((x + sx, y + sy));
        }
        self.save(&st);
        drop(st);
        self.emit();
        Ok(())
    }

    /// 向附近的一台设备发起配对。
    pub fn pair_with(&self, id: &DeviceId) {
        let addrs = {
            let mut st = self.lock();
            if st.sessions.contains_key(id)
                || st.pairings.contains_key(id)
                || st.connecting.contains(id)
            {
                return;
            }
            let Some(found) = st.nearby.get(id) else {
                drop(st);
                self.notice("找不到这台设备了，请确认它和本机在同一网络中，且流界正在运行。");
                return;
            };
            let addrs = found.addrs.clone();
            st.connecting.insert(id.clone());
            addrs
        };
        self.emit();
        self.inner
            .rt
            .spawn(self.clone().connect_and_run(id.clone(), addrs, true, true));
    }

    /// 手动连接一台已配对的设备（自动重连停止后由用户点击）。
    pub fn connect_device(&self, id: &DeviceId) {
        let addrs = {
            let mut st = self.lock();
            if st.sessions.contains_key(id) || st.connecting.contains(id) {
                return;
            }
            let Some(peer) = st.config.peer(id) else {
                return;
            };
            let addrs = st
                .nearby
                .get(id)
                .map(|f| f.addrs.clone())
                .unwrap_or_else(|| peer.last_addrs.clone());
            let name = peer.name.clone();
            if addrs.is_empty() {
                drop(st);
                self.notice(format!(
                    "找不到「{name}」的网络地址。请确认它和本机在同一局域网，且流界正在运行。"
                ));
                return;
            }
            st.manual.remove(id);
            st.backoff.remove(id);
            st.connecting.insert(id.clone());
            addrs
        };
        self.emit();
        self.inner
            .rt
            .spawn(self.clone().connect_and_run(id.clone(), addrs, false, true));
    }

    pub fn respond_pairing(&self, id: &DeviceId, accept: bool) {
        if let Some(p) = self.lock().pairings.get(id) {
            let _ = p.cmd.send(accept);
        }
    }

    /// 移除已配对设备：双方都会删除对方的可信记录。
    pub fn remove_device(&self, id: &DeviceId) {
        let session = {
            let mut st = self.lock();
            st.config.peers.retain(|p| &p.id != id);
            st.backoff.remove(id);
            st.manual.remove(id);
            self.save(&st);
            st.sessions.remove(id)
        };
        if let Some(s) = session {
            let _ = s.tx.send(Message::Bye {
                reason: ByeReason::Removed,
            });
            self.inner.rt.spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                s.conn.close(0u32.into(), b"removed");
            });
        }
        self.emit();
    }

    // -----------------------------------------------------------------------
    // 后台任务
    // -----------------------------------------------------------------------

    async fn discovery_loop(self, mut rx: mpsc::UnboundedReceiver<DiscoveryEvent>) {
        while let Some(ev) = rx.recv().await {
            {
                let mut st = self.lock();
                match ev {
                    DiscoveryEvent::Found(f) => {
                        debug!(device = %short(&f.device), name = %f.name, addrs = ?f.addrs, "发现设备");
                        // 重新出现的设备立刻重试，不等退避（不重置次数，也不恢复已停止的自动重连）。
                        if let Some(b) = st.backoff.get_mut(&f.device) {
                            b.next = Instant::now();
                        }
                        st.nearby.insert(f.device.clone(), f);
                    }
                    DiscoveryEvent::Lost(id) => {
                        debug!(device = %short(&id), "设备从局域网消失");
                        st.nearby.remove(&id);
                    }
                }
            }
            self.emit();
        }
    }

    async fn accept_loop(self) {
        while let Some(incoming) = self.inner.endpoint.accept().await {
            let engine = self.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        let from = conn.remote_address();
                        debug!(%from, "收到连接");
                        if let Err(e) = engine.run_link(conn, false, false).await {
                            warn!(%from, error = %e, "入站连接握手失败");
                        }
                    }
                    Err(e) => warn!(error = %e, "入站连接建立失败"),
                }
            });
        }
    }

    /// 定时：检查显示器变化、为离线的已配对设备重连。
    async fn maintenance_loop(self) {
        let mut tick: u64 = 0;
        loop {
            tokio::time::sleep(TICK).await;
            tick += 1;
            if tick % 3 == 0 {
                self.refresh_displays();
            }
            self.ensure_capture();
            let targets = {
                let mut st = self.lock();
                let now = Instant::now();
                let me = self.local_id().clone();
                let mut out = Vec::new();
                let mut gave_up = Vec::new();
                let candidates: Vec<(DeviceId, Vec<SocketAddr>)> = st
                    .config
                    .peers
                    .iter()
                    .filter(|p| {
                        !st.sessions.contains_key(&p.id)
                            && !st.connecting.contains(&p.id)
                            && !st.pairings.contains_key(&p.id)
                            && !st.manual.contains(&p.id)
                    })
                    .map(|p| {
                        let addrs = st
                            .nearby
                            .get(&p.id)
                            .map(|f| f.addrs.clone())
                            .unwrap_or_else(|| p.last_addrs.clone());
                        (p.id.clone(), addrs)
                    })
                    .filter(|(_, a)| !a.is_empty())
                    .collect();
                for (id, addrs) in candidates {
                    // ID 较小的一方先连；较大的一方稍后才尝试，避免两边同时连接。
                    let b = st.backoff.entry(id.clone()).or_insert_with(|| Backoff {
                        next: if me < id {
                            now
                        } else {
                            now + Duration::from_secs(6)
                        },
                        delay: Duration::from_secs(2),
                        attempts: 0,
                    });
                    if b.next > now {
                        continue;
                    }
                    if b.attempts >= MAX_AUTO_ATTEMPTS {
                        st.backoff.remove(&id);
                        st.manual.insert(id.clone());
                        gave_up.push(id);
                        continue;
                    }
                    b.attempts += 1;
                    b.next = now + b.delay;
                    b.delay = (b.delay * 2).min(MAX_BACKOFF);
                    st.connecting.insert(id.clone());
                    out.push((id, addrs));
                }
                let names: Vec<String> = gave_up
                    .iter()
                    .filter_map(|id| st.config.peer(id).map(|p| p.name.clone()))
                    .collect();
                (out, names)
            };
            let (targets, gave_up) = targets;
            for name in &gave_up {
                info!(%name, "自动重连次数用完，停止自动连接");
                self.notice(format!(
                    "多次连接「{name}」都没有成功，已停止自动连接。对方的流界打开后会自动连过来，也可以在设备详情里点「连接」。"
                ));
            }
            if !targets.is_empty() || !gave_up.is_empty() {
                self.emit();
            }
            for (id, addrs) in targets {
                tokio::spawn(self.clone().connect_and_run(id, addrs, false, false));
            }
        }
    }

    fn refresh_displays(&self) {
        // 显示器名称要在主线程读取；这里先安排刷新缓存，新名称在下一轮生效。
        if let Some(run) = &self.inner.main_thread {
            run(Box::new(flow_platform::refresh_display_names));
        }
        let Ok(now) = flow_platform::display_provider().displays() else {
            return;
        };
        let changed = {
            let mut st = self.lock();
            if st.local_displays == now {
                false
            } else {
                st.local_displays = now.clone();
                for s in st.sessions.values() {
                    let _ = s.tx.send(Message::Displays(now.clone()));
                }
                true
            }
        };
        if changed {
            self.emit();
        }
    }

    // -----------------------------------------------------------------------
    // 连接：握手 → 配对或会话
    // -----------------------------------------------------------------------

    /// `user`：用户点击发起（配对或手动连接），失败时提示原因。
    async fn connect_and_run(self, peer: DeviceId, addrs: Vec<SocketAddr>, pair: bool, user: bool) {
        debug!(device = %short(&peer), ?addrs, pair, user, "发起连接");
        let result = self.try_connect(&peer, &addrs, pair).await;
        self.lock().connecting.remove(&peer);
        if let Err(e) = &result {
            warn!(device = %short(&peer), error = %e, pair, "连接失败");
        }
        if let Err(e) = result
            && user
        {
            let name = {
                let mut st = self.lock();
                if !pair {
                    // 手动连接失败：仍然停在"需要手动连接"，不恢复自动重连。
                    st.manual.insert(peer.clone());
                }
                st.config
                    .peer(&peer)
                    .map(|p| p.name.clone())
                    .or_else(|| st.nearby.get(&peer).map(|f| f.name.clone()))
                    .unwrap_or_default()
            };
            self.notice(format!(
                "无法连接到「{name}」：{e}。请确认两台电脑在同一局域网，且对方的流界正在运行。"
            ));
        }
        self.emit();
    }

    async fn try_connect(
        &self,
        peer: &DeviceId,
        addrs: &[SocketAddr],
        pair: bool,
    ) -> Result<(), String> {
        let mut last_err = "没有可用的网络地址".to_owned();
        for addr in addrs {
            let connecting = match self.inner.endpoint.connect(*addr, "flowacross.local") {
                Ok(c) => c,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };
            match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
                Ok(Ok(conn)) => {
                    // mDNS 广播可以被伪造，必须核对证书指纹就是我们要连的那台设备。
                    match peer_fingerprint(&conn) {
                        Ok(fp) if &device_id(&fp) == peer => {
                            return self.run_link(conn, true, pair).await;
                        }
                        _ => {
                            conn.close(0u32.into(), b"unexpected peer");
                            last_err = "对方的身份与发现的设备不一致".to_owned();
                        }
                    }
                }
                Ok(Err(e)) => last_err = format!("连接失败（{e}）"),
                Err(_) => last_err = "连接超时".to_owned(),
            }
        }
        Err(last_err)
    }

    fn hello(&self, trusts_you: bool) -> Message {
        Message::Hello {
            protocol: PROTOCOL_VERSION,
            device: self.local_id().clone(),
            name: self.name(),
            os: Os::current(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            trusts_you,
        }
    }

    /// 握手。只有握手阶段的失败返回 `Err`；进入配对或会话后，结束时都返回 `Ok`。
    async fn run_link(
        &self,
        conn: quinn::Connection,
        initiator: bool,
        pair: bool,
    ) -> Result<(), String> {
        let fp = peer_fingerprint(&conn).map_err(|e| e.to_string())?;
        let peer = device_id(&fp);
        if &peer == self.local_id() {
            conn.close(0u32.into(), b"self");
            return Err("不能连接本机".into());
        }
        let we_trust = self.lock().config.is_trusted(&peer);
        let hello = self.hello(we_trust);

        let (mut link, first) = if initiator {
            let mut link = tokio::time::timeout(HANDSHAKE_TIMEOUT, Link::open(conn, &hello))
                .await
                .map_err(timed_out("握手超时"))?
                .map_err(|e| e.to_string())?;
            let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, link.receiver.recv())
                .await
                .map_err(timed_out("等待对方响应超时"))?
                .map_err(|e| e.to_string())?;
            (link, first)
        } else {
            let mut link = tokio::time::timeout(HANDSHAKE_TIMEOUT, Link::accept(conn))
                .await
                .map_err(timed_out("握手超时"))?
                .map_err(|e| e.to_string())?;
            let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, link.receiver.recv())
                .await
                .map_err(timed_out("等待对方响应超时"))?
                .map_err(|e| e.to_string())?;
            link.sender.send(&hello).await.map_err(|e| e.to_string())?;
            (link, first)
        };

        let Some(Message::Hello {
            protocol,
            device,
            name,
            os,
            app_version,
            trusts_you,
        }) = first
        else {
            link.conn.close(0u32.into(), b"bad hello");
            return Err("对方不是流界设备".into());
        };
        if device != peer {
            link.conn.close(0u32.into(), b"identity mismatch");
            return Err("设备身份校验失败".into());
        }
        if protocol != PROTOCOL_VERSION {
            let _ = link
                .sender
                .send(&Message::Bye {
                    reason: ByeReason::VersionMismatch,
                })
                .await;
            linger_close(&link.conn, b"version").await;
            self.notice(format!(
                "「{name}」的流界版本与本机不兼容，请把两台电脑都升级到最新版本。"
            ));
            return Ok(());
        }
        self.lock().connecting.remove(&peer);

        info!(device = %short(&peer), %name, version = %app_version, we_trust, trusts_you, initiator, "握手完成");
        let peer_info = PeerInfo {
            id: peer,
            name,
            os,
            version: app_version,
        };
        let name = peer_info.name.clone();
        if we_trust && trusts_you {
            self.run_session(link, peer_info, initiator).await;
        } else if initiator && pair {
            if link.sender.send(&Message::PairRequest).await.is_ok() {
                self.run_pairing(link, peer_info, true).await;
            }
        } else if initiator {
            // 自动重连时发现对方已不信任本机。
            let _ = link
                .sender
                .send(&Message::Bye {
                    reason: ByeReason::Unpaired,
                })
                .await;
            linger_close(&link.conn, b"unpaired").await;
            if we_trust {
                self.notice(format!("「{name}」已不再信任本机，需要重新配对。"));
            }
        } else {
            match tokio::time::timeout(Duration::from_secs(30), link.receiver.recv()).await {
                Ok(Ok(Some(Message::PairRequest))) => {
                    self.run_pairing(link, peer_info, false).await
                }
                Ok(Ok(Some(Message::Bye {
                    reason: ByeReason::Unpaired,
                }))) if we_trust => {
                    self.notice(format!("「{name}」已不再信任本机，需要重新配对。"));
                }
                _ => link.conn.close(0u32.into(), b"no pairing"),
            }
        }
        Ok(())
    }

    async fn run_pairing(&self, mut link: Link, info: PeerInfo, outgoing: bool) {
        let PeerInfo {
            id: peer, name, os, ..
        } = info.clone();
        let code = pairing_code(&self.inner.identity.fingerprint, &link.peer_fingerprint);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        {
            let mut st = self.lock();
            if st.pairings.contains_key(&peer) || st.sessions.contains_key(&peer) {
                drop(st);
                link.conn.close(0u32.into(), b"busy");
                return;
            }
            st.pairings.insert(
                peer.clone(),
                Pairing {
                    name: name.clone(),
                    os,
                    code,
                    outgoing,
                    local: false,
                    remote: false,
                    cmd: cmd_tx,
                },
            );
        }
        self.emit();

        let deadline = tokio::time::sleep(PAIRING_TIMEOUT);
        tokio::pin!(deadline);
        let outcome: Result<(), String> = loop {
            tokio::select! {
                _ = &mut deadline => break Err("配对超时，请重新发起。".into()),
                cmd = cmd_rx.recv() => match cmd {
                    Some(true) => {
                        let first_confirm = match self.lock().pairings.get_mut(&peer) {
                            Some(p) if !p.local => { p.local = true; true }
                            _ => false,
                        };
                        if first_confirm && link.sender.send(&Message::PairConfirm).await.is_err() {
                            break Err("配对过程中连接断开了，请重试。".into());
                        }
                    }
                    _ => {
                        let _ = link.sender.send(&Message::PairReject).await;
                        break Err(String::new());
                    }
                },
                msg = link.receiver.recv() => match msg {
                    Ok(Some(Message::PairConfirm)) => {
                        if let Some(p) = self.lock().pairings.get_mut(&peer) {
                            p.remote = true;
                        }
                    }
                    Ok(Some(Message::PairReject)) => break Err(format!("「{name}」拒绝了配对。")),
                    Ok(Some(_)) => {}
                    _ => break Err("配对过程中连接断开了，请重试。".into()),
                },
            }
            if self
                .lock()
                .pairings
                .get(&peer)
                .is_some_and(|p| p.local && p.remote)
            {
                break Ok(());
            }
            self.emit();
        };
        self.lock().pairings.remove(&peer);

        match outcome {
            Ok(()) => {
                {
                    let mut st = self.lock();
                    match st.config.peer_mut(&peer) {
                        Some(r) => {
                            r.name = name.clone();
                            r.os = os;
                        }
                        None => st.config.peers.push(PeerRecord {
                            id: peer.clone(),
                            name: name.clone(),
                            os,
                            displays: Vec::new(),
                            offset: None,
                            last_addrs: Vec::new(),
                            paused: false,
                        }),
                    }
                    self.save(&st);
                }
                info!(device = %short(&peer), %name, "配对成功");
                self.notice(format!("已与「{name}」配对，之后两台电脑会自动连接。"));
                self.run_session(link, info, outgoing).await;
            }
            Err(msg) => {
                info!(device = %short(&peer), reason = %msg, "配对结束（未成功）");
                linger_close(&link.conn, b"pairing ended").await;
                if !msg.is_empty() {
                    self.notice(msg);
                }
                self.emit();
            }
        }
    }

    async fn run_session(&self, link: Link, info: PeerInfo, initiator: bool) {
        let PeerInfo {
            id: peer,
            name,
            os,
            version,
        } = info;
        let Link {
            conn,
            mut sender,
            mut receiver,
            ..
        } = link;
        // 对方支持时，鼠标位置走数据报（网络停顿后直接跳到最新位置），其余消息走可靠流。
        let mut moves = MoveSender::new(conn.clone(), &version);
        info!(device = %short(&peer), datagram = moves.enabled(), "鼠标位置发送方式");
        // 单元测试里两端都是当前（未发布的）版本号，直接视为支持。
        let files = peer_supports_files(&version) || cfg!(test);
        info!(device = %short(&peer), version = %version, files, "对方版本");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let initiator_id = if initiator {
            self.local_id().clone()
        } else {
            peer.clone()
        };
        {
            let mut st = self.lock();
            if let Some(existing) = st.sessions.get(&peer) {
                if existing.initiator < initiator_id {
                    drop(st);
                    conn.close(0u32.into(), b"duplicate");
                    return;
                }
                existing.conn.close(0u32.into(), b"replaced");
            }
            info!(device = %short(&peer), remote = %conn.remote_address(), "会话建立");
            st.sessions.insert(
                peer.clone(),
                Session {
                    tx: tx.clone(),
                    conn: conn.clone(),
                    initiator: initiator_id,
                    files,
                },
            );
            st.backoff.remove(&peer);
            st.manual.remove(&peer);
            if let Some(r) = st.config.peer_mut(&peer) {
                r.name = name;
                r.os = os;
                if initiator {
                    r.last_addrs = vec![conn.remote_address()];
                }
            }
            self.save(&st);
            let _ = tx.send(Message::Displays(st.local_displays.clone()));
            // 连上之前本机已经复制了文件：补发清单。
            if files && st.files_allowed(&peer) {
                let local = self.inner.files.lock().expect("files").local.clone();
                if let Some((offer, _)) = local {
                    let _ = tx.send(Message::FileOffer(offer));
                }
            }
        }
        self.emit();
        self.ensure_capture();

        // 对方新开的流：取本机复制的文件。
        let streams = {
            let engine = self.clone();
            let conn = conn.clone();
            let peer = peer.clone();
            tokio::spawn(async move {
                while let Ok((tx, rx)) = flow_net::link::accept_stream(&conn).await {
                    tokio::spawn(engine.clone().serve_files(peer.clone(), tx, rx));
                }
            })
        };

        // 发送端节流鼠标位置：高回报率鼠标每秒上千个事件，全发出去会让网络排队、远端注入跟不上。
        let writer = tokio::spawn(async move {
            let epoch = Instant::now();
            let now = || epoch.elapsed().as_millis() as u64;
            let mut throttle = MoveThrottle::new(MOUSE_SEND_INTERVAL_MS);
            loop {
                let out = match throttle.deadline() {
                    Some(due) => {
                        let wait = Duration::from_millis(due.saturating_sub(now()));
                        tokio::select! {
                            m = rx.recv() => match m {
                                Some(m) => throttle.push(m, now()),
                                None => break,
                            },
                            _ = tokio::time::sleep(wait) => throttle.flush(now()).into_iter().collect(),
                        }
                    }
                    None => match rx.recv().await {
                        Some(m) => throttle.push(m, now()),
                        None => break,
                    },
                };
                for m in out {
                    if !moves.try_send(&m) && sender.send(&m).await.is_err() {
                        return;
                    }
                }
            }
        });

        // 数据报里的鼠标位置：每批只处理最新的一个。
        let datagrams = {
            let engine = self.clone();
            let peer = peer.clone();
            let mut rx = MoveReceiver::new(conn.clone());
            tokio::spawn(async move {
                while let Ok(m) = rx.recv_latest().await {
                    if let Some(m) = m {
                        engine.on_remote_input(&peer, os, m);
                    }
                }
            })
        };

        loop {
            match receiver.recv().await {
                Ok(Some(Message::Displays(list))) => {
                    let mut st = self.lock();
                    if let Some(r) = st.config.peer_mut(&peer) {
                        r.displays = list;
                    }
                    let local = self.local_id().clone();
                    st.place_new_peer(&local, &peer);
                    self.save(&st);
                    drop(st);
                    self.emit();
                }
                Ok(Some(Message::Profile { name })) => {
                    let mut st = self.lock();
                    if let Some(r) = st.config.peer_mut(&peer) {
                        r.name = name;
                    }
                    self.save(&st);
                    drop(st);
                    self.emit();
                }
                Ok(Some(Message::Bye { reason })) => {
                    if reason == ByeReason::Removed {
                        let removed = {
                            let mut st = self.lock();
                            let name = st.config.peer(&peer).map(|p| p.name.clone());
                            st.config.peers.retain(|p| p.id != peer);
                            self.save(&st);
                            name
                        };
                        if let Some(n) = removed {
                            self.notice(format!("「{n}」移除了与本机的配对。"));
                        }
                    }
                    break;
                }
                Ok(Some(
                    m @ (Message::Enter { .. }
                    | Message::Leave { .. }
                    | Message::MouseMove { .. }
                    | Message::Button { .. }
                    | Message::Wheel { .. }
                    | Message::Key { .. }
                    | Message::ReleaseAll
                    | Message::Heartbeat { .. }),
                )) => self.on_remote_input(&peer, os, m),
                Ok(Some(Message::Clipboard { origin, seq, text })) => {
                    self.on_remote_clipboard(&peer, &origin, seq, &text)
                }
                Ok(Some(Message::FileOffer(offer))) => self.on_file_offer(&peer, offer),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }

        writer.abort();
        datagrams.abort();
        streams.abort();
        self.release_controlled(Some(&peer));
        info!(device = %short(&peer), reason = ?conn.close_reason(), "会话结束");
        let current = {
            let mut st = self.lock();
            let current = st
                .sessions
                .get(&peer)
                .is_some_and(|s| s.conn.stable_id() == conn.stable_id());
            if current {
                st.sessions.remove(&peer);
            }
            current
        };
        if current {
            // 断开后取不到它的文件了。正在进行的传输会因为连接关闭自行失败。
            self.inner
                .files
                .lock()
                .expect("files")
                .book
                .clear_from(&peer);
        }
        conn.close(0u32.into(), b"bye");
        self.emit();
    }

    // -----------------------------------------------------------------------
    // 跨屏：控制方
    // -----------------------------------------------------------------------

    /// 有已连接的设备时启动键鼠捕获；失败（通常是缺少权限）只提示一次，之后定时重试。
    fn ensure_capture(&self) {
        if !self.inner.input || self.lock().sessions.is_empty() {
            return;
        }
        let mut cap = self.inner.capture.lock().expect("capture");
        if cap.is_some() {
            return;
        }
        let mut retry_at = self.inner.capture_retry_at.lock().expect("retry");
        if retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let ctl = self.inner.controller.clone();
        let handler: flow_platform::CaptureHandler = Box::new(move |ev| match ctl.lock() {
            Ok(mut c) => c.handle(ev),
            Err(_) => {
                // 控制器出错：至少保证本机键鼠可用。
                flow_platform::set_cursor_captured(false, None);
                flow_platform::Verdict::Pass
            }
        });
        let mut capture = flow_platform::input_capture();
        match capture.start(handler) {
            Ok(()) => {
                info!("键鼠捕获已启动");
                self.inner.capture_warned.store(false, Ordering::Relaxed);
                *retry_at = None;
                *cap = Some(capture);
            }
            Err(e) => {
                // 缺权限只是本进程内的查询，下个周期再查即可；系统接口本身失败则 30 秒后再试。
                if !matches!(e, PlatformError::PermissionDenied(_)) {
                    *retry_at = Some(Instant::now() + Duration::from_secs(30));
                }
                if !self.inner.capture_warned.swap(true, Ordering::Relaxed) {
                    warn!(error = %e, "无法启动键鼠捕获");
                    drop(retry_at);
                    drop(cap);
                    self.notice(capture_error_text(&e));
                }
            }
        }
    }

    async fn control_event_loop(self, mut rx: mpsc::UnboundedReceiver<ControlEvent>) {
        let mut was_remote = false;
        while let Some(ev) = rx.recv().await {
            if let ControlEvent::Lost(id) = &ev {
                let name = self
                    .lock()
                    .config
                    .peer(id)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                self.notice(format!("与「{name}」的连接中断，键鼠已回到本机。"));
            }
            let remote = self.controller().active_device().is_some();
            if was_remote && !remote {
                // 控制远端后回到本机：在那边复制的文件可能要在这里粘贴。
                self.on_arrive();
            }
            was_remote = remote;
            self.emit();
        }
    }

    /// 定时：控制方发心跳、处理贴边停留；被控制方检查对方是否超时。
    async fn input_loop(self) {
        loop {
            tokio::time::sleep(INPUT_TICK).await;
            self.controller().tick();
            let stale = self
                .inner
                .controlled
                .lock()
                .expect("controlled")
                .as_ref()
                .filter(|c| c.last_seen.elapsed() > CONTROL_TIMEOUT)
                .map(|c| c.by.clone());
            if let Some(by) = stale {
                warn!(device = %short(&by), "控制方超时，释放按键");
                self.release_controlled(Some(&by));
                let name = self
                    .lock()
                    .config
                    .peer(&by)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                self.notice(format!(
                    "「{name}」长时间没有响应，已释放它按下的所有按键。"
                ));
            }
        }
    }

    // -----------------------------------------------------------------------
    // 剪贴板
    // -----------------------------------------------------------------------

    /// 可以收发剪贴板的设备：已连接且没有被暂停。第二项表示可以发文件清单给它。
    fn clipboard_peers(&self) -> Option<Vec<(mpsc::UnboundedSender<Message>, bool)>> {
        let st = self.lock();
        if !st.config.settings.clipboard || st.config.settings.sharing_paused {
            return None;
        }
        let peers: Vec<_> = st
            .sessions
            .iter()
            .filter(|(id, _)| st.config.peer(id).is_some_and(|p| !p.paused))
            .map(|(id, s)| (s.tx.clone(), s.files && st.files_allowed(id)))
            .collect();
        (!peers.is_empty()).then_some(peers)
    }

    /// 定时检查本机剪贴板：新的文本发给其他设备；复制了文件则发文件清单。
    async fn clipboard_loop(self) {
        if !self.inner.input {
            return;
        }
        loop {
            tokio::time::sleep(CLIPBOARD_POLL).await;
            let peers = self.clipboard_peers();
            let (epoch, withdrawn, files, msg, peers) = {
                let mut cb = self.inner.clipboard.lock().expect("clipboard");
                cb.sync.enabled = peers.is_some();
                if cb.access.is_none() {
                    match flow_platform::clipboard() {
                        Ok(a) => cb.access = Some(a),
                        Err(e) => {
                            warn!(error = %e, "无法访问剪贴板");
                            return;
                        }
                    }
                }
                let Some(access) = cb.access.as_mut() else {
                    continue;
                };
                let count = access.change_count();
                let first = cb.last_count.is_none();
                if cb.last_count == Some(count) {
                    continue;
                }
                cb.last_count = Some(count);
                if first {
                    continue;
                }
                // 本机剪贴板换了内容：收到的文件清单作废，正在接收的文件收完后也不再写入剪贴板。
                let (epoch, withdrawn) = {
                    let mut f = self.inner.files.lock().expect("files");
                    f.epoch += 1;
                    f.book.clear();
                    (f.epoch, f.local.take().map(|(o, _)| o.id))
                };
                let Some(peers) = peers else {
                    continue;
                };
                let Some(access) = cb.access.as_mut() else {
                    continue;
                };
                let files = if peers.iter().any(|(_, f)| *f) {
                    match access.get_files() {
                        Ok(f) => f,
                        Err(e) => {
                            debug!(error = %e, "读取剪贴板里的文件失败");
                            None
                        }
                    }
                } else {
                    None
                };
                let text = match access.get_text() {
                    Ok(t) => t,
                    Err(e) => {
                        debug!(error = %e, "读取剪贴板失败");
                        None
                    }
                };
                let msg = text.and_then(|t| cb.sync.on_local_change(&t));
                (epoch, withdrawn, files, msg, peers)
            };
            if let Some(id) = withdrawn {
                for (tx, _) in peers.iter().filter(|(_, f)| *f) {
                    let _ = tx.send(Message::FileOffer(FileOffer::withdrawn(id)));
                }
            }
            match files {
                Some(paths) => {
                    // 复制了文件：支持的设备收清单；旧版本照旧收文字（Finder 会附带文件名）。
                    if let Some(msg) = msg {
                        for (tx, _) in peers.iter().filter(|(_, f)| !*f) {
                            let _ = tx.send(msg.clone());
                        }
                    }
                    tokio::spawn(self.clone().offer_files(paths, epoch));
                }
                None => {
                    if let Some(msg) = msg {
                        debug!(peers = peers.len(), "发送剪贴板");
                        for (tx, _) in peers {
                            let _ = tx.send(msg.clone());
                        }
                    }
                }
            }
        }
    }

    fn on_remote_clipboard(&self, peer: &DeviceId, origin: &DeviceId, seq: u64, text: &str) {
        if !self.inner.input {
            return;
        }
        let allowed = {
            let st = self.lock();
            st.config.settings.clipboard
                && !st.config.settings.sharing_paused
                && st.config.peer(peer).is_some_and(|p| !p.paused)
        };
        if !allowed {
            return;
        }
        let mut cb = self.inner.clipboard.lock().expect("clipboard");
        cb.sync.enabled = true;
        let Some(text) = cb.sync.on_remote(origin, seq, text) else {
            return;
        };
        if cb.access.is_none() {
            cb.access = flow_platform::clipboard().ok();
        }
        let Some(access) = cb.access.as_mut() else {
            return;
        };
        match access.set_text(&text) {
            Ok(()) => {
                // 记下写入后的计数，轮询时不会把它当成本机的新内容再发回去。
                let count = access.change_count();
                cb.last_count = Some(count);
                // 剪贴板现在是这段文字：之前收到的文件清单作废。
                let mut f = self.inner.files.lock().expect("files");
                f.epoch += 1;
                f.book.clear();
                f.local = None;
                debug!(device = %short(peer), "已写入远端剪贴板内容");
            }
            Err(e) => warn!(error = %e, "写入剪贴板失败"),
        }
    }

    // -----------------------------------------------------------------------
    // 跨设备复制文件
    // -----------------------------------------------------------------------

    fn now_ms(&self) -> u64 {
        self.inner.started.elapsed().as_millis() as u64
    }

    /// 传输进度变化：最多每 [`PROGRESS_EMIT`] 推送一次。
    fn progress(&self) {
        let due = {
            let mut last = self.inner.progress_emitted.lock().expect("progress");
            let due = last.elapsed() >= PROGRESS_EMIT;
            if due {
                *last = Instant::now();
            }
            due
        };
        if due {
            self.emit();
        }
    }

    /// 把本机这些文件作为当前复制的内容提供给其他设备：整理清单并发出去。
    /// `epoch` 是发现时剪贴板的代，整理期间剪贴板又变了就放弃。
    pub(crate) async fn offer_files(self, paths: Vec<PathBuf>, epoch: u64) {
        let roots = paths.clone();
        let summary = tokio::task::spawn_blocking(move || {
            transfer::walk(&roots).map(|entries| transfer::summarize(&roots, &entries))
        })
        .await;
        let summary = match summary {
            Ok(Ok(s)) if s.items > 0 => s,
            Ok(Ok(_)) | Err(_) => return,
            Ok(Err(e)) => {
                warn!(error = %e, "整理复制的文件失败");
                self.notice(format!("复制的文件无法共享给其他电脑：{e}"));
                return;
            }
        };
        let offer = {
            let mut f = self.inner.files.lock().expect("files");
            if f.epoch != epoch {
                return;
            }
            f.next_offer += 1;
            let offer = FileOffer {
                id: f.next_offer,
                ..summary
            };
            f.local = Some((offer.clone(), paths));
            offer
        };
        let peers: Vec<_> = self
            .clipboard_peers()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, f)| *f)
            .collect();
        info!(
            files = offer.files,
            bytes = offer.bytes,
            peers = peers.len(),
            "复制了文件，已通知其他设备"
        );
        for (tx, _) in peers {
            let _ = tx.send(Message::FileOffer(offer.clone()));
        }
    }

    /// 本机当前剪贴板的代（测试中模拟"复制了文件"时用）。
    #[cfg(test)]
    fn clipboard_epoch(&self) -> u64 {
        self.inner.files.lock().expect("files").epoch
    }

    fn on_file_offer(&self, peer: &DeviceId, offer: FileOffer) {
        if !self.lock().files_allowed(peer) {
            return;
        }
        let controlled = self.inner.controlled.lock().expect("controlled").is_some();
        debug!(device = %short(peer), items = offer.items, bytes = offer.bytes, "收到文件清单");
        let fetch = self.inner.files.lock().expect("files").book.on_offer(
            peer.clone(),
            offer,
            controlled,
            self.now_ms(),
        );
        if let Some((from, offer)) = fetch {
            self.start_fetch(from, offer);
        }
        self.emit();
    }

    /// 键鼠来到本机（开始被控制，或控制远端后回到本机）：有其他设备刚复制的文件就去取。
    fn on_arrive(&self) {
        let fetch = self
            .inner
            .files
            .lock()
            .expect("files")
            .book
            .on_arrive(self.now_ms());
        if let Some((from, offer)) = fetch
            && self.lock().files_allowed(&from)
        {
            self.start_fetch(from, offer);
        }
    }

    /// 用户点了「接收」：取等待中的文件。
    pub fn accept_files(&self) {
        let pending = self.inner.files.lock().expect("files").book.take();
        if let Some((from, offer)) = pending {
            self.start_fetch(from, offer);
        }
        self.emit();
    }

    pub fn cancel_transfer(&self, id: u64) {
        self.inner.files.lock().expect("files").transfers.cancel(id);
    }

    pub fn dismiss_transfer(&self, id: u64) {
        self.inner
            .files
            .lock()
            .expect("files")
            .transfers
            .dismiss(id);
        self.emit();
    }

    pub fn receive_dir(&self) -> Option<&std::path::Path> {
        self.inner.receive_dir.as_deref()
    }

    fn start_fetch(&self, peer: DeviceId, offer: FileOffer) {
        let (conn, name) = {
            let st = self.lock();
            let conn = st
                .sessions
                .get(&peer)
                .filter(|s| s.files)
                .map(|s| s.conn.clone());
            let name = st
                .config
                .peer(&peer)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            (conn, name)
        };
        let Some(conn) = conn else {
            self.notice(format!(
                "「{name}」已断开，无法接收它复制的文件。重新连接后请再复制一次。"
            ));
            return;
        };
        let Some(root) = self.inner.receive_dir.clone() else {
            self.notice("找不到本机的下载文件夹，无法接收文件。");
            return;
        };
        let (id, ticket, epoch) = {
            let mut f = self.inner.files.lock().expect("files");
            // 剪贴板只有一份：新的文件取代还没收完的旧文件。
            f.transfers.cancel_incoming();
            let (id, ticket) = f
                .transfers
                .start(peer.clone(), true, offer.label(), offer.bytes);
            (id, ticket, f.epoch)
        };
        info!(device = %short(&peer), files = offer.files, bytes = offer.bytes, "开始接收文件");
        self.emit();
        let engine = self.clone();
        self.inner.rt.spawn(async move {
            let result = async {
                let (mut tx, mut rx) = flow_net::link::open_stream(&conn)
                    .await
                    .map_err(|e| format!("无法开始传输：{e}"))?;
                transfer::receive(&mut tx, &mut rx, offer.id, &root, &ticket, || {
                    engine.progress()
                })
                .await
            }
            .await;
            engine.finish_fetch(id, &name, &offer, epoch, result);
        });
    }

    fn finish_fetch(
        &self,
        id: u64,
        name: &str,
        offer: &FileOffer,
        epoch: u64,
        result: Result<Vec<PathBuf>, String>,
    ) {
        let label = offer.label();
        match result {
            Ok(paths) => {
                let pasted = self.paste_files(&paths, epoch);
                info!(files = paths.len(), pasted, "文件接收完成");
                self.inner
                    .files
                    .lock()
                    .expect("files")
                    .transfers
                    .finish(id, Ok(pasted));
                self.notice(if pasted {
                    format!("已收到「{name}」的「{label}」，可以直接粘贴。")
                } else {
                    format!("已收到「{name}」的「{label}」，保存在下载文件夹的「流界」里。")
                });
            }
            Err(e) => {
                warn!(error = %e, "接收文件失败");
                if e != transfer::CANCELED {
                    self.notice(format!("接收「{name}」的「{label}」失败：{e}"));
                }
                self.inner
                    .files
                    .lock()
                    .expect("files")
                    .transfers
                    .finish(id, Err(e));
            }
        }
        self.emit();
    }

    /// 把收到的文件写入本机剪贴板。期间用户复制了别的内容就不覆盖。返回是否写入。
    fn paste_files(&self, paths: &[PathBuf], epoch: u64) -> bool {
        if !self.inner.input || paths.is_empty() {
            return false;
        }
        let mut cb = self.inner.clipboard.lock().expect("clipboard");
        if self.inner.files.lock().expect("files").epoch != epoch {
            return false;
        }
        if cb.access.is_none() {
            cb.access = flow_platform::clipboard().ok();
        }
        let Some(access) = cb.access.as_mut() else {
            return false;
        };
        match access.set_files(paths) {
            Ok(()) => {
                // 记下写入后的计数，轮询时不会把收到的文件当成本机复制的再发出去。
                let count = access.change_count();
                cb.last_count = Some(count);
                true
            }
            Err(e) => {
                warn!(error = %e, "把文件写入剪贴板失败");
                false
            }
        }
    }

    /// 对方来取本机复制的文件。
    async fn serve_files(
        self,
        peer: DeviceId,
        mut tx: flow_net::LinkSender,
        mut rx: flow_net::LinkReceiver,
    ) {
        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, rx.recv()).await;
        let Ok(Ok(Some(Message::FileRequest { id }))) = first else {
            tx.reset();
            return;
        };
        let allowed = self.lock().files_allowed(&peer);
        let local = self
            .inner
            .files
            .lock()
            .expect("files")
            .local
            .clone()
            .filter(|(o, _)| o.id == id && allowed);
        let Some((offer, paths)) = local else {
            let _ = tx
                .send(&Message::FileError {
                    reason: "复制的内容已经变了，请在那台电脑上重新复制文件。".into(),
                })
                .await;
            tx.finish();
            return;
        };
        let (tid, ticket) = self.inner.files.lock().expect("files").transfers.start(
            peer.clone(),
            false,
            offer.label(),
            offer.bytes,
        );
        info!(device = %short(&peer), files = offer.files, bytes = offer.bytes, "开始发送文件");
        self.emit();
        let result = transfer::send(&mut tx, paths, &ticket, || self.progress()).await;
        match &result {
            Ok(()) => info!(device = %short(&peer), "文件发送完成"),
            Err(e) if e == transfer::CANCELED => tx.reset(),
            Err(e) => {
                warn!(device = %short(&peer), error = %e, "发送文件失败");
                let _ = tx.send(&Message::FileError { reason: e.clone() }).await;
                tx.finish();
            }
        }
        self.inner
            .files
            .lock()
            .expect("files")
            .transfers
            .finish(tid, result.map(|()| false));
        self.emit();
    }

    // -----------------------------------------------------------------------
    // 跨屏：被控制方
    // -----------------------------------------------------------------------

    fn inject(&self, f: impl FnOnce(&mut dyn InputInjector) -> Result<(), PlatformError>) {
        if !self.inner.input {
            return;
        }
        let mut guard = self.inner.injector.lock().expect("injector");
        if guard.is_none() {
            match flow_platform::input_injector() {
                Ok(i) => *guard = Some(i),
                Err(e) => {
                    warn!(error = %e, "无法创建键鼠注入");
                    return;
                }
            }
        }
        if let Some(inj) = guard.as_mut()
            && let Err(e) = f(inj.as_mut())
        {
            debug!(error = %e, "键鼠注入失败");
        }
    }

    fn on_remote_input(&self, peer: &DeviceId, peer_os: Os, msg: Message) {
        if let Message::Enter {
            session,
            display,
            x,
            y,
        } = msg
        {
            self.on_enter(peer, peer_os, session, &display, x, y);
            return;
        }
        // 只接受当前控制本机的设备发来的输入，并记录它按下的键。
        let action = {
            let mut guard = self.inner.controlled.lock().expect("controlled");
            let Some(c) = guard.as_mut().filter(|c| &c.by == peer) else {
                return;
            };
            c.last_seen = Instant::now();
            match msg {
                Message::MouseMove {
                    session,
                    display,
                    x,
                    y,
                } if session == c.session => Some(Remote::Move(display, x, y)),
                Message::Button { button, down } => {
                    c.pressed.on_button(button, down);
                    Some(Remote::Button(button, down))
                }
                Message::Wheel { dx, dy } => Some(Remote::Wheel(dx, dy)),
                Message::Key { code, down, repeat } => {
                    let code = map_key(c.by_os, code);
                    c.pressed.on_key(code, down);
                    Some(Remote::Key(code, down, repeat))
                }
                Message::Leave { session } if session == c.session => Some(Remote::Release),
                Message::ReleaseAll => Some(Remote::ReleaseKeys),
                _ => None,
            }
        };
        match action {
            Some(Remote::Move(display, x, y)) => self.inject(|i| i.move_to(&display, x, y)),
            Some(Remote::Button(b, down)) => self.inject(|i| i.button(b, down)),
            Some(Remote::Wheel(dx, dy)) => self.inject(|i| i.wheel(dx, dy)),
            Some(Remote::Key(code, down, repeat)) => self.inject(|i| i.key(code, down, repeat)),
            Some(Remote::Release) => self.release_controlled(Some(peer)),
            Some(Remote::ReleaseKeys) => {
                let events = self
                    .inner
                    .controlled
                    .lock()
                    .expect("controlled")
                    .as_mut()
                    .map(|c| c.pressed.release_all())
                    .unwrap_or_default();
                self.inject_releases(events);
            }
            None => {}
        }
    }

    fn on_enter(&self, peer: &DeviceId, peer_os: Os, session: u32, display: &str, x: f64, y: f64) {
        // 换了一台设备来控制：先把上一台按下的键释放掉。
        self.release_controlled(None);
        if self.inner.input
            && flow_platform::permissions().state(Permission::Accessibility)
                == PermissionState::Denied
            && !self
                .inner
                .accessibility_warned
                .swap(true, Ordering::Relaxed)
        {
            self.notice(
                "本机缺少「辅助功能」权限，其他电脑的键鼠无法操作这台电脑。请在 设置 › 系统权限 中授权。",
            );
        }
        info!(device = %short(peer), "开始被控制");
        *self.inner.controlled.lock().expect("controlled") = Some(Controlled {
            by: peer.clone(),
            by_os: peer_os,
            session,
            pressed: PressedState::default(),
            last_seen: Instant::now(),
        });
        self.inject(|i| i.move_to(display, x, y));
        // 键鼠来到本机：对方刚复制的文件可能要在这里粘贴。
        self.on_arrive();
        self.emit();
    }

    /// 结束被控制并释放对方按下的所有键。`by` 为 `None` 时不论是谁都释放。
    fn release_controlled(&self, by: Option<&DeviceId>) {
        let taken = {
            let mut guard = self.inner.controlled.lock().expect("controlled");
            if guard
                .as_ref()
                .is_some_and(|c| by.is_none_or(|b| &c.by == b))
            {
                guard.take()
            } else {
                None
            }
        };
        let Some(mut c) = taken else {
            return;
        };
        info!(device = %short(&c.by), "结束被控制");
        self.inject_releases(c.pressed.release_all());
        self.emit();
    }

    fn inject_releases(&self, events: Vec<InputEvent>) {
        for ev in events {
            match ev {
                InputEvent::Key { code, .. } => self.inject(|i| i.key(code, false, false)),
                InputEvent::Button { button, .. } => self.inject(|i| i.button(button, false)),
            }
        }
    }

    #[cfg(test)]
    fn add_nearby(&self, device: DeviceId, name: &str, addr: SocketAddr) {
        self.lock().nearby.insert(
            device.clone(),
            Found {
                device,
                name: name.into(),
                os: "macos".into(),
                addrs: vec![addr],
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::TransferStatus;

    struct Node {
        engine: Engine,
        notices: Arc<Mutex<Vec<String>>>,
        dir: PathBuf,
    }

    async fn node(tag: &str) -> Node {
        let dir =
            std::env::temp_dir().join(format!("flowacross-engine-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let notices = Arc::new(Mutex::new(Vec::new()));
        let n = notices.clone();
        let notify: Notify = Arc::new(move |ev| {
            if let EngineEvent::Notice(s) = ev {
                n.lock().unwrap().push(s);
            }
        });
        let engine = Engine::start(
            EngineOptions {
                data_dir: dir.clone(),
                port: 0,
                discovery: false,
                main_thread: None,
                input: false,
                receive_dir: Some(dir.join("received")),
            },
            notify,
        )
        .await
        .unwrap();
        Node {
            engine,
            notices,
            dir,
        }
    }

    fn addr(e: &Engine) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], e.inner.port))
    }

    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..100 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("等待超时：{what}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pair_confirm_then_remove() {
        let a = node("a1").await;
        let b = node("b1").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        a.engine.add_nearby(idb.clone(), "B", addr(&b.engine));
        a.engine.pair_with(&idb);

        wait_for("双方出现配对请求", || {
            !a.engine.view().pairings.is_empty() && !b.engine.view().pairings.is_empty()
        })
        .await;
        let (pa, pb) = (
            a.engine.view().pairings[0].clone(),
            b.engine.view().pairings[0].clone(),
        );
        assert_eq!(pa.code, pb.code, "两边配对码一致");
        assert!(pa.outgoing && !pb.outgoing);

        a.engine.respond_pairing(&idb, true);
        b.engine.respond_pairing(&ida, true);
        wait_for("双方建立会话", || {
            let (va, vb) = (a.engine.view(), b.engine.view());
            va.devices
                .iter()
                .any(|d| d.id == idb && d.status == DeviceStatus::Connected)
                && vb
                    .devices
                    .iter()
                    .any(|d| d.id == ida && d.status == DeviceStatus::Connected)
        })
        .await;
        assert!(Config::load(&a.dir).is_trusted(&idb));
        assert!(Config::load(&b.dir).is_trusted(&ida));

        // 移除后对方也删除可信记录
        a.engine.remove_device(&idb);
        wait_for("对方删除配对", || {
            !b.engine.lock().config.is_trusted(&ida)
        })
        .await;
        assert!(
            b.notices
                .lock()
                .unwrap()
                .iter()
                .any(|n| n.contains("移除了与本机的配对"))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rejected_pairing_is_not_trusted() {
        let a = node("a2").await;
        let b = node("b2").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        a.engine.add_nearby(idb.clone(), "B", addr(&b.engine));
        a.engine.pair_with(&idb);
        wait_for("B 收到配对请求", || {
            !b.engine.view().pairings.is_empty()
        })
        .await;
        b.engine.respond_pairing(&ida, false);
        wait_for("A 收到拒绝", || {
            a.notices.lock().unwrap().iter().any(|n| n.contains("拒绝"))
        })
        .await;
        assert!(!a.engine.lock().config.is_trusted(&idb));
        assert!(!b.engine.lock().config.is_trusted(&ida));
        wait_for("配对状态清除", || a.engine.view().pairings.is_empty()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn paired_devices_reconnect_after_restart() {
        let a = node("a3").await;
        let b = node("b3").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        a.engine.add_nearby(idb.clone(), "B", addr(&b.engine));
        a.engine.pair_with(&idb);
        wait_for("配对请求", || !b.engine.view().pairings.is_empty()).await;
        a.engine.respond_pairing(&idb, true);
        b.engine.respond_pairing(&ida, true);
        wait_for("会话", || b.engine.lock().sessions.contains_key(&ida)).await;

        // B "重启"：关闭端点，用同一个数据目录重新启动（端口变化，A 通过 mDNS 才能知道新地址，这里手动告知）
        b.engine.shutdown();
        wait_for("A 发现断开", || {
            !a.engine.lock().sessions.contains_key(&idb)
        })
        .await;
        let notify: Notify = Arc::new(|_| {});
        let b2 = Engine::start(
            EngineOptions {
                data_dir: b.dir.clone(),
                port: 0,
                discovery: false,
                main_thread: None,
                input: false,
                receive_dir: None,
            },
            notify,
        )
        .await
        .unwrap();
        assert_eq!(b2.local_id(), &idb, "重启后身份不变");
        a.engine.add_nearby(idb.clone(), "B", addr(&b2));
        a.engine.lock().backoff.clear();
        b2.add_nearby(ida.clone(), "A", addr(&a.engine));
        b2.lock().backoff.clear();
        wait_for_long("自动重连", || {
            a.engine.lock().sessions.contains_key(&idb) && b2.lock().sessions.contains_key(&ida)
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn auto_reconnect_stops_then_manual_connect_works() {
        let a = node("a4").await;
        let b = node("b4").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        a.engine.add_nearby(idb.clone(), "B", addr(&b.engine));
        a.engine.pair_with(&idb);
        wait_for("配对请求", || !b.engine.view().pairings.is_empty()).await;
        a.engine.respond_pairing(&idb, true);
        b.engine.respond_pairing(&ida, true);
        wait_for("会话", || a.engine.lock().sessions.contains_key(&idb)).await;

        b.engine.shutdown();
        wait_for("A 发现断开", || {
            !a.engine.lock().sessions.contains_key(&idb)
        })
        .await;
        // 模拟自动重连次数已经用完
        a.engine.lock().backoff.insert(
            idb.clone(),
            Backoff {
                next: Instant::now(),
                delay: Duration::from_secs(2),
                attempts: MAX_AUTO_ATTEMPTS,
            },
        );
        wait_for_long("停止自动连接", || {
            a.engine
                .view()
                .devices
                .iter()
                .any(|d| d.id == idb && d.manual)
        })
        .await;
        assert!(
            a.notices
                .lock()
                .unwrap()
                .iter()
                .any(|n| n.contains("已停止自动连接"))
        );

        // B 重新上线（不会主动连 A），A 也不再自动连
        let notify: Notify = Arc::new(|_| {});
        let b2 = Engine::start(
            EngineOptions {
                data_dir: b.dir.clone(),
                port: 0,
                discovery: false,
                main_thread: None,
                input: false,
                receive_dir: None,
            },
            notify,
        )
        .await
        .unwrap();
        a.engine.add_nearby(idb.clone(), "B", addr(&b2));
        tokio::time::sleep(TICK + Duration::from_millis(500)).await;
        assert!(!a.engine.lock().sessions.contains_key(&idb), "不应自动重连");

        // 手动连接
        a.engine.connect_device(&idb);
        wait_for_long("手动连接成功", || {
            a.engine.lock().sessions.contains_key(&idb) && b2.lock().sessions.contains_key(&ida)
        })
        .await;
        assert!(!a.engine.view().devices.iter().any(|d| d.manual));
    }

    async fn paired(ta: &str, tb: &str) -> (Node, Node) {
        let a = node(ta).await;
        let b = node(tb).await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        a.engine.add_nearby(idb.clone(), "B", addr(&b.engine));
        a.engine.pair_with(&idb);
        wait_for("配对请求", || !b.engine.view().pairings.is_empty()).await;
        a.engine.respond_pairing(&idb, true);
        b.engine.respond_pairing(&ida, true);
        wait_for("会话", || {
            a.engine.lock().sessions.contains_key(&idb)
                && b.engine.lock().sessions.contains_key(&ida)
        })
        .await;
        (a, b)
    }

    fn latest_transfer(e: &Engine) -> Option<TransferView> {
        e.view().transfers.into_iter().next()
    }

    /// A 复制文件和文件夹 → B 收到清单 → B 接收，内容一致；A 的剪贴板变了之后再取会失败并提示原因；
    /// 键鼠来到 B 时自动取回，同名文件改名。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copied_files_reach_the_other_device() {
        let (a, b) = paired("a5", "b5").await;
        let src = a.dir.join("src");
        let photos = src.join("照片");
        std::fs::create_dir_all(photos.join("2024")).unwrap();
        std::fs::create_dir_all(photos.join("空文件夹")).unwrap();
        // 跨好几个块的大文件 + 空文件
        let big: Vec<u8> = (0..(flow_core::files::CHUNK_SIZE * 3 + 123))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(photos.join("2024").join("a.bin"), &big).unwrap();
        std::fs::write(photos.join("2024").join("空.txt"), b"").unwrap();
        std::fs::write(photos.join(".DS_Store"), b"junk").unwrap();
        let report = src.join("报告.txt");
        std::fs::write(&report, "你好，流界").unwrap();

        let epoch = a.engine.clipboard_epoch();
        a.engine
            .clone()
            .offer_files(vec![report.clone(), photos.clone()], epoch)
            .await;
        wait_for("B 收到清单", || b.engine.view().file_offer.is_some()).await;
        let offer = b.engine.view().file_offer.unwrap();
        assert_eq!(offer.label, "报告.txt 等 2 项");
        assert_eq!(offer.files, 3);
        assert_eq!(offer.bytes, big.len() as u64 + "你好，流界".len() as u64);

        // 键鼠还在 A，B 上用户手动接收。
        b.engine.accept_files();
        wait_for("B 收完", || {
            latest_transfer(&b.engine).is_some_and(|t| t.status == TransferStatus::Done)
        })
        .await;
        let recv = b.dir.join("received");
        assert_eq!(
            std::fs::read_to_string(recv.join("报告.txt")).unwrap(),
            "你好，流界"
        );
        assert_eq!(
            std::fs::read(recv.join("照片").join("2024").join("a.bin")).unwrap(),
            big
        );
        assert!(recv.join("照片").join("2024").join("空.txt").is_file());
        assert!(recv.join("照片").join("空文件夹").is_dir());
        assert!(!recv.join("照片").join(".DS_Store").exists());
        let t = latest_transfer(&b.engine).unwrap();
        assert!(t.incoming && t.done == offer.bytes);
        wait_for("A 显示发送完成", || {
            latest_transfer(&a.engine)
                .is_some_and(|t| !t.incoming && t.status == TransferStatus::Done)
        })
        .await;
        // 没有残留临时目录
        let leftovers = std::fs::read_dir(&recv)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(leftovers, 0);

        // A 的剪贴板换了内容，旧清单取不到了。
        a.engine.clone().offer_files(vec![photos], epoch).await;
        wait_for("第二份清单", || b.engine.view().file_offer.is_some()).await;
        a.engine.inner.files.lock().unwrap().local = None;
        b.engine.accept_files();
        wait_for("提示失败原因", || {
            b.notices
                .lock()
                .unwrap()
                .iter()
                .any(|n| n.contains("复制的内容已经变了"))
        })
        .await;
        assert_eq!(
            latest_transfer(&b.engine).unwrap().status,
            TransferStatus::Failed
        );
        assert!(!recv.join("照片 (1)").exists());

        // 同一个文件再复制一次，键鼠来到 B 时自动取回：重名改为「报告 (1).txt」。
        a.engine.clone().offer_files(vec![report], epoch).await;
        wait_for("第三份清单", || b.engine.view().file_offer.is_some()).await;
        b.engine.on_arrive();
        assert!(b.engine.view().file_offer.is_none(), "已经开始自动接收");
        wait_for("第三次收完", || {
            recv.join("报告 (1).txt").is_file()
                && latest_transfer(&b.engine).is_some_and(|t| t.status == TransferStatus::Done)
        })
        .await;
    }

    /// 接收方取消：两边都停下，不留下半个文件。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn receiver_can_cancel_transfer() {
        let (a, b) = paired("a6", "b6").await;
        let src = a.dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // 足够大，取消时还没传完
        let file = src.join("大文件.bin");
        std::fs::write(&file, vec![1u8; 64 * 1024 * 1024]).unwrap();
        let epoch = a.engine.clipboard_epoch();
        a.engine.clone().offer_files(vec![file], epoch).await;
        wait_for("B 收到清单", || b.engine.view().file_offer.is_some()).await;
        b.engine.accept_files();
        wait_for("开始传输", || {
            latest_transfer(&b.engine).is_some_and(|t| t.done > 0)
        })
        .await;
        let id = latest_transfer(&b.engine).unwrap().id;
        b.engine.cancel_transfer(id);
        wait_for("B 已取消", || {
            latest_transfer(&b.engine).is_some_and(|t| t.status == TransferStatus::Canceled)
        })
        .await;
        wait_for("A 停止发送", || {
            latest_transfer(&a.engine).is_some_and(|t| t.status == TransferStatus::Failed)
        })
        .await;
        let recv = b.dir.join("received");
        assert_eq!(
            std::fs::read_dir(&recv).unwrap().count(),
            0,
            "不留下半个文件"
        );
        assert!(
            !b.notices.lock().unwrap().iter().any(|n| n.contains("失败")),
            "取消不算失败，不弹提示"
        );
    }

    async fn wait_for_long(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..300 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("等待超时：{what}");
    }
}
