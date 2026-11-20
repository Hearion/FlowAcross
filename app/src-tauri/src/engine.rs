//! 引擎：管理本机身份、配置、设备发现、连接、配对与自动重连、跨屏控制，并把状态推给界面。
//!
//! 并发模型：配置、会话、设备状态在 `state` 这把 `std::sync::Mutex` 里，控制器、剪贴板、文件传输、
//! 被控制状态等各有自己的锁（顺序见 `Inner` 各字段的说明），**持锁期间不 await**；
//! 每条连接一个 tokio 任务（握手 → 配对或会话），写方向再单独一个任务。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use flow_core::Rect;
use flow_core::autolock::{self, AutoLock, Foreground, LatencyLevel};
use flow_core::clipboard::ClipboardSync;
use flow_core::diagnose::{
    self, Cause, ConnectFacts, Diagnosis, Fault, Gone, LocalNet, ResumeDetector, offline_note,
};
use flow_core::files::{
    FileOffer, IMAGE_FILE_NAME, MAX_IMAGE_BYTES, OfferBook, peer_supports_files,
    peer_supports_images,
};
use flow_core::geometry::Point;
use flow_core::hotkeys::Hotkeys;
use flow_core::keys::{InputEvent, KeyCode, Modifiers, PressedState, WinToMacKeys};
use flow_core::layout::{DeviceId, Display, DisplayId, Layout, SharedLayout};
use flow_core::pairing::{Nonce, PairingCode, commitment, device_id, sas_code, verify_commitment};
use flow_core::protocol::{
    ByeReason, DisplayInfo, MAX_KEYSTROKE_KEYS, Message, Os, PROTOCOL_VERSION, clean_name,
    peer_supports_remote_actions, peer_supports_secure, peer_supports_wake, sane_displays,
    settle_displays,
};
use flow_core::switching::{SwitchPolicy, accepts_control};
use flow_core::throttle::{MoveThrottle, RateLimit};
use flow_net::discovery::{Advert, Discovery, DiscoveryEvent, Found};
use flow_net::quinn;
use flow_net::{
    Identity, Link, MoveReceiver, MoveSender, channel_binding, peer_fingerprint, random_bytes,
};
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
/// 被控制期间，最多每隔这么久告诉系统一次"有键鼠操作"（见 `Engine::remote_activity`）。
const REMOTE_ACTIVITY_EVERY: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// 设备离线后自动重连的次数（约 30 秒内）。用完后停止，等用户手动连接或对方主动连过来。
const MAX_AUTO_ATTEMPTS: u32 = 5;
/// 「附近的设备」最多记这么多台（mDNS 可以被局域网里任何人伪造）。
const MAX_NEARBY: usize = 256;

/// 后续的无效地址不能盖过已经发生的超时或身份校验失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ConnectFailureKind {
    Address,
    Transport,
    Timeout,
    Handshake,
    Identity,
}

#[derive(Debug)]
struct ConnectFailure {
    kind: ConnectFailureKind,
    details: String,
}

impl std::fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.details)
    }
}

impl ConnectFailure {
    /// 换成诊断用的分类。
    fn fault(&self) -> Fault {
        match self.kind {
            ConnectFailureKind::Address => Fault::NoAddress,
            ConnectFailureKind::Transport | ConnectFailureKind::Timeout => Fault::Unreachable,
            ConnectFailureKind::Handshake => Fault::Handshake,
            ConnectFailureKind::Identity => Fault::Identity,
        }
    }
}

/// 睡眠唤醒后这么久之内，离线的设备显示成「正在重连」（网络恢复通常要几秒到十几秒）。
const RESUME_WINDOW: Duration = Duration::from_secs(45);
/// 定时器比预期晚了这么久，就认为系统睡过一觉（见 [`ResumeDetector`]）。此时对方那边的连接
/// 早已超时（QUIC 空闲超时 8 秒），本机这边的也没用了。
const RESUME_SLACK: Duration = Duration::from_secs(13);

/// 本机现在的网络地址。单元测试里用固定值，诊断结果不随跑测试的机器变化。
fn local_nets() -> Vec<LocalNet> {
    if cfg!(test) {
        vec![LocalNet {
            addr: std::net::Ipv4Addr::new(192, 168, 1, 2).into(),
            prefix: 24,
        }]
    } else {
        flow_net::local_networks()
    }
}

/// 把键鼠切到一台停在锁屏界面的电脑时的提示。
fn locked_notice(name: &str, os: Os) -> String {
    match os {
        Os::MacOs | Os::Windows => format!(
            "「{name}」停在锁屏界面，需要在那台电脑上用它自己的键盘解锁：{} 的锁屏界面不接受其他电脑发来的按键。",
            if os == Os::MacOs { "macOS" } else { "Windows" }
        ),
        Os::Linux | Os::Other => format!(
            "「{name}」停在锁屏界面。可以直接输入密码试试；输不进去的话，需要在那台电脑上解锁。"
        ),
    }
}

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
/// 没有设备连着时，上面那个定时器放慢到这个间隔。
const IDLE_TICK: Duration = Duration::from_millis(500);
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);
/// 传文件期间推送进度的最小间隔。
const PROGRESS_EMIT: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// 推给界面的状态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct StateView {
    /// 这份状态的序号，越大越新。并发推送时界面据此丢掉过时的一份。
    pub seq: u64,
    pub local: LocalView,
    pub onboarded: bool,
    pub port: u16,
    pub layout: Layout,
    pub devices: Vec<DeviceView>,
    pub nearby: Vec<NearbyView>,
    pub pairings: Vec<PairingView>,
    pub settings: Settings,
    pub locked: bool,
    /// 键鼠被自动锁定在本机的原因（哪个应用在前台 / 全屏）；没有自动锁定时为 `None`。
    /// 和手动的 `locked` 互不影响，任何一个成立都切不出去。
    pub auto_lock: Option<AutoLock>,
    /// 最近在前台的应用（最新的在前），设置页往自动锁定的名单里加应用时挑选。
    pub recent_apps: Vec<String>,
    pub permissions: Vec<PermissionView>,
    /// 权限相关的额外说明（例如开发模式下权限归属于终端）。
    pub permission_hint: Option<String>,
    /// 本机正在被哪台设备控制。
    pub controlled_by: Option<DeviceId>,
    pub low_latency: LowLatencyView,
    /// 当前系统做不到的切换方式和效果，界面不显示对应的设置。
    pub unsupported: UnsupportedView,
    /// 这台电脑上做不到「保持屏幕常亮」的原因；做得到时为 `None`。界面据此禁用开关并显示原因。
    pub keep_awake_problem: Option<String>,
    pub about: AboutView,
    /// 正在进行和最近结束的文件传输，最新的在前。
    pub transfers: Vec<TransferView>,
    /// 其他设备复制了文件、等待用户手动接收（太大没有自动接收）。
    pub file_offer: Option<FileOfferView>,
    /// 收到的文件保存在这里；`None` 表示找不到下载文件夹，无法接收文件。
    pub receive_dir: Option<String>,
    /// 启动时配置文件损坏的说明（原因和备份位置），用户点「知道了」前一直显示。
    pub config_warning: Option<String>,
    /// 系统的授权弹窗正在等用户点，界面显示倒计时。
    pub auth_prompt: Option<AuthPromptView>,
    /// 没拿到授权的事（`capture` / `inject`）：界面提示并给「重新授权」。下次需要时也会自动再问。
    pub auth_denied: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileOfferView {
    pub peer: DeviceId,
    pub peer_name: String,
    pub label: String,
    pub files: u64,
    pub bytes: u64,
}

/// 系统的授权弹窗正在等用户点（只有 Linux 的 GNOME / KDE 会有）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct AuthPromptView {
    /// `capture`：捕获这台电脑的键鼠；`inject`：控制这台电脑的键鼠。
    pub kind: &'static str,
    /// 还剩多少秒自动关闭。界面收到后自己往下数。
    pub seconds_left: u32,
}

fn auth_kind(kind: flow_platform::AuthKind) -> &'static str {
    match kind {
        flow_platform::AuthKind::Capture => "capture",
        flow_platform::AuthKind::Inject => "inject",
    }
}

/// 见 [`flow_platform::Limits`]。`true` 表示在这台电脑上用不了。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UnsupportedView {
    pub hold_key: bool,
    pub local_hotkey: bool,
    pub dwell: bool,
    pub edge_glow: bool,
    /// 按前台应用 / 全屏自动锁定：查不到前台是哪个应用。
    pub auto_lock: bool,
}

impl From<flow_platform::Limits> for UnsupportedView {
    fn from(l: flow_platform::Limits) -> Self {
        Self {
            hold_key: l.hold_key,
            local_hotkey: l.local_hotkey,
            dwell: l.dwell,
            edge_glow: l.edge_glow,
            auto_lock: l.foreground_app,
        }
    }
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
    /// 界面上「键鼠在这里」标在这台设备上。通常和 `active` 一样；例外是别的设备刚结束对本机的控制：
    /// 键鼠跟着它回去了，这时标在它上面而不是本机，直到本机有键鼠输入。
    pub has_input: bool,
    pub paused: bool,
    /// 对方暂停了共享或暂停了本机，暂时不能切过去。
    pub remote_paused: bool,
    /// 可以请它点亮屏幕：连着，而且对方的版本认得这条消息。
    pub can_wake: bool,
    /// 对方停在锁屏界面（它自己报告的；旧版本不报告，一直是 false）。
    pub locked: bool,
    /// 现在可以把键鼠切到这台设备（托盘菜单和设备详情里的「把键鼠切到这里」据此启用）。
    pub can_switch: bool,
    /// 快捷键映射可以把按键发给它：连着，而且对方的版本认得这条消息。
    pub can_send_keys: bool,
    /// 到这台设备的网络延迟（往返时间，毫秒，不到 1 毫秒时是 0）；没连着、本机这一项为 `None`。
    pub latency_ms: Option<u32>,
    /// 延迟算好、一般还是差（见 `flow_core::autolock::latency_level`）。
    pub latency_level: Option<LatencyLevel>,
    /// 没连上的原因和处理办法；连着、或没有可说的时为 `None`。
    pub problem: Option<ProblemView>,
}

/// 设备连不上 / 离线的原因和排查步骤。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProblemView {
    pub summary: String,
    pub steps: Vec<String>,
}

impl From<&Diagnosis> for ProblemView {
    fn from(d: &Diagnosis) -> Self {
        Self {
            summary: d.summary.clone(),
            steps: d.steps.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NearbyView {
    pub id: DeviceId,
    pub name: String,
    pub os: Os,
    /// 正在连接或配对中。
    pub connecting: bool,
    /// 上次配对没连上的原因和处理办法。
    pub problem: Option<ProblemView>,
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
    /// 只有传输进度变了：单独推传输列表，不用每秒好几次重建并序列化整份状态。
    Transfers(Vec<TransferView>),
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
    /// 键鼠捕获现在是否在运行（`capture` 是否为 `Some`）。生成界面状态时要看，
    /// 单独记一份免得去拿 `capture` 那把锁。
    capture_on: AtomicBool,
    /// 推给界面的状态的序号，在 `state` 锁里递增：两个任务同时推送时，界面丢掉先算出来却后到的那份。
    view_seq: AtomicU64,
    capture_warned: AtomicBool,
    /// `shutdown()` 之后为 true：后台循环退出，不再重连、不再重新安装键鼠捕获。
    stopped: AtomicBool,
    /// 系统接口失败后，下次重试的时间（避免频繁调用系统接口）。
    capture_retry_at: Mutex<Option<Instant>>,
    /// 被控制方：当前控制本机的设备，以及它按下的键（失败时据此释放）。
    controlled: Mutex<Option<Controlled>>,
    injector: Mutex<Option<Box<dyn InputInjector>>>,
    /// 被 PC 键盘控制时的修饰键对应（设置的副本），改设置时不用等注入完成。
    /// 锁顺序：`state` → `controlled` → `win_keys`。
    win_keys: Mutex<WinToMacKeys>,
    accessibility_warned: AtomicBool,
    /// 捕获键鼠的授权没拿到（用户拒绝、到时间没点、被系统收回）。不自动重试——那等于隔一阵就弹
    /// 一次窗——等"又需要了"（有设备连上）或用户点「重新授权」时再申请。
    capture_denied: AtomicBool,
    /// 注入键鼠用不了（没拿到授权，或注入器建不起来）。这次被控制期间不再尝试，否则每个键鼠事件
    /// 都重试一次；下次有电脑把键鼠切过来时再试。
    inject_blocked: AtomicBool,
    /// 系统权限的查询结果和查询时刻。每次推状态都要用，而查一次是一次系统调用（macOS 的
    /// 辅助功能、输入监控），所以一秒内复用上一次的。
    permissions: Mutex<Option<(Instant, Vec<PermissionView>)>>,
    /// 现在没有任何设备连着：定时器可以放慢。
    idle: AtomicBool,
    /// 上次推给界面的授权状态（弹窗在等的是哪一种、哪些被拒绝了），变了才推。
    auth_shown: Mutex<(Option<&'static str>, Vec<&'static str>)>,
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
    /// 上次要求系统"保持屏幕常亮"的状态；`None` 表示还没同步过。
    keep_awake_active: Mutex<Option<bool>>,
    /// 这台电脑上做不到「保持屏幕常亮」的原因（启动时探测，做不到时定时重查）。
    keep_awake_problem: Mutex<Option<String>>,
    /// 已经提示过"没能保持屏幕常亮"。
    keep_awake_warned: AtomicBool,
    /// 上次告诉系统"别的电脑的键鼠在操作本机"的时刻（引擎启动后的毫秒数 + 1），0 表示还没有过。
    remote_activity_at: AtomicU64,
    /// 键鼠被这台设备带走了：它刚结束对本机的控制，之后本机还没有过键鼠输入。只用于界面显示。
    input_at: Mutex<Option<DeviceId>>,
    /// 正处在握手 / 配对阶段（对方还没通过认证）的入站连接数，见 [`HandshakeSlot`]。
    unauthenticated: Arc<AtomicUsize>,
    /// 别人发来的配对请求（本机没有发起）的频率限制。
    pair_requests: Mutex<RateLimit>,
}

/// 同时处在握手 / 配对阶段的入站连接上限。正常情况下同一时刻最多一两个。
const MAX_UNAUTHENTICATED: usize = 8;

/// 占用一个"未认证入站连接"名额，丢弃时归还。进入会话（双方互信）时就归还。
struct HandshakeSlot(Arc<AtomicUsize>);

impl HandshakeSlot {
    fn take(count: &Arc<AtomicUsize>) -> Option<Self> {
        // 先占再判断：超了就由丢弃的 slot 归还名额。
        let slot = Self(count.clone());
        (count.fetch_add(1, Ordering::SeqCst) < MAX_UNAUTHENTICATED).then_some(slot)
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
    /// 复制文件时没有能收文件的设备连着：先只记下路径和当时剪贴板的代，
    /// 等设备连上再整理清单发过去（见 `run_session`）。
    unoffered: Option<(Vec<PathBuf>, u64)>,
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
    /// 对方按下的键（它发来的键码）→ 当时换算后注入的键码。抬起时按这里的记录注入，
    /// 中途改了「Windows 键盘的修饰键」设置也不会按下一个键、抬起另一个键（卡键）。
    mapped: HashMap<KeyCode, KeyCode>,
    last_seen: Instant,
}

/// 被控制方换算对方的修饰键。PC 键盘控制 Mac 时按设置里的键位（默认 Ctrl → ⌘）；
/// Mac 控制 PC 时 ⌘ 与 Ctrl 互换：在 Mac 上按 ⌘C，Windows 收到 Ctrl+C。
fn map_key(from: Os, local: Os, win_keys: WinToMacKeys, code: KeyCode) -> KeyCode {
    if from == local {
        code
    } else if local == Os::MacOs {
        win_keys.apply(code)
    } else if from == Os::MacOs {
        code.swap_meta_ctrl()
    } else {
        code
    }
}

/// 控制器实际使用的快捷键。看不到全局键盘的桌面（Linux 的 Wayland）不支持快捷键映射：键鼠在本机时
/// 按触发键没有反应，只有键鼠已经切到别的电脑后才触发，和"不用切过去"的用途相反（用户反馈），
/// 设置页也不显示这一节，所以配置里留着的映射（比如从 X11 会话带过来的）一律不用。
fn usable_hotkeys(hotkeys: &Hotkeys, limits: flow_platform::Limits) -> Hotkeys {
    let mut hotkeys = hotkeys.clone();
    if limits.local_hotkey {
        hotkeys.mappings.clear();
    }
    hotkeys
}

/// `limits`：当前系统做不到的切换方式。做不到的设置一律忽略，否则打开后永远切不过去
/// （比如看不到键盘，就永远等不到"修饰键按着"）。
fn switch_policy(settings: &Settings, locked: bool, limits: flow_platform::Limits) -> SwitchPolicy {
    SwitchPolicy {
        require_modifiers: match settings.hold_key {
            _ if limits.hold_key => Modifiers::NONE,
            HoldKey::None => Modifiers::NONE,
            HoldKey::Ctrl => Modifiers::CTRL,
            HoldKey::Alt => Modifiers::ALT,
            HoldKey::Shift => Modifiers::SHIFT,
        },
        dwell_ms: if limits.dwell { 0 } else { settings.dwell_ms },
        corner_deadzone: if settings.corner_guard {
            flow_core::switching::CORNER_DEADZONE
        } else {
            0.0
        },
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
    /// 启动时配置文件损坏的说明，用户确认前一直显示。
    config_warning: Option<String>,
    /// 配置文件读不了也备份不了：这次运行不写配置，避免覆盖原文件。
    config_readonly: bool,
    /// 告诉本机"现在不接受控制"的设备（对方暂停了共享或暂停了本机），不往它那边切。
    remote_blocked: HashSet<DeviceId>,
    /// 最近一次连接失败的诊断（自动重连和手动连接都记），连上或开始配对后清掉。
    failures: HashMap<DeviceId, Diagnosis>,
    /// 设备为什么离线了（对方退出、突然没回应、本机刚睡醒），连上后清掉。
    gone: HashMap<DeviceId, Gone>,
    /// 报告自己停在锁屏界面的设备。
    remote_locked: HashSet<DeviceId>,
    /// 已经提示过"它锁着屏"的设备，解锁后清掉，下次锁屏再提示。
    lock_warned: HashSet<DeviceId>,
    /// 本机是否停在锁屏界面（定时查询的结果）；`None` 表示还没查过或查不到。
    local_locked: Option<bool>,
    /// 本机刚从睡眠中醒来，在这个时刻之前把离线的设备显示成「正在重连」。
    resume_until: Option<Instant>,
    /// 睡前就没连着的设备：醒来后照样再试一轮（可能是带着笔记本回到了有它的网络），
    /// 但这一轮试不通时不弹「已暂停自动重试」——用户并没有丢失什么连接。
    quiet_retry: HashSet<DeviceId>,
    /// 定时查到的本机前台应用；没查过、查不到时为 `None`。
    foreground: Option<Foreground>,
    /// 键鼠现在被自动锁定在本机的原因（见 [`State::refresh_auto_lock`]）。
    auto_lock: Option<AutoLock>,
    /// 最近在前台的应用，最新的在前。
    recent_apps: Vec<String>,
    /// 上次推给界面的各设备延迟（毫秒）。延迟一直在抖，变化明显时才重新推。
    latency_shown: HashMap<DeviceId, u32>,
}

struct Session {
    tx: mpsc::UnboundedSender<Message>,
    conn: quinn::Connection,
    /// 发起这条连接的设备。两边同时连上对方时，双方都保留"发起方 ID 较小"的那条，保证结果一致。
    initiator: DeviceId,
    /// 对方支持跨设备复制文件（看版本）。
    files: bool,
    /// 对方支持复制图片（看版本）；不支持时图片按普通文件发过去。
    images: bool,
    /// 对方支持 0.1.7 新增的消息：心跳回执、`Availability`（看版本）。
    acks: bool,
    /// 对方认得 `Wake`（看版本）。
    wake: bool,
    /// 对方认得 0.2.4 新增的 `KeyStroke`、`SwitchTo`、`Status`（看版本）。
    remote_actions: bool,
    /// 上次告诉对方的"本机是否接受它控制"；`None` 表示还没发过。
    accepting_sent: Option<bool>,
}

/// 可以收发剪贴板的一台设备。
struct ClipPeer {
    tx: mpsc::UnboundedSender<Message>,
    /// 可以发文件清单给它。
    files: bool,
    /// 它能把收到的图片直接写入剪贴板。
    images: bool,
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
///
/// 只在第一次调用时查询：macOS 上要启动 `scutil` 子进程，而用户没起名字时每次刷新界面都会用到它。
pub fn system_device_name() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(query_system_device_name).clone()
}

fn query_system_device_name() -> String {
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

/// 平台错误里给用户看的部分（去掉"系统调用失败："之类的前缀）。
fn platform_reason(e: &PlatformError) -> String {
    match e {
        PlatformError::Os(m) | PlatformError::Denied(m) => m.clone(),
        other => other.to_string(),
    }
}

/// macOS 按"负责进程"记录权限：没有打包成 .app 运行时（`npm run tauri dev`），
/// 权限会算在启动它的终端或编辑器上，系统设置的列表里看不到流界。
fn permission_hint() -> Option<String> {
    // 只取决于程序自己的路径，运行期间不会变；每次刷新界面都会用到，算一次就够了。
    static HINT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HINT.get_or_init(compute_permission_hint).clone()
}

fn compute_permission_hint() -> Option<String> {
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

/// 日志里只打印设备 ID 前 8 位（按字符截断：ID 可能来自伪造的 mDNS 广播）。
fn short(id: &DeviceId) -> &str {
    id.short()
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
    /// 把大写锁定对齐到对方的状态。
    Caps(bool),
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
        // 系统的授权弹窗没通过（Linux 的 GNOME / KDE）。
        PlatformError::Denied(why) => format!(
            "流界没有拿到捕获键鼠的授权（{why}），键鼠暂时切不到其他电脑。下次有设备连上时会再询问一次，也可以现在点「重新授权」。"
        ),
        // 这个系统或会话类型本来就不支持（例如 Linux 的 Wayland 会话），重新打开也没用。
        PlatformError::Unsupported(what) => {
            format!("当前系统暂不支持：{what}。键鼠无法切到其他电脑。")
        }
        other => {
            format!("无法监听键鼠（{other}），键鼠暂时无法切到其他电脑。请重新打开流界后再试。")
        }
    }
}

/// 流界自己的应用名：自己的窗口在前台时不触发自动锁定。平台层查到前台是本进程时应用名给的是空，
/// 这里再按名字挡一次（同一台电脑上开着的另一个流界实例）。
const OWN_APP: &str = "FlowAcross";

/// 一条连接现在的延迟：QUIC 自己估算的往返时间，四舍五入到毫秒。连接每 2 秒发一次保活，
/// 空闲时这个值也一直在更新，不用另外发包去量。
fn session_latency(s: &Session) -> u32 {
    let micros = s.conn.rtt().as_micros();
    u32::try_from((micros + 500) / 1000).unwrap_or(u32::MAX)
}

impl State {
    /// 按当前设置和上次查到的前台应用重新判定要不要自动锁定。返回有没有变化。
    fn refresh_auto_lock(&mut self) -> bool {
        let s = &self.config.settings;
        let next = autolock::auto_lock(
            s.auto_lock_fullscreen,
            &s.auto_lock_apps,
            self.foreground.as_ref(),
            OWN_APP,
        );
        let changed = next != self.auto_lock;
        self.auto_lock = next;
        changed
    }

    /// 自动锁定的设置有没有打开（没打开时不用为它去查前台应用）。
    fn auto_lock_wanted(&self) -> bool {
        let s = &self.config.settings;
        s.auto_lock_fullscreen || !s.auto_lock_apps.is_empty()
    }

    /// 一台没连上的设备现在该显示什么原因。本机刚睡醒时先说"正在重连"；对方是自己退出或
    /// 突然没回应的，用这个当原因（突然没回应时附上最近一次连接失败的排查步骤）；
    /// 其余用最近一次连接失败的诊断。
    fn problem(&self, id: &DeviceId, name: &str) -> Option<ProblemView> {
        if self.sessions.contains_key(id) {
            return None;
        }
        let note = |gone: Gone, steps: Vec<String>| ProblemView {
            summary: offline_note(gone, name),
            steps,
        };
        let failure = self.failures.get(id);
        // 对方退出或睡眠后，局域网里它的 mDNS 记录还会留一阵（睡眠的机器来不及说"我走了"，
        // 最长两分钟才过期），这期间的重连会被诊断成"看得到却连不上：防火墙"。已经知道它是
        // 怎么不见的，就不拿这个误判盖掉真正的原因。
        let expected = |f: &Diagnosis| matches!(f.cause, Cause::NotFound | Cause::Blocked);
        match self.gone.get(id) {
            Some(Gone::LocalResume) if self.resuming() => Some(note(Gone::LocalResume, vec![])),
            Some(Gone::Quit) if failure.is_none_or(expected) => Some(note(Gone::Quit, vec![])),
            Some(Gone::Silent) if failure.is_none_or(expected) => Some(note(
                Gone::Silent,
                failure
                    .filter(|f| f.cause == Cause::NotFound)
                    .map(|f| f.steps.clone())
                    .unwrap_or_default(),
            )),
            _ => failure.map(ProblemView::from),
        }
    }

    /// 本机刚从睡眠中醒来，还在重新连接的那段时间里。
    fn resuming(&self) -> bool {
        self.resume_until.is_some_and(|t| Instant::now() < t)
    }

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
    /// 返回这次是否真的放了（之前没有位置、现在有了）。
    fn place_new_peer(&mut self, local: &DeviceId, id: &DeviceId) -> bool {
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
            return false;
        };
        if peer.offset.is_some() {
            return false;
        }
        match bounds(&peer.displays) {
            Some((min_x, min_y)) => {
                peer.offset = Some((right - min_x, local_top - min_y));
                true
            }
            None => false,
        }
    }

    /// 本机眼里的整份布局（本机和每台已经有位置的设备），用来发给别的电脑同步。
    fn shared_layout(&self, local: &DeviceId) -> SharedLayout {
        let mut offsets = vec![(local.clone(), self.config.local_offset)];
        offsets.extend(
            self.config
                .peers
                .iter()
                .filter_map(|p| p.offset.map(|o| (p.id.clone(), o))),
        );
        SharedLayout {
            rev: self.config.layout_rev,
            by: self
                .config
                .layout_by
                .clone()
                .unwrap_or_else(|| local.clone()),
            offsets,
        }
    }

    /// 把本机的布局发给别的电脑。`only`：只发给这一台；`except`：这一台不发（布局就是它给的）。
    /// 只发给认得这条消息、而且已经在本机布局里有位置的设备：布局里没有它自己的话，它没法
    /// 对上坐标系。
    fn send_layout(&self, local: &DeviceId, only: Option<&DeviceId>, except: Option<&DeviceId>) {
        if !self.config.settings.sync_layout {
            return;
        }
        let layout = self.shared_layout(local);
        for (id, s) in &self.sessions {
            let wanted = only.is_none_or(|o| o == id) && except != Some(id);
            let placed = self.config.peer(id).is_some_and(|p| p.offset.is_some());
            if wanted && placed && s.remote_actions {
                let _ = s.tx.send(Message::Layout(layout.clone()));
            }
        }
    }
}

impl Engine {
    pub async fn start(opts: EngineOptions, notify: Notify) -> Result<Engine, String> {
        let identity = Identity::load_or_create(&opts.data_dir).map_err(|e| e.to_string())?;
        // 配置损坏时已备份原文件，用默认配置继续运行，并在界面上一直提示到用户点「知道了」。
        // 连备份都失败时不再写配置，避免覆盖原文件。
        let (config, config_problem) = match Config::load(&opts.data_dir) {
            Ok(c) => (c, None),
            Err(p) => {
                warn!(reason = %p.reason, backup = ?p.backup, "配置文件损坏");
                (Config::default(), Some(p))
            }
        };
        let config_readonly = config_problem.as_ref().is_some_and(|p| p.backup.is_none());
        let config_warning = config_problem.map(|p| p.message());
        let mut config = config;
        // 配置文件可能被手动改过：不合法、重复的快捷键在这里就整理掉。
        config.settings.hotkeys = std::mem::take(&mut config.settings.hotkeys).sanitized();
        config.settings.auto_lock_apps =
            autolock::sanitize_apps(std::mem::take(&mut config.settings.auto_lock_apps));
        let win_keys = config.settings.win_to_mac_keys();
        let endpoint = flow_net::bind_endpoint(&identity, opts.port).map_err(|e| e.to_string())?;
        let port = endpoint.local_addr().map_err(|e| e.to_string())?.port();
        let local_displays = flow_platform::display_provider()
            .displays()
            .unwrap_or_default();
        // Linux 上要问一次会话总线，放到阻塞线程里。
        let keep_awake_problem = if opts.input {
            tokio::task::spawn_blocking(flow_platform::power::keep_awake_problem)
                .await
                .unwrap_or(None)
        } else {
            None
        };

        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let cursor: crate::input::CursorControl = if opts.input {
            Box::new(flow_platform::set_cursor_captured)
        } else {
            Box::new(|_, _| {})
        };
        let mut controller = Controller::new(identity.device_id.clone(), ctl_tx, cursor);
        if opts.input {
            controller.set_caps_probe(Box::new(flow_platform::caps_lock_on));
        }
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
                    config_warning,
                    config_readonly,
                    ..Default::default()
                }),
                input: opts.input,
                controller: Arc::new(Mutex::new(controller)),
                capture: Mutex::new(None),
                capture_on: AtomicBool::new(false),
                view_seq: AtomicU64::new(0),
                capture_warned: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                capture_retry_at: Mutex::new(None),
                controlled: Mutex::new(None),
                injector: Mutex::new(None),
                win_keys: Mutex::new(win_keys),
                accessibility_warned: AtomicBool::new(false),
                capture_denied: AtomicBool::new(false),
                inject_blocked: AtomicBool::new(false),
                auth_shown: Mutex::new((None, Vec::new())),
                permissions: Mutex::new(None),
                idle: AtomicBool::new(true),
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
                keep_awake_active: Mutex::new(None),
                keep_awake_problem: Mutex::new(keep_awake_problem),
                keep_awake_warned: AtomicBool::new(false),
                remote_activity_at: AtomicU64::new(0),
                input_at: Mutex::new(None),
                unauthenticated: Arc::new(AtomicUsize::new(0)),
                // 每分钟最多弹 3 次别人发起的配对。
                pair_requests: Mutex::new(RateLimit::new(3, 60_000)),
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
    /// 可以重复调用（托盘「退出」和应用退出事件都会调到），只有第一次生效。
    pub fn shutdown(&self) {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.controller().force_local();
        if let Some(mut c) = self.inner.capture.lock().expect("capture").take() {
            self.inner.capture_on.store(false, Ordering::SeqCst);
            c.stop();
        }
        // 停掉 mDNS：退出时向局域网发一条"我走了"，别的电脑马上就发现不了本机，
        // 而不是等两分钟记录过期（那段时间它们会把"连不上"误判成防火墙拦截）。
        drop(self.inner.discovery.lock().expect("discovery").take());
        self.release_controlled(None);
        self.inner
            .files
            .lock()
            .expect("files")
            .transfers
            .cancel_all();
        if self.inner.input {
            // 退出前恢复 AWDL。
            let _ = flow_platform::low_latency::set_active(false);
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

    /// 某个任务持锁时 panic 不应让整个引擎从此瘫痪：锁中毒时照常使用里面的状态。
    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
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
        self.sync_availability();
        self.sync_input();
        self.sync_low_latency();
        self.sync_keep_awake();
        self.emit_view();
    }

    /// 只把当前状态推给界面，不重新同步控制器、可用状态和低延迟模式。
    /// 用于只有显示内容变化的场合（传输进度每秒推好几次，没必要每次都重建布局）。
    fn emit_view(&self) {
        let view = self.view();
        (self.inner.notify)(EngineEvent::State(Box::new(view)));
    }

    /// 本机现在是否接受 `peer` 的控制（见 [`accepts_control`]）。
    ///
    /// 不在配置里的设备（刚被移除、连接还没来得及关）一律不接受。
    fn accepting(st: &State, peer: &DeviceId) -> bool {
        st.config
            .peer(peer)
            .is_some_and(|p| accepts_control(st.config.settings.sharing_paused, p.paused))
    }

    fn controlled_by(&self) -> Option<DeviceId> {
        self.inner
            .controlled
            .lock()
            .expect("controlled")
            .as_ref()
            .map(|c| c.by.clone())
    }

    /// 暂停状态变化后：告诉支持的设备本机是否还接受它控制；正在控制本机、但已不被接受的
    /// （暂停了共享或暂停了它）立即结束，释放它按下的键。只在变化时发送。
    fn sync_availability(&self) {
        let controlled_by = self.controlled_by();
        let mut kick = None;
        {
            let mut st = self.lock();
            let decisions: Vec<(DeviceId, bool)> = st
                .sessions
                .keys()
                .map(|id| (id.clone(), Self::accepting(&st, id)))
                .collect();
            for (id, ok) in decisions {
                if !ok && controlled_by.as_ref() == Some(&id) {
                    kick = Some(id.clone());
                }
                if let Some(s) = st.sessions.get_mut(&id)
                    && s.acks
                    && s.accepting_sent != Some(ok)
                {
                    let _ = s.tx.send(Message::Availability { accepting: ok });
                    s.accepting_sent = Some(ok);
                }
            }
        }
        if let Some(by) = kick {
            info!(device = %short(&by), "本机已暂停共享，结束被控制");
            // release_controlled 会再次 emit，此时已不再被它控制，不会重复处理。
            self.release_controlled(Some(&by));
        }
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
            *cur = Some(want);
            match flow_platform::low_latency::set_active(want) {
                Ok(()) => info!(active = want, "低延迟模式"),
                Err(e) => {
                    warn!(error = %e, "低延迟模式没有生效");
                    drop(cur);
                    self.notice(format!(
                        "低延迟模式这次没有生效：{}。可以关掉再打开试试。",
                        platform_reason(&e)
                    ));
                }
            }
        }
    }

    /// 设置里开了「保持屏幕常亮」且有设备连着时，不让本机屏幕自动熄灭；否则恢复系统默认。
    fn sync_keep_awake(&self) {
        if !self.inner.input {
            return;
        }
        // 整个过程拿着这把锁：两处同时同步时，后算出来的状态一定后生效，
        // 不会出现"设备都断开了，屏幕却一直不熄"。锁顺序：`keep_awake_active` → `state`。
        let mut cur = self.inner.keep_awake_active.lock().expect("keep awake");
        let able = self
            .inner
            .keep_awake_problem
            .lock()
            .expect("keep awake problem")
            .is_none();
        let want = able && {
            let st = self.lock();
            st.config.settings.keep_awake && !st.sessions.is_empty()
        };
        if *cur == Some(want) {
            return;
        }
        // 失败也记下来，不在每次状态变化时重试；设备重新连上或重新打开开关时再试。
        *cur = Some(want);
        let result = flow_platform::power::set_keep_awake(want);
        drop(cur);
        match result {
            Ok(()) => info!(active = want, "保持屏幕常亮"),
            Err(e) => {
                warn!(error = %e, "无法保持屏幕常亮");
                // 只提示一次：这个桌面做不到的话，每次设备重连都会失败。重新打开开关后再提示。
                if want && !self.inner.keep_awake_warned.swap(true, Ordering::Relaxed) {
                    self.notice(format!(
                        "没能保持屏幕常亮：{}。可以在系统设置里把自动关闭显示器的时间调长。",
                        platform_reason(&e)
                    ));
                }
            }
        }
    }

    /// 启动时探测到做不到「保持屏幕常亮」的话定时重查：开机自启时流界可能比桌面的
    /// 屏幕保护服务先起来。查到能用了就不再查。
    async fn recheck_keep_awake(&self) {
        let known = |e: &Engine| {
            e.inner
                .keep_awake_problem
                .lock()
                .expect("keep awake problem")
                .clone()
        };
        if !self.inner.input || known(self).is_none() {
            return;
        }
        let now = tokio::task::spawn_blocking(flow_platform::power::keep_awake_problem)
            .await
            .unwrap_or(None);
        if now != known(self) {
            info!(problem = ?now, "保持屏幕常亮的可用性变化");
            *self
                .inner
                .keep_awake_problem
                .lock()
                .expect("keep awake problem") = now;
            self.emit();
        }
    }

    /// 别的电脑的键鼠正在操作本机：告诉系统"有人在用"。有的系统（macOS）不把注入的键鼠算作
    /// 用户操作，屏幕熄着不会亮、亮着也照样会到时间熄灭。键鼠刚进来和之后每次输入都会调用，
    /// 所以限制为每隔 [`REMOTE_ACTIVITY_EVERY`] 最多通知系统一次。
    fn remote_activity(&self) {
        if !self.inner.input {
            return;
        }
        // 加 1：0 留给"还没通知过"。
        let now = self.inner.started.elapsed().as_millis() as u64 + 1;
        let last = self.inner.remote_activity_at.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < REMOTE_ACTIVITY_EVERY.as_millis() as u64 {
            return;
        }
        self.inner.remote_activity_at.store(now, Ordering::Relaxed);
        if let Err(e) = flow_platform::power::input_activity() {
            debug!(error = %e, "通知系统有键鼠操作失败");
        }
    }

    /// 对方点了「唤醒屏幕」：点亮本机的屏幕。系统没有专门的接口时（Linux 的 Wayland），
    /// 改为把鼠标移到主屏中间，合成器会把它当作用户操作。
    fn wake_local(&self) {
        if !self.inner.input {
            return;
        }
        match flow_platform::power::wake_display() {
            Ok(()) => {}
            Err(PlatformError::Unsupported(_)) => {
                // 本机正在控制别的电脑时不动鼠标：有人正在用这台电脑，屏幕本来就亮着，
                // 凭空多出来的鼠标移动还会被当成本机输入发给对方。
                if self.controller().active_device().is_some() {
                    return;
                }
                let target = {
                    let st = self.lock();
                    st.local_displays
                        .iter()
                        .find(|d| d.primary)
                        .or(st.local_displays.first())
                        .map(|d| (d.local_id.clone(), d.rect.width / 2.0, d.rect.height / 2.0))
                };
                if let Some((display, x, y)) = target {
                    self.inject(|i| i.move_to(&display, x, y));
                }
            }
            Err(e) => warn!(error = %e, "点亮屏幕失败"),
        }
    }

    /// 请一台已连接的设备点亮屏幕。失败时返回给用户看的原因。
    pub fn wake_device(&self, device: &DeviceId) -> Result<(), String> {
        let st = self.lock();
        let Some(s) = st.sessions.get(device) else {
            return Err("这台设备现在没有连着，没法唤醒它的屏幕。".into());
        };
        if !s.wake {
            return Err("对方的流界版本太旧，不支持唤醒屏幕。请把对方升级到最新版本。".into());
        }
        s.tx.send(Message::Wake)
            .map_err(|_| "和这台设备的连接刚刚断开，请稍后再试。".to_string())
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
        self.inner
            .idle
            .store(st.sessions.is_empty(), Ordering::Relaxed);
        let local = self.local_id().clone();
        let outputs: HashMap<DeviceId, mpsc::UnboundedSender<Message>> = st
            .sessions
            .iter()
            .filter(|(id, _)| {
                st.config.peer(id).is_some_and(|p| !p.paused) && !st.remote_blocked.contains(*id)
            })
            .map(|(id, s)| (id.clone(), s.tx.clone()))
            .collect();
        let mut layout = st.layout(&local);
        layout
            .displays
            .retain(|d| d.id.device == local || outputs.contains_key(&d.id.device));
        // 平台层只在通往别的电脑的屏幕边上感知"推到边上"（Linux 的 Wayland 需要，其他系统无操作）。
        if self.inner.input {
            flow_platform::set_switch_edges(&layout.switch_edges(&local));
        }
        let cfg = ControllerConfig {
            layout,
            local_offset: st.config.local_offset,
            outputs,
            policy: switch_policy(
                &st.config.settings,
                st.locked || st.auto_lock.is_some(),
                flow_platform::limits(),
            ),
            mapping: st.config.settings.mapping,
            wheel_percent: st.config.settings.wheel_percent(),
            enabled: !st.config.settings.sharing_paused,
            acks: st
                .sessions
                .iter()
                .filter(|(_, s)| s.acks)
                .map(|(id, _)| id.clone())
                .collect(),
            hotkeys: usable_hotkeys(&st.config.settings.hotkeys, flow_platform::limits()),
            remote_keys: st
                .sessions
                .iter()
                .filter(|(_, s)| s.remote_actions)
                .map(|(id, _)| id.clone())
                .collect(),
        };
        self.controller().configure(cfg);
    }

    fn notice(&self, msg: impl Into<String>) {
        (self.inner.notify)(EngineEvent::Notice(msg.into()));
    }

    fn save(&self, st: &State) {
        if st.config_readonly {
            // 原配置文件读不了也备份不了，写入会把它覆盖掉。
            return;
        }
        if let Err(e) = st.config.save(&self.inner.data_dir) {
            self.notice(format!("保存设置失败：{e}"));
        }
    }

    /// 各项系统权限现在的状态。一秒内复用上一次查到的；`fresh` 时一定重新查（界面主动来问，
    /// 多半是用户刚从系统设置里授权回来）。
    fn permission_views(&self, fresh: bool) -> Vec<PermissionView> {
        let mut cache = self.inner.permissions.lock().expect("permissions");
        if let Some((at, views)) = cache.as_ref()
            && !fresh
            && at.elapsed() < Duration::from_secs(1)
        {
            return views.clone();
        }
        let perms = flow_platform::permissions();
        let views: Vec<PermissionView> = flow_platform::required_permissions()
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
            .collect();
        *cache = Some((Instant::now(), views.clone()));
        views
    }

    /// 界面主动来要状态（打开窗口、从系统设置回来）：权限重新查一遍。
    pub fn fresh_view(&self) -> StateView {
        self.permission_views(true);
        self.view()
    }

    /// 重新查权限并把状态推给界面（刚申请完权限）。
    pub fn refresh_view(&self) {
        self.permission_views(true);
        self.emit_view();
    }

    pub fn view(&self) -> StateView {
        // 先查权限再拿状态锁：查询是系统调用，别占着锁。
        let permissions = self.permission_views(false);
        let st = self.lock();
        let local = self.local_id().clone();
        let active = self.controller().active_device().cloned();
        let controlled_by = self
            .inner
            .controlled
            .lock()
            .expect("controlled")
            .as_ref()
            .map(|c| c.by.clone());
        // 键鼠被刚离开的设备带走了：它还连着、本机没在控制别的设备、也没在被控制时才算数。
        let input_at = self
            .inner
            .input_at
            .lock()
            .expect("input at")
            .clone()
            .filter(|id| {
                active.is_none() && controlled_by.is_none() && st.sessions.contains_key(id)
            });
        let mut devices = vec![DeviceView {
            id: local.clone(),
            name: self.name_of(&st),
            os: Os::current(),
            status: DeviceStatus::Local,
            manual: false,
            active: active.is_none(),
            has_input: active.is_none() && input_at.is_none(),
            paused: false,
            remote_paused: false,
            can_wake: false,
            locked: false,
            // 回到本机：只有本机正在控制别的设备时才有意义。
            can_switch: active.is_some() && !st.config.settings.sharing_paused,
            can_send_keys: false,
            latency_ms: None,
            latency_level: None,
            problem: None,
        }];
        // 本机正被别的电脑控制时，键鼠是它的：要切走得请它来切，它的版本得认得这个请求。
        let via_controller = controlled_by
            .as_ref()
            .map(|by| st.sessions.get(by).is_some_and(|s| s.remote_actions));
        let resuming = st.resuming();
        devices.extend(st.config.peers.iter().map(|p| {
            DeviceView {
                id: p.id.clone(),
                name: p.name.clone(),
                os: p.os,
                status: if st.sessions.contains_key(&p.id) {
                    DeviceStatus::Connected
                } else if st.connecting.contains(&p.id)
                || st.pairings.contains_key(&p.id)
                // 本机刚睡醒：两次重试之间也显示「连接中」，不要一会儿离线一会儿连接。
                || (resuming
                    && st.gone.get(&p.id) == Some(&Gone::LocalResume)
                    && !st.manual.contains(&p.id))
                {
                    DeviceStatus::Connecting
                } else {
                    DeviceStatus::Offline
                },
                manual: st.manual.contains(&p.id) && !st.sessions.contains_key(&p.id),
                active: active.as_ref() == Some(&p.id),
                has_input: active.as_ref() == Some(&p.id) || input_at.as_ref() == Some(&p.id),
                paused: p.paused,
                remote_paused: st.remote_blocked.contains(&p.id),
                can_wake: st.sessions.get(&p.id).is_some_and(|s| s.wake),
                locked: st.sessions.contains_key(&p.id) && st.remote_locked.contains(&p.id),
                can_switch: !st.config.settings.sharing_paused
                    && st.sessions.contains_key(&p.id)
                    && match via_controller {
                        // 请控制方切：切回它自己，或切到本机也连着的另一台。
                        Some(supported) => supported,
                        None => {
                            active.as_ref() != Some(&p.id)
                                && self.switch_blocker(&st, &p.id, active.is_some()).is_none()
                        }
                    },
                can_send_keys: st.sessions.get(&p.id).is_some_and(|s| s.remote_actions),
                latency_ms: st.sessions.get(&p.id).map(session_latency),
                latency_level: st
                    .sessions
                    .get(&p.id)
                    .map(|s| autolock::latency_level(session_latency(s))),
                problem: st.problem(&p.id, &p.name),
            }
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
                problem: st.failures.get(&f.device).map(ProblemView::from),
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
            // 还拿着 `state` 锁：序号的先后就是状态的先后。
            seq: self.inner.view_seq.fetch_add(1, Ordering::SeqCst) + 1,
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
            auto_lock: st.auto_lock.clone(),
            recent_apps: st.recent_apps.clone(),
            permissions,
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
            unsupported: flow_platform::limits().into(),
            keep_awake_problem: self
                .inner
                .keep_awake_problem
                .lock()
                .expect("keep awake problem")
                .clone(),
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
            config_warning: st.config_warning.clone(),
            auth_prompt: self.auth_prompt(),
            auth_denied: self.auth_denied(),
        }
    }

    fn auth_prompt(&self) -> Option<AuthPromptView> {
        if !self.inner.input {
            return None;
        }
        flow_platform::pending_authorization().map(|p| AuthPromptView {
            kind: auth_kind(p.kind),
            seconds_left: p.seconds_left,
        })
    }

    fn auth_denied(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.inner.capture_denied.load(Ordering::SeqCst) {
            out.push("capture");
        }
        if self.inner.inject_blocked.load(Ordering::SeqCst) {
            out.push("inject");
        }
        out
    }

    /// 用户点了「重新授权」：忘掉之前的拒绝，马上重新申请（需要的话系统会再弹窗）。
    pub fn retry_authorization(&self) {
        info!("用户要求重新授权");
        self.inner.capture_denied.store(false, Ordering::SeqCst);
        self.inner.inject_blocked.store(false, Ordering::SeqCst);
        *self.inner.capture_retry_at.lock().expect("retry") = None;
        self.ensure_capture();
        // 建一次注入器：上次没拿到授权的话，这一步会重新申请。可能要等一会儿，放到阻塞线程里。
        let engine = self.clone();
        self.inner.rt.spawn_blocking(move || {
            engine.inject(|_| Ok(()));
            engine.emit_view();
        });
        self.emit_view();
    }

    /// 用户看过配置损坏的提示了。
    pub fn dismiss_config_warning(&self) {
        self.lock().config_warning = None;
        self.emit();
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
        *self.inner.win_keys.lock().expect("win_keys") = settings.win_to_mac_keys();
        let mut st = self.lock();
        // 低延迟模式要先装好后台服务才能开，只能通过 `set_low_latency` 改；
        // 界面传来的是整份设置的快照，这一项以引擎里的为准。
        let low_latency = st.config.settings.low_latency;
        let was_paused = st.config.settings.sharing_paused;
        let was_syncing = st.config.settings.sync_layout;
        if settings.keep_awake != st.config.settings.keep_awake {
            self.inner.keep_awake_warned.store(false, Ordering::Relaxed);
        }
        st.config.settings = Settings {
            low_latency,
            // 界面传来的快捷键不一定合法（重复、不带修饰键）：整理后再用。
            hotkeys: settings.hotkeys.clone().sanitized(),
            auto_lock_apps: autolock::sanitize_apps(settings.auto_lock_apps.clone()),
            ..settings
        };
        // 关掉自动锁定、或把正在前台的应用移出名单：马上解除，不等下一次查前台应用。
        if st.refresh_auto_lock() {
            info!(reason = ?st.auto_lock, "自动锁定状态随设置变化");
        }
        if st.config.settings.sync_layout && !was_syncing {
            // 刚打开同步：把本机的布局发出去，和别的电脑比一比谁的新。
            st.send_layout(self.local_id(), None, None);
        }
        let paused = st.config.settings.sharing_paused;
        if paused != was_paused {
            // 与收发路径按同一顺序持锁，暂停返回后不能再写入远端剪贴板或启动新传输。
            let mut cb = self.inner.clipboard.lock().expect("clipboard");
            cb.sync.enabled = !paused && st.config.settings.clipboard;
            // 暂停期间复制的内容不在恢复时补发。
            cb.last_count = cb.access.as_mut().map(|a| a.change_count());
            if paused {
                let mut f = self.inner.files.lock().expect("files");
                f.epoch += 1;
                f.book.clear();
                f.unoffered = None;
                f.transfers.cancel_all();
                if let Some((offer, _)) = f.local.take() {
                    for s in st.sessions.values().filter(|s| s.files) {
                        let _ =
                            s.tx.send(Message::FileOffer(FileOffer::withdrawn(offer.id)));
                    }
                }
            }
        }
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

    /// 定时查到的本机前台应用（`None`：查不到）。记进「最近在前台的应用」，并重新判定自动锁定。
    ///
    /// 键鼠正在别的电脑上时不改变自动锁定的状态：这时锁上会把正在用的键鼠拽回本机。
    /// 等键鼠回到本机后的下一次查询再生效。
    fn on_foreground(&self, foreground: Option<Foreground>) {
        let mut st = self.lock();
        let recent_changed = foreground
            .as_ref()
            .is_some_and(|f| autolock::remember_app(&mut st.recent_apps, &f.app, OWN_APP));
        st.foreground = foreground;
        // 锁顺序 state → controller。
        let on_remote = self.controller().active_device().is_some();
        let lock_changed = !on_remote && st.refresh_auto_lock();
        if lock_changed {
            info!(reason = ?st.auto_lock, "自动锁定状态变化");
        }
        drop(st);
        if lock_changed {
            self.emit();
        } else if recent_changed {
            self.emit_view();
        }
    }

    /// 各设备的延迟和上次推给界面的相比有明显变化时，重新推一次界面。
    fn refresh_latency(&self) {
        let changed = {
            let mut st = self.lock();
            let now: HashMap<DeviceId, u32> = st
                .sessions
                .iter()
                .map(|(id, s)| (id.clone(), session_latency(s)))
                .collect();
            let changed = now.len() != st.latency_shown.len()
                || now.iter().any(|(id, ms)| {
                    autolock::latency_changed(st.latency_shown.get(id).copied(), Some(*ms))
                });
            if changed {
                st.latency_shown = now;
            }
            changed
        };
        if changed {
            self.emit_view();
        }
    }

    /// 设置页正在录制快捷键：期间不响应任何快捷键，按下的键原样交给界面。
    pub fn set_hotkeys_suspended(&self, suspended: bool) {
        self.controller().set_hotkeys_suspended(suspended);
    }

    /// 把键鼠切到指定的设备（托盘菜单、设备详情里的按钮）。`id` 是本机时回到本机。
    /// 失败时返回给用户看的原因。
    ///
    /// 本机正被别的电脑控制时（用户是用那台电脑的键鼠点的这个菜单），键鼠不归本机管：
    /// 发 `SwitchTo` 请控制方来切。否则本机就是键鼠所在的电脑，直接让控制器切过去。
    pub fn switch_to(&self, id: &DeviceId) -> Result<(), String> {
        let local = self.local_id().clone();
        let name_of = |st: &State, id: &DeviceId| {
            st.config
                .peer(id)
                .map(|p| p.name.clone())
                .unwrap_or_default()
        };
        if let Some(by) = self.controlled_by() {
            if id == &local {
                // 键鼠已经在这台电脑上了。
                return Ok(());
            }
            let st = self.lock();
            let by_name = name_of(&st, &by);
            let Some(s) = st.sessions.get(&by) else {
                return Err(format!("和「{by_name}」的连接刚刚断开，请稍后再试。"));
            };
            if !s.remote_actions {
                return Err(format!(
                    "现在是「{by_name}」的键鼠在操作这台电脑，它的流界版本太旧，不能从这里让键鼠切走。请把鼠标推过屏幕边，或把「{by_name}」的流界升级到 0.2.4 以上。"
                ));
            }
            return s
                .tx
                .send(Message::SwitchTo { device: id.clone() })
                .map_err(|_| format!("和「{by_name}」的连接刚刚断开，请稍后再试。"));
        }
        if id == &local {
            self.controller().switch_to_device(&local);
            return Ok(());
        }
        let on_remote = self.controller().active_device().is_some();
        if let Some(reason) = self.switch_blocker(&self.lock(), id, on_remote) {
            return Err(reason);
        }
        if self.controller().switch_to_device(id) {
            Ok(())
        } else {
            Err("现在切不过去，请稍后再试。".into())
        }
    }

    /// 命令行 `flowacross --switch <目标>`（由已经在运行的流界执行）。目标可以是：
    /// `left` / `right`（布局里左边、右边最近的一台，也认 `左` / `右`）、`home`（回到这台电脑，
    /// 也认 `local`、`本机`）、设备名称（不分大小写）或设备 ID 的前几位（至少 6 位）。
    /// 失败时返回给用户看的原因。
    pub fn switch_command(&self, target: &str) -> Result<(), String> {
        let target = target.trim();
        let local = self.local_id().clone();
        let id = match target.to_lowercase().as_str() {
            "home" | "local" | "本机" => local,
            dir @ ("left" | "right" | "左" | "右") => {
                let right = matches!(dir, "right" | "右");
                // 本机正被别的电脑控制时，按本机在布局里的位置找（键鼠现在就在本机的屏幕上）。
                self.controller().neighbor(right).ok_or_else(|| {
                    format!(
                        "{}边没有可以切过去的电脑。",
                        if right { "右" } else { "左" }
                    )
                })?
            }
            wanted => {
                let st = self.lock();
                let mut hits = st.config.peers.iter().filter(|p| {
                    p.name.to_lowercase() == wanted
                        || (wanted.len() >= 6 && p.id.0.starts_with(wanted))
                });
                match (hits.next(), hits.next()) {
                    (Some(p), None) => p.id.clone(),
                    (Some(_), Some(_)) => {
                        return Err(format!(
                            "有不止一台电脑叫「{target}」，请改用设备 ID 的前几位。"
                        ));
                    }
                    _ => {
                        return Err(format!(
                            "没有叫「{target}」的电脑。可以用 left、right、home，或设备列表里的名称。"
                        ));
                    }
                }
            }
        };
        self.switch_to(&id)
    }

    /// 本机是键鼠所在的电脑时，为什么现在不能把键鼠切到 `id`（一台已配对的设备）；能切时返回
    /// `None`。界面上按钮和托盘菜单能不能点（`DeviceView::can_switch`）和真去切时的检查共用它，
    /// 不会出现"能点、一点就报错"。`on_remote`：本机的键鼠现在已经在别的电脑上。
    fn switch_blocker(&self, st: &State, id: &DeviceId, on_remote: bool) -> Option<String> {
        let name = st
            .config
            .peer(id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        if st.config.settings.sharing_paused {
            return Some("共享已暂停，先恢复共享再切换。".into());
        }
        if st.locked {
            return Some("键鼠已锁定在本机，先解除锁定再切换。".into());
        }
        match &st.auto_lock {
            Some(AutoLock::App(app)) => {
                return Some(format!(
                    "「{app}」在前台，键鼠已自动锁定在本机。切到别的应用，或在 设置 › 跨屏与键鼠 里把它移出名单。"
                ));
            }
            Some(AutoLock::Fullscreen(app)) => {
                return Some(format!(
                    "「{app}」正全屏显示，键鼠已自动锁定在本机。退出全屏，或在 设置 › 跨屏与键鼠 里关掉「全屏时锁定在本机」。"
                ));
            }
            None => {}
        }
        let Some(peer) = st.config.peer(id) else {
            return Some("找不到这台设备。".into());
        };
        if !st.sessions.contains_key(id) {
            return Some(format!("「{name}」现在没有连着，切不过去。"));
        }
        if peer.paused {
            return Some(format!("「{name}」已被暂停，先在设备详情里恢复它。"));
        }
        if st.remote_blocked.contains(id) {
            return Some(format!("「{name}」暂停了共享，对方恢复后才能切过去。"));
        }
        if peer.offset.is_none() || peer.displays.is_empty() {
            return Some(format!("还没有收到「{name}」的屏幕信息，稍等几秒再试。"));
        }
        if self.inner.input {
            // 没有键鼠捕获就切过去的话，本机的光标被锁住、按键却到不了对方，也切不回来。
            if !self.inner.capture_on.load(Ordering::SeqCst) {
                return Some(
                    "键鼠捕获还没有启动（多半是缺少系统权限），现在切不过去。请到 设置 › 系统权限 里检查。"
                        .into(),
                );
            }
            // GNOME / KDE 的 Wayland 上只有光标撞到屏幕边时合成器才肯交出键鼠。
            if flow_platform::limits().commanded_switch && !on_remote {
                return Some(
                    "这个桌面（GNOME / KDE 的 Wayland）只能把鼠标推过屏幕边来切换；键鼠已经在别的电脑上时才能用这个菜单。"
                        .into(),
                );
            }
        }
        None
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
        // 用户动了布局：记一次修改，让别的电脑跟着变。
        st.config.layout_rev += 1;
        st.config.layout_by = Some(local.clone());
        st.send_layout(&local, None, None);
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

    /// 重试也不会成功（版本不兼容、对方已不信任本机）：停止自动重连，等用户处理后手动连接。
    /// 否则同一条提示会随着每次自动重试反复弹出，最后再弹一条「已暂停自动重试」。
    fn stop_auto_reconnect(&self, id: &DeviceId) {
        let mut st = self.lock();
        st.backoff.remove(id);
        st.manual.insert(id.clone());
    }

    /// 移除已配对设备：双方都会删除对方的可信记录。
    pub fn remove_device(&self, id: &DeviceId) {
        let session = {
            let mut st = self.lock();
            st.config.peers.retain(|p| &p.id != id);
            st.config.settings.hotkeys.forget_device(id);
            st.backoff.remove(id);
            st.manual.remove(id);
            st.remote_blocked.remove(id);
            st.remote_locked.remove(id);
            st.lock_warned.remove(id);
            st.failures.remove(id);
            st.gone.remove(id);
            st.quiet_retry.remove(id);
            self.save(&st);
            st.sessions.remove(id)
        };
        // 会话已经从表里拿掉，它结束时不会再清理，这里把它等待接收的文件清单一并撤掉。
        self.inner.files.lock().expect("files").book.clear_from(id);
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
                        // 重新出现的设备立刻重试，不等退避（不重置次数，也不恢复已停止的自动重连）。
                        if let Some(b) = st.backoff.get_mut(&f.device) {
                            b.next = Instant::now();
                        }
                        // mDNS 会周期性重复通告：内容没变就不用再推一遍界面。
                        if st.nearby.get(&f.device) == Some(&f) {
                            continue;
                        }
                        // 局域网里任何人都能广播任意多台"设备"：没配对过的最多记这么多。
                        if st.nearby.len() >= MAX_NEARBY
                            && !st.nearby.contains_key(&f.device)
                            && !st.config.is_trusted(&f.device)
                        {
                            continue;
                        }
                        debug!(device = %short(&f.device), name = %f.name, addrs = ?f.addrs, "发现设备");
                        st.nearby.insert(f.device.clone(), f);
                    }
                    DiscoveryEvent::Lost(id) => {
                        debug!(device = %short(&id), "设备从局域网消失");
                        st.nearby.remove(&id);
                        // 没配对过的设备不在别处显示，配对失败的说明跟着它一起消失。
                        if !st.config.is_trusted(&id) {
                            st.failures.remove(&id);
                        }
                    }
                }
            }
            // 附近的设备只影响显示，不用重新同步控制器和可用状态。
            self.emit_view();
        }
    }

    async fn accept_loop(self) {
        while let Some(incoming) = self.inner.endpoint.accept().await {
            // 任何人都能连进来：限制同时处在握手 / 配对阶段的入站连接数，防止耗尽内存。
            let slot = HandshakeSlot::take(&self.inner.unauthenticated);
            let Some(slot) = slot else {
                debug!(from = %incoming.remote_address(), "未认证的连接过多，拒绝");
                incoming.refuse();
                continue;
            };
            let engine = self.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        let from = conn.remote_address();
                        debug!(%from, "收到连接");
                        if let Err(e) = engine.run_link(conn, false, false, Some(slot)).await {
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
        let mut resume =
            ResumeDetector::new(TICK.as_millis() as u64, RESUME_SLACK.as_millis() as u64);
        // 连续几次查不到锁屏状态（这个系统或桌面不支持）就不再查。
        let mut lock_unknown = 0u8;
        loop {
            tokio::time::sleep(TICK).await;
            if self.stopped() {
                // 已经关闭（退出，或更新安装失败后等用户重新打开）：端点关了，再重连只会刷失败提示。
                return;
            }
            tick += 1;
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if let Some(slept) = resume.tick(wall) {
                self.on_resume(Duration::from_millis(slept));
            }
            // 显示器每 6 秒看一次有没有变（Linux 上每看一次要连一次 X 服务 / 合成器）；没有设备连着时
            // 没人等着这份信息，半分钟一次。
            let idle = self.inner.idle.load(Ordering::Relaxed);
            let every = if idle { 15 } else { 3 };
            if tick % every == 0 {
                self.refresh_displays();
            }
            if tick % 3 == 0 {
                let connected = !self.lock().sessions.is_empty();
                if !connected {
                    // 没有设备连着时不查（Linux 上每查一次要连一次系统总线），也不留着旧结果：
                    // 下次有设备连上时重新查。
                    self.lock().local_locked = None;
                } else if self.inner.input && lock_unknown < 3 {
                    // Linux 上要连一次系统总线，放到阻塞线程里。
                    let locked = tokio::task::spawn_blocking(flow_platform::power::screen_locked)
                        .await
                        .unwrap_or(None);
                    lock_unknown = if locked.is_some() {
                        0
                    } else {
                        lock_unknown + 1
                    };
                    // 查不到了就别让对方一直以为本机锁着。
                    self.set_local_locked(if lock_unknown >= 3 {
                        Some(false)
                    } else {
                        locked
                    });
                }
            }
            if tick % 15 == 0 {
                self.recheck_keep_awake().await;
            }
            self.refresh_latency();
            // 前台应用：自动锁定打开着，或有设备连着（给设置页攒「最近在前台的应用」）时才查。
            let watch = {
                let st = self.lock();
                self.inner.input
                    && !flow_platform::limits().foreground_app
                    && (st.auto_lock_wanted() || !st.sessions.is_empty())
            };
            if watch {
                // macOS 上要问一次前台应用、Linux 上要连一次 X 服务，放到阻塞线程里。
                let foreground = tokio::task::spawn_blocking(flow_platform::foreground)
                    .await
                    .unwrap_or(None);
                self.on_foreground(foreground);
            }
            self.ensure_capture();
            self.stop_idle_capture();
            // 授权弹窗出现 / 消失、授权被拒绝：告诉界面（倒计时由界面自己往下数）。
            let auth = (self.auth_prompt().map(|p| p.kind), self.auth_denied());
            let auth_changed = {
                let mut shown = self.inner.auth_shown.lock().expect("auth shown");
                let changed = *shown != auth;
                *shown = auth;
                changed
            };
            if auth_changed {
                self.emit_view();
            }
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
                        // 醒来后顺带试的那一轮（睡前就没连着）不提示。
                        if !st.quiet_retry.remove(&id) {
                            gave_up.push(id);
                        }
                        continue;
                    }
                    b.attempts += 1;
                    b.next = now + b.delay;
                    b.delay = (b.delay * 2).min(MAX_BACKOFF);
                    st.connecting.insert(id.clone());
                    out.push((id, addrs));
                }
                let names: Vec<(String, Option<String>)> = gave_up
                    .iter()
                    .filter_map(|id| st.config.peer(id).map(|p| p.name.clone()).map(|n| (id, n)))
                    .map(|(id, name)| {
                        let why = st.problem(id, &name).map(|p| p.summary);
                        (name, why)
                    })
                    .collect();
                (out, names)
            };
            let (targets, gave_up) = targets;
            for (name, why) in &gave_up {
                info!(%name, "自动重连次数用完，停止自动连接");
                let why = why
                    .clone()
                    .unwrap_or_else(|| format!("与「{name}」的连接尚未恢复。"));
                self.notice(format!(
                    "{why}\n已暂停自动重试：处理好之后在设备详情里点「连接」；对方的流界重新打开时也会自己连回来。排查办法见设备详情。"
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

    /// 这台电脑刚从睡眠中醒来（`slept`：停了多久）。睡着的这段时间对方早就判定本机离线了，
    /// 本机手里的连接也都没用了：先把键鼠还给本机、释放按键，关掉旧连接，然后马上重连——
    /// 不等这些连接自己超时，也不受"自动重连已停止"的限制。
    fn on_resume(&self, slept: Duration) {
        info!(secs = slept.as_secs(), "系统从睡眠中醒来，重新连接");
        self.controller().force_local();
        self.release_controlled(None);
        let (conns, any) = {
            let mut st = self.lock();
            let now = Instant::now();
            let ids: Vec<DeviceId> = st.config.peers.iter().map(|p| p.id.clone()).collect();
            st.manual.clear();
            st.resume_until = Some(now + RESUME_WINDOW);
            for id in &ids {
                if st.sessions.contains_key(id) {
                    st.gone.insert(id.clone(), Gone::LocalResume);
                } else {
                    // 睡前就没连着：再试一轮，但试不通时不打扰。
                    st.quiet_retry.insert(id.clone());
                }
                st.failures.remove(id);
                // 两边都立刻重试：只有醒来的这台知道该重连了。
                st.backoff.insert(
                    id.clone(),
                    Backoff {
                        next: now,
                        delay: Duration::from_secs(2),
                        attempts: 0,
                    },
                );
            }
            let conns: Vec<quinn::Connection> =
                st.sessions.values().map(|s| s.conn.clone()).collect();
            // 睡前有连着的设备才提示：带着一台长期不在线的配对设备的笔记本，不该每次开盖都弹。
            let any = !conns.is_empty();
            (conns, any)
        };
        for c in conns {
            c.close(0u32.into(), b"resume");
        }
        if any {
            self.notice("这台电脑刚从睡眠中醒来，正在重新连接其他电脑…");
        }
        self.emit();
    }

    /// 记下本机是否停在锁屏界面；变了就告诉认得这条消息的设备。
    fn set_local_locked(&self, locked: Option<bool>) {
        let Some(locked) = locked else {
            return;
        };
        let mut st = self.lock();
        if st.local_locked == Some(locked) {
            return;
        }
        // 第一次查到"没锁"不用说：对方默认就当本机没锁。
        let announce = locked || st.local_locked == Some(true);
        info!(locked, "本机锁屏状态");
        st.local_locked = Some(locked);
        if announce {
            for s in st.sessions.values().filter(|s| s.remote_actions) {
                let _ = s.tx.send(Message::Status { locked });
            }
        }
    }

    /// 别的电脑发来它眼里的布局。比本机手里的新就照着摆（换算到本机的坐标系，本机自己不动），
    /// 再转给其他电脑；比本机的旧就把本机的回给它。
    fn on_remote_layout(&self, peer: &DeviceId, theirs: SharedLayout) {
        let local = self.local_id().clone();
        let mut st = self.lock();
        if !st.config.settings.sync_layout || !st.config.is_trusted(peer) || !theirs.is_sane() {
            return;
        }
        let mine_by = st.config.layout_by.clone().unwrap_or_else(|| local.clone());
        if !theirs.is_newer_than(st.config.layout_rev, &mine_by) {
            // 本机的更新（不是同一份）：回给它，让它跟上。
            if (theirs.rev, &theirs.by) != (st.config.layout_rev, &mine_by) {
                st.send_layout(&local, Some(peer), None);
            }
            return;
        }
        let Some(placed) = theirs.rebased(&local, st.config.local_offset) else {
            // 它的布局里还没有本机：等它收到本机的屏幕信息后会再发。
            return;
        };
        // 先在一份副本上摆，摆出来有重叠（它不认识的设备挡在那里）就整份不用。
        let mut next = st.config.peers.clone();
        for p in &mut next {
            if let Some((_, offset)) = placed.iter().find(|(id, _)| id == &p.id) {
                p.offset = Some(*offset);
            }
        }
        let mut displays: Vec<Display> = st
            .local_displays
            .iter()
            .map(|d| to_display(&local, d, st.config.local_offset))
            .collect();
        for p in &next {
            if let Some(off) = p.offset {
                displays.extend(p.displays.iter().map(|d| to_display(&p.id, d, off)));
            }
        }
        if let Err(e) = Layout::new(displays).validate() {
            info!(device = %short(peer), error = %e, "对方发来的布局在本机摆不下，保持原样");
            return;
        }
        info!(device = %short(peer), rev = theirs.rev, "采用对方的设备布局");
        st.config.peers = next;
        st.config.layout_rev = theirs.rev;
        st.config.layout_by = Some(theirs.by.clone());
        self.save(&st);
        // 转给其他电脑（它们之间可能没有直接配对）。同一份布局再传回来时不算新，不会转圈。
        st.send_layout(&local, None, Some(peer));
        drop(st);
        self.emit();
    }

    /// 对方报告它是否停在锁屏界面。本机的键鼠正在它那边时马上提示一次。
    fn on_remote_status(&self, peer: &DeviceId, locked: bool) {
        let on_it = self.controller().active_device() == Some(peer);
        let warn = {
            let mut st = self.lock();
            let changed = if locked {
                st.remote_locked.insert(peer.clone())
            } else {
                st.lock_warned.remove(peer);
                st.remote_locked.remove(peer)
            };
            if !changed {
                return;
            }
            info!(device = %short(peer), locked, "对方的锁屏状态变化");
            (locked && on_it && st.lock_warned.insert(peer.clone()))
                .then(|| st.config.peer(peer).map(|p| (p.name.clone(), p.os)))
                .flatten()
        };
        if let Some((name, os)) = warn {
            self.notice(locked_notice(&name, os));
        }
        self.emit();
    }

    /// 记下一次连接失败的原因和排查步骤，返回给调用方用来提示。
    fn record_failure(&self, peer: &DeviceId, addrs: &[SocketAddr], fault: Fault) -> Diagnosis {
        let nets = local_nets();
        let mut st = self.lock();
        let found = st.nearby.get(peer);
        let (name, os) = match st.config.peer(peer) {
            Some(p) => (p.name.clone(), p.os),
            None => found
                .map(|f| (f.name.clone(), os_from_tag(&f.os)))
                .unwrap_or((String::new(), Os::Other)),
        };
        let ips: Vec<std::net::IpAddr> = addrs.iter().map(|a| a.ip()).collect();
        let d = diagnose::diagnose(&ConnectFacts {
            name: &name,
            peer_os: os,
            local_os: Os::current(),
            fault,
            discovered: found.is_some(),
            peer_addrs: &ips,
            local_nets: &nets,
            port: addrs
                .first()
                .map(|a| a.port())
                .unwrap_or(flow_net::DEFAULT_PORT),
        });
        st.failures.insert(peer.clone(), d.clone());
        d
    }

    fn refresh_displays(&self) {
        // 显示器名称要在主线程读取；这里先安排刷新缓存，新名称在下一轮生效。
        if let Some(run) = &self.inner.main_thread {
            run(Box::new(flow_platform::refresh_display_names));
        }
        let Ok(now) = flow_platform::display_provider().displays() else {
            return;
        };
        // 绝大多数时候列表没变：这时不用查熄屏状态（Linux 上查一次要多连一次 X 服务）。
        if self.lock().local_displays == now {
            return;
        }
        let asleep = flow_platform::power::displays_asleep();
        let changed = {
            let mut st = self.lock();
            // 熄屏期间系统报的列表会变少甚至变空，照发的话别的电脑就切不过来了。
            let now = settle_displays(&st.local_displays, now, asleep);
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
        if let Err(e) = result {
            // 自动重连失败也记下原因，显示在设备详情里；只有用户自己点的才弹提示。
            let d = self.record_failure(&peer, &addrs, e.fault());
            if user {
                if !pair {
                    // 手动连接失败：仍然停在"需要手动连接"，不恢复自动重连。
                    self.lock().manual.insert(peer.clone());
                }
                let first = d
                    .steps
                    .first()
                    .map(|s| format!("\n{s}"))
                    .unwrap_or_default();
                let more = if pair {
                    "更多排查办法见「添加设备」窗口里这台电脑下面的说明"
                } else {
                    "更多排查办法见设备详情"
                };
                self.notice(format!(
                    "{}{first}\n{more}；详细原因可在「设置 › 关于 › 打开日志文件夹」查看。",
                    d.summary
                ));
            }
        }
        self.emit();
    }

    async fn try_connect(
        &self,
        peer: &DeviceId,
        addrs: &[SocketAddr],
        pair: bool,
    ) -> Result<(), ConnectFailure> {
        let mut errors = Vec::new();
        let mut kind = ConnectFailureKind::Address;
        // 和本机同一个子网的地址先试：对方常常还广播着 VPN、虚拟机网卡的地址，每个都要等到超时。
        let addrs = diagnose::order_addrs(addrs, &local_nets());
        for addr in &addrs {
            let connecting = match self.inner.endpoint.connect(*addr, "flowacross.local") {
                Ok(c) => c,
                Err(e) => {
                    warn!(%addr, error = %e, "无法使用连接地址");
                    errors.push(format!("{addr}：{e}"));
                    continue;
                }
            };
            let error = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
                Ok(Ok(conn)) => {
                    // mDNS 广播可以被伪造，必须核对证书指纹就是我们要连的那台设备。
                    match peer_fingerprint(&conn) {
                        Ok(fp) if &device_id(&fp) == peer => {
                            return self.run_link(conn, true, pair, None).await.map_err(
                                |details| ConnectFailure {
                                    kind: ConnectFailureKind::Handshake,
                                    details,
                                },
                            );
                        }
                        _ => {
                            conn.close(0u32.into(), b"unexpected peer");
                            kind = ConnectFailureKind::Identity;
                            "对方的身份与发现的设备不一致".to_owned()
                        }
                    }
                }
                Ok(Err(e)) => {
                    let failure = if matches!(e, quinn::ConnectionError::TimedOut) {
                        ConnectFailureKind::Timeout
                    } else {
                        ConnectFailureKind::Transport
                    };
                    kind = kind.max(failure);
                    format!("连接失败（{e}）")
                }
                Err(_) => {
                    kind = kind.max(ConnectFailureKind::Timeout);
                    "连接超时（UDP 握手未完成）".to_owned()
                }
            };
            warn!(%addr, %error, "连接地址尝试失败");
            errors.push(format!("{addr}：{error}"));
        }
        let details = if errors.is_empty() {
            "没有可用的网络地址".to_owned()
        } else {
            errors.join("；")
        };
        Err(ConnectFailure { kind, details })
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
    /// `slot`：入站连接占用的"未认证"名额，进入会话时归还（见 [`HandshakeSlot`]）。
    async fn run_link(
        &self,
        conn: quinn::Connection,
        initiator: bool,
        pair: bool,
        slot: Option<HandshakeSlot>,
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
        // 对方还没通过认证，名称和版本号可以是任意内容：截短、去掉控制字符后再用于日志和界面。
        let name = Some(clean_name(&name))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "未命名设备".to_owned());
        let app_version = clean_name(&app_version);
        if protocol != PROTOCOL_VERSION {
            let _ = link
                .sender
                .send(&Message::Bye {
                    reason: ByeReason::VersionMismatch,
                })
                .await;
            linger_close(&link.conn, b"version").await;
            // 对方还没通过认证：只有本机主动去连、或它是已配对的设备时才提示，
            // 否则局域网里任何人都能反复连过来刷提示。
            if initiator || we_trust {
                if we_trust {
                    self.stop_auto_reconnect(&peer);
                }
                let d = diagnose::version_mismatch(&name, &app_version, env!("CARGO_PKG_VERSION"));
                self.notice(format!("{}\n{}", d.summary, d.steps.join("\n")));
                self.lock().failures.insert(peer.clone(), d);
            }
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
        // 旧版本只会用"两个指纹算配对码"的旧方式，可以被中间人凑出相同的码，不再和它们配对。
        // 已经配对过的旧版本设备照常连接。单元测试里两端都是当前版本，直接视为支持。
        let secure = peer_supports_secure(&peer_info.version) || cfg!(test);
        let too_old = || {
            format!(
                "「{name}」的流界版本过旧（{}），不支持安全配对。请先把两台电脑都更新到最新版本，再重新配对。",
                peer_info.version
            )
        };
        if we_trust && trusts_you {
            drop(slot);
            self.run_session(link, peer_info, initiator).await;
        } else if initiator && pair {
            if !secure {
                let _ = link
                    .sender
                    .send(&Message::Bye {
                        reason: ByeReason::VersionMismatch,
                    })
                    .await;
                linger_close(&link.conn, b"version").await;
                self.notice(too_old());
            } else if link.sender.send(&Message::PairRequest).await.is_ok() {
                self.run_pairing(link, peer_info, true, None).await;
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
                self.stop_auto_reconnect(&peer_info.id);
                self.notice(format!("「{name}」已不再信任本机，需要重新配对。"));
            }
        } else {
            match tokio::time::timeout(Duration::from_secs(30), link.receiver.recv()).await {
                Ok(Ok(Some(Message::PairRequest))) => {
                    // 本机没有发起，任何局域网里的主机都能触发这个弹窗：限制频率。
                    let now = self.now_ms();
                    let allowed = self
                        .inner
                        .pair_requests
                        .lock()
                        .expect("pair_requests")
                        .allow(now);
                    if !allowed {
                        warn!(device = %short(&peer_info.id), "配对请求过于频繁，已忽略");
                        let _ = link.sender.send(&Message::PairReject).await;
                        linger_close(&link.conn, b"too many").await;
                    } else if !secure {
                        let _ = link.sender.send(&Message::PairReject).await;
                        linger_close(&link.conn, b"version").await;
                        self.notice(too_old());
                    } else {
                        self.run_pairing(link, peer_info, false, slot).await
                    }
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

    /// 配对前交换随机数（发起方先承诺），算出两边一致的配对码。见 [`flow_core::pairing`]。
    async fn exchange_pairing_nonces(
        &self,
        link: &mut Link,
        outgoing: bool,
        name: &str,
    ) -> Result<PairingCode, String> {
        let me = self.inner.identity.fingerprint;
        let them = link.peer_fingerprint;
        let binding = channel_binding(&link.conn).map_err(|e| e.to_string())?;
        let lost = || "配对过程中连接断开了，请重试。".to_owned();
        let recv = async |link: &mut Link| match link.receiver.recv().await {
            Ok(Some(Message::PairReject)) => Err(format!("「{name}」拒绝了配对。")),
            Ok(Some(m)) => Ok(m),
            _ => Err(lost()),
        };
        let mine: Nonce = random_bytes().map_err(|e| e.to_string())?;
        if outgoing {
            link.sender
                .send(&Message::PairCommit(commitment(&me, &mine)))
                .await
                .map_err(|_| lost())?;
            let Message::PairNonce(theirs) = recv(link).await? else {
                return Err("对方的配对消息顺序不对，请重新配对。".into());
            };
            link.sender
                .send(&Message::PairNonce(mine))
                .await
                .map_err(|_| lost())?;
            Ok(sas_code(&me, &them, &mine, &theirs, &binding))
        } else {
            let Message::PairCommit(commit) = recv(link).await? else {
                return Err("对方的配对消息顺序不对，请重新配对。".into());
            };
            link.sender
                .send(&Message::PairNonce(mine))
                .await
                .map_err(|_| lost())?;
            let Message::PairNonce(theirs) = recv(link).await? else {
                return Err("对方的配对消息顺序不对，请重新配对。".into());
            };
            if !verify_commitment(&them, &theirs, &commit) {
                return Err(format!(
                    "与「{name}」配对时校验失败，可能有人在冒充对方。请确认两台电脑在可信的网络中，再重新配对。"
                ));
            }
            Ok(sas_code(&them, &me, &theirs, &mine, &binding))
        }
    }

    async fn run_pairing(
        &self,
        mut link: Link,
        info: PeerInfo,
        outgoing: bool,
        slot: Option<HandshakeSlot>,
    ) {
        let PeerInfo {
            id: peer, name, os, ..
        } = info.clone();
        let exchanged = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            self.exchange_pairing_nonces(&mut link, outgoing, &name),
        )
        .await
        .unwrap_or_else(|_| Err("配对超时，请重新发起。".into()));
        let code = match exchanged {
            Ok(code) => code,
            Err(msg) => {
                info!(device = %short(&peer), reason = %msg, "配对失败（交换随机数）");
                linger_close(&link.conn, b"pairing failed").await;
                // 对方没有通过校验时两边都要知道；本机是被动方的其他失败（比如对方取消）不打扰用户。
                if outgoing || msg.contains("冒充") {
                    self.notice(msg);
                }
                return;
            }
        };
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
            // 已经连上并开始配对了，上次没连上的说明不用再留着。
            st.failures.remove(&peer);
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
                drop(slot);
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
        // 双方互信，放开单帧上限。
        receiver.authenticated();
        // 对方会回复心跳回执（控制方据此快速发现断网），也要求我们回复。单元测试里视为支持。
        let acks = peer_supports_secure(&version) || cfg!(test);
        // 对方支持时，鼠标位置走数据报（网络停顿后直接跳到最新位置），其余消息走可靠流。
        let mut moves = MoveSender::new(conn.clone(), &version);
        info!(device = %short(&peer), datagram = moves.enabled(), "鼠标位置发送方式");
        // 单元测试里两端都是当前（未发布的）版本号，直接视为支持。
        let files = peer_supports_files(&version) || cfg!(test);
        let images = files && (peer_supports_images(&version) || cfg!(test));
        let wake = peer_supports_wake(&version) || cfg!(test);
        let remote_actions = peer_supports_remote_actions(&version) || cfg!(test);
        info!(device = %short(&peer), version = %version, files, images, "对方版本");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let initiator_id = if initiator {
            self.local_id().clone()
        } else {
            peer.clone()
        };
        {
            let mut st = self.lock();
            // 握手期间用户可能已经把它移除了（握手开始时读到的"信任"是旧的）：不能再建会话，
            // 否则它不出现在设备列表里，却还能操作本机。
            if !st.config.is_trusted(&peer) {
                drop(st);
                info!(device = %short(&peer), "握手完成时设备已被移除，关闭连接");
                conn.close(0u32.into(), b"removed");
                return;
            }
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
                    images,
                    acks,
                    wake,
                    remote_actions,
                    accepting_sent: None,
                },
            );
            st.backoff.remove(&peer);
            st.manual.remove(&peer);
            st.failures.remove(&peer);
            st.gone.remove(&peer);
            st.quiet_retry.remove(&peer);
            st.remote_locked.remove(&peer);
            st.lock_warned.remove(&peer);
            // 对方可能刚重启过，剪贴板序号从头开始（锁顺序 state → clipboard）。
            self.inner
                .clipboard
                .lock()
                .expect("clipboard")
                .sync
                .forget(&peer);
            if let Some(r) = st.config.peer_mut(&peer) {
                r.name = name;
                r.os = os;
                if initiator {
                    // 双栈端点会把 IPv4 显示为 ::ffff:x.x.x.x；保存原生 IPv4，
                    // 下次系统禁用 IPv6、回退到 IPv4 端点后仍能用缓存地址重连。
                    let remote = conn.remote_address();
                    let remote = match remote {
                        SocketAddr::V6(addr) => addr
                            .ip()
                            .to_ipv4_mapped()
                            .map(|ip| SocketAddr::new(ip.into(), addr.port()))
                            .unwrap_or(remote),
                        _ => remote,
                    };
                    r.last_addrs = vec![remote];
                }
            }
            self.save(&st);
            let _ = tx.send(Message::Displays(st.local_displays.clone()));
            // 对一下布局：谁的新就用谁的（它在本机布局里还没有位置时，等收到它的屏幕信息再发）。
            st.send_layout(self.local_id(), Some(&peer), None);
            // 本机正停在锁屏界面的话，一连上就告诉对方。
            if remote_actions && st.local_locked == Some(true) {
                let _ = tx.send(Message::Status { locked: true });
            }
            // 连上之前本机已经复制了文件：补发清单。
            if files && st.files_allowed(&peer) {
                let mut f = self.inner.files.lock().expect("files");
                if let Some((offer, _)) = &f.local {
                    let _ = tx.send(offer.clone().into_message(images));
                } else if let Some((paths, epoch)) = f.unoffered.take() {
                    // 复制时还没有设备连着，清单没整理过：现在整理并发给所有能收的设备。
                    tokio::spawn(self.clone().offer_files(paths, epoch, false));
                }
            }
        }
        self.emit();
        // 有设备连上了，又需要捕获键鼠：上次没拿到授权的话重新申请。
        self.inner.capture_denied.store(false, Ordering::SeqCst);
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
            // 最近一次用数据报发出、还没在可靠流上确认过的鼠标位置。
            let mut last_datagram_move: Option<Message> = None;
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
                    if matches!(m, Message::MouseMove { .. }) {
                        if moves.try_send(&m) {
                            last_datagram_move = Some(m);
                            continue;
                        }
                        last_datagram_move = None;
                    } else if matches!(
                        m,
                        Message::Button { .. } | Message::Wheel { .. } | Message::Key { .. }
                    ) && let Some(pos) = last_datagram_move.take()
                    {
                        // 位置走数据报、点击和按键走可靠流，两条通道之间没有先后保证，数据报还可能丢。
                        // 在可靠流上补发一次最新位置，保证"先移到那里再点"。
                        if sender.send(&pos).await.is_err() {
                            return;
                        }
                    }
                    if sender.send(&m).await.is_err() {
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
                        // 旧版本在屏幕熄灭后会发来空列表：沿用原来的，否则就切不过去唤醒它了。
                        // 对端的数据不可信：带 NaN、离谱尺寸的显示器先丢掉（写进配置会让配置读不出来）。
                        r.displays = settle_displays(&r.displays, sane_displays(list), false);
                    }
                    let local = self.local_id().clone();
                    if st.place_new_peer(&local, &peer) {
                        // 第一次有了它的位置（默认放在右边）：把布局发给它。两边各自的默认摆法
                        // 是矛盾的（都把对方放在自己右边），比过之后统一成一份。
                        st.send_layout(&local, Some(&peer), None);
                    }
                    self.save(&st);
                    drop(st);
                    self.emit();
                }
                Ok(Some(Message::Profile { name })) => {
                    let name = clean_name(&name);
                    let mut st = self.lock();
                    if let Some(r) = st.config.peer_mut(&peer)
                        && !name.is_empty()
                    {
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
                            st.config.settings.hotkeys.forget_device(&peer);
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
                    | Message::ReleaseAll),
                )) => self.on_remote_input(&peer, os, m),
                Ok(Some(m @ Message::Heartbeat { seq })) => {
                    // 回执走同一条可靠流，控制方收不到就说明网络断了。
                    if acks {
                        let _ = tx.send(Message::HeartbeatAck { seq });
                    }
                    self.on_remote_input(&peer, os, m);
                }
                Ok(Some(Message::HeartbeatAck { .. })) => self.controller().on_ack(&peer),
                Ok(Some(Message::Availability { accepting })) => {
                    let changed = {
                        let mut st = self.lock();
                        if accepting {
                            st.remote_blocked.remove(&peer)
                        } else {
                            st.remote_blocked.insert(peer.clone())
                        }
                    };
                    if changed {
                        info!(device = %short(&peer), accepting, "对方的共享状态变化");
                        // 不再接受时，控制器会在同步配置时回到本机并发出 Lost。
                        self.emit();
                    }
                }
                Ok(Some(Message::Clipboard { origin, seq, text })) => {
                    self.on_remote_clipboard(&peer, &origin, seq, &text)
                }
                Ok(Some(Message::Wake)) => {
                    // 暂停共享或暂停了这台设备时，它不能操作本机，也不该能点亮本机的屏幕。
                    let ok = Self::accepting(&self.lock(), &peer);
                    info!(device = %short(&peer), accepted = ok, "对方请求点亮屏幕");
                    if ok {
                        self.wake_local();
                    }
                }
                Ok(Some(msg @ (Message::FileOffer(_) | Message::ImageOffer(_)))) => {
                    if let Some(offer) = FileOffer::from_message(msg) {
                        self.on_file_offer(&peer, offer);
                    }
                }
                Ok(Some(Message::KeyStroke { keys })) => self.on_keystroke(&peer, os, &keys),
                Ok(Some(Message::SwitchTo { device })) => self.on_switch_request(&peer, &device),
                Ok(Some(Message::Status { locked })) => self.on_remote_status(&peer, locked),
                Ok(Some(Message::Layout(layout))) => self.on_remote_layout(&peer, layout),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }

        writer.abort();
        datagrams.abort();
        streams.abort();
        self.release_controlled(Some(&peer));
        info!(device = %short(&peer), reason = ?conn.close_reason(), "会话结束");
        // 对方是怎么不见的：流界正常退出（它关闭端点时带着 "shutdown"），还是突然没了回应。
        let gone = match conn.close_reason() {
            Some(quinn::ConnectionError::ApplicationClosed(c))
                if c.reason.as_ref() == b"shutdown" =>
            {
                Some(Gone::Quit)
            }
            Some(quinn::ConnectionError::TimedOut) => Some(Gone::Silent),
            _ => None,
        };
        let current = {
            let mut st = self.lock();
            let current = st
                .sessions
                .get(&peer)
                .is_some_and(|s| s.conn.stable_id() == conn.stable_id());
            if current {
                st.sessions.remove(&peer);
                st.remote_blocked.remove(&peer);
                st.remote_locked.remove(&peer);
                st.lock_warned.remove(&peer);
                // 本机刚睡醒时是本机自己关的连接，原因已经记过了。
                if let Some(gone) = gone
                    && st.config.is_trusted(&peer)
                    && st.gone.get(&peer) != Some(&Gone::LocalResume)
                {
                    st.gone.insert(peer.clone(), gone);
                }
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

    /// `shutdown()` 之后为真：后台循环据此退出。
    fn stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// 没有设备连着时卸掉键鼠捕获：钩子 / 事件监听挂在系统每一个输入事件的路径上，
    /// 没有可切过去的设备就不该留着。有设备连上时 `ensure_capture` 会重新安装。
    fn stop_idle_capture(&self) {
        if !self.lock().sessions.is_empty() {
            return;
        }
        let taken = self.inner.capture.lock().expect("capture").take();
        if let Some(mut c) = taken {
            self.inner.capture_on.store(false, Ordering::SeqCst);
            self.controller().force_local();
            c.stop();
            info!("没有已连接的设备，键鼠捕获已停止");
        }
    }

    /// 有已连接的设备时启动键鼠捕获；失败（通常是缺少权限）只提示一次，之后定时重试。
    fn ensure_capture(&self) {
        if !self.inner.input || self.stopped() || self.lock().sessions.is_empty() {
            return;
        }
        // 捕获自己停了（授权弹窗后来被拒绝或超时、桌面把权限收回）：卸掉它，按启动失败处理。
        let dead = {
            let mut cap = self.inner.capture.lock().expect("capture");
            match cap.as_ref().and_then(|c| c.failure()) {
                Some(e) => cap.take().map(|c| (c, e)),
                None => None,
            }
        };
        if let Some((mut capture, e)) = dead {
            self.inner.capture_on.store(false, Ordering::SeqCst);
            // 先把键鼠还给本机，再提示。
            self.controller().force_local();
            capture.stop();
            warn!(error = %e, "键鼠捕获已停止");
            self.capture_failed(&e);
        }
        if self.inner.capture_denied.load(Ordering::SeqCst) {
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
        // 和 `Engine::controller()` 一样：某次持锁时 panic 不能让键鼠捕获从此失效，锁中毒时照常使用。
        let handler: flow_platform::CaptureHandler =
            Box::new(move |ev| ctl.lock().unwrap_or_else(|e| e.into_inner()).handle(ev));
        let mut capture = flow_platform::input_capture();
        match capture.start(handler) {
            Ok(()) => {
                info!("键鼠捕获已启动");
                self.inner.capture_warned.store(false, Ordering::Relaxed);
                *retry_at = None;
                *cap = Some(capture);
                self.inner.capture_on.store(true, Ordering::SeqCst);
            }
            Err(e) => {
                drop(retry_at);
                drop(cap);
                warn!(error = %e, "无法启动键鼠捕获");
                self.capture_failed(&e);
            }
        }
    }

    /// 键鼠捕获没启动起来，或者启动后自己停了。
    fn capture_failed(&self, e: &PlatformError) {
        match e {
            // 没拿到授权：每次都告诉用户（这是用户刚做的选择，或刚错过的弹窗），不自动重试。
            PlatformError::Denied(_) => {
                self.inner.capture_denied.store(true, Ordering::SeqCst);
                self.notice(capture_error_text(e));
            }
            other => {
                // 缺权限只是本进程内的查询，下个周期再查即可；系统接口本身失败则 30 秒后再试。
                if !matches!(other, PlatformError::PermissionDenied(_)) {
                    *self.inner.capture_retry_at.lock().expect("retry") =
                        Some(Instant::now() + Duration::from_secs(30));
                }
                if !self.inner.capture_warned.swap(true, Ordering::Relaxed) {
                    self.notice(capture_error_text(other));
                }
            }
        }
    }

    async fn control_event_loop(self, mut rx: mpsc::UnboundedReceiver<ControlEvent>) {
        let mut was_remote = false;
        while let Some(ev) = rx.recv().await {
            if let ControlEvent::Returned(at) = &ev {
                // 键鼠已经回到本机（前一个 `Changed` 处理过了），这里只负责浮光。
                self.glow_on_return(*at);
                continue;
            }
            // 本机有键鼠输入了，或者本机的键鼠切到了别处：不再认为键鼠在刚离开的那台设备上。
            self.set_input_at(None);
            if matches!(ev, ControlEvent::LocalInput) {
                // 只是「键鼠在这里」的标记变了。
                self.emit_view();
                continue;
            }
            if let ControlEvent::Lost(id) = &ev {
                // 用户自己移除或暂停了正在控制的设备：键鼠回到本机是预期的，不提示"连接中断"。
                let (by_user, name, refused) = {
                    let st = self.lock();
                    let peer = st.config.peer(id);
                    (
                        peer.is_none_or(|p| p.paused),
                        peer.map(|p| p.name.clone()).unwrap_or_default(),
                        st.remote_blocked.contains(id) && st.sessions.contains_key(id),
                    )
                };
                if !by_user {
                    self.notice(if refused {
                        format!("「{name}」暂停了共享，键鼠已回到本机。对方恢复后可以再切过去。")
                    } else {
                        format!("与「{name}」的连接中断，键鼠已回到本机。")
                    });
                }
            }
            let active = self.controller().active_device().cloned();
            // 切到了一台停在锁屏界面的电脑：提示一次（它解锁后再锁屏会再提示）。
            if let Some(id) = &active {
                let warn = {
                    let mut st = self.lock();
                    (st.remote_locked.contains(id) && st.lock_warned.insert(id.clone()))
                        .then(|| st.config.peer(id).map(|p| (p.name.clone(), p.os)))
                        .flatten()
                };
                if let Some((name, os)) = warn {
                    self.notice(locked_notice(&name, os));
                }
            }
            let remote = active.is_some();
            if remote {
                // 本机正被别的设备控制时，用户直接用本机键鼠切到了其他设备：本机已经不归对方控制了，
                // 释放对方按下的键并不再注入它的输入。对方收到本机的 Enter 后也会交出控制权。
                self.release_controlled(None);
            }
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
            // 没有设备连着时没有心跳要发、没有停留要计时、也没人控制本机：半秒看一眼就够了，
            // 不用每 40 毫秒醒一次。
            let idle = self.inner.idle.load(Ordering::Relaxed);
            tokio::time::sleep(if idle { IDLE_TICK } else { INPUT_TICK }).await;
            if self.stopped() {
                return;
            }
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

    /// 可以收发剪贴板的设备：已连接且没有被暂停。
    fn clipboard_peers(st: &State) -> Option<Vec<ClipPeer>> {
        if !st.config.settings.clipboard || st.config.settings.sharing_paused {
            return None;
        }
        let peers: Vec<_> = st
            .sessions
            .iter()
            .filter(|(id, _)| st.config.peer(id).is_some_and(|p| !p.paused))
            .map(|(id, s)| ClipPeer {
                tx: s.tx.clone(),
                files: s.files && st.files_allowed(id),
                images: s.images,
            })
            .collect();
        Some(peers)
    }

    /// 定时检查本机剪贴板：新的文本发给其他设备；复制了文件或图片（截图）则发清单。
    async fn clipboard_loop(self) {
        if !self.inner.input {
            return;
        }
        let mut clipboard_retry_at: Option<Instant> = None;
        let mut clipboard_warned = false;
        loop {
            tokio::time::sleep(CLIPBOARD_POLL).await;
            if self.stopped() {
                return;
            }
            // `None`：共享剪贴板关着或暂停了；空列表：开着但现在没有设备连着。
            let (epoch, withdrawn, files, image, msg, peers) = {
                let st = self.lock();
                let peers = Self::clipboard_peers(&st);
                let s = &st.config.settings;
                let files_on = s.clipboard && s.clipboard_files && !s.sharing_paused;
                let mut cb = self.inner.clipboard.lock().expect("clipboard");
                // 读剪贴板可能要等别的程序交出内容（Linux 上最多半秒，大图更久）：只留着剪贴板的锁，
                // 放掉 `state`，否则这期间被控制的键鼠事件、界面的查询全都卡着。读完后下面会按
                // 当时的状态再核对一遍。
                drop(st);
                cb.sync.enabled = peers.is_some();
                if cb.access.is_none() {
                    // 上次没打开的话隔半分钟再试（桌面的剪贴板服务可能比流界晚起来）。
                    if clipboard_retry_at.is_some_and(|t| Instant::now() < t) {
                        continue;
                    }
                    match flow_platform::clipboard() {
                        Ok(a) => {
                            cb.access = Some(a);
                            clipboard_retry_at = None;
                        }
                        Err(e) => {
                            warn!(error = %e, "无法访问剪贴板");
                            clipboard_retry_at = Some(Instant::now() + Duration::from_secs(30));
                            drop(cb);
                            // 只说一次：设置里「共享剪贴板」还开着，用户得知道它现在其实没在工作。
                            if !clipboard_warned {
                                clipboard_warned = true;
                                self.notice(format!(
                                    "流界现在读不到这台电脑的剪贴板（{}），复制的内容暂时同步不到其他电脑。流界会每隔半分钟再试。",
                                    platform_reason(&e)
                                ));
                            }
                            continue;
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
                    f.unoffered = None;
                    (f.epoch, f.local.take().map(|(o, _)| o.id))
                };
                let Some(access) = cb.access.as_mut() else {
                    continue;
                };
                let capable = peers.as_ref().is_some_and(|p| p.iter().any(|p| p.files));
                let mut files = if capable || files_on {
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
                if !capable && let Some(paths) = files.take() {
                    // 现在没有能收文件的设备：记下来，之后连上的设备会收到清单。
                    self.inner.files.lock().expect("files").unoffered = Some((paths, epoch));
                }
                let Some(peers) = peers.filter(|p| !p.is_empty()) else {
                    continue;
                };
                let Some(access) = cb.access.as_mut() else {
                    continue;
                };
                let text = match access.get_text() {
                    Ok(t) => t,
                    Err(e) => {
                        debug!(error = %e, "读取剪贴板失败");
                        None
                    }
                };
                // 既不是文件也没有文字：可能是截图或复制的图片，稍后在后台读出来（转换较慢，不在这里持锁做）。
                // 有文字时不看图片：表格、文档软件复制文字时会顺带放一张渲染图。
                let image = capable && files.is_none() && text.as_deref().is_none_or(str::is_empty);
                let msg = text.and_then(|t| cb.sync.on_local_change(&t));
                (epoch, withdrawn, files, image, msg, peers)
            };
            // 读取剪贴板后可能刚好暂停了共享；按当前状态和代号核对后再投递。
            let st = self.lock();
            if st.config.settings.sharing_paused
                || !st.config.settings.clipboard
                || self.inner.files.lock().expect("files").epoch != epoch
            {
                continue;
            }
            if let Some(id) = withdrawn {
                for p in peers.iter().filter(|p| p.files) {
                    let _ = p.tx.send(Message::FileOffer(FileOffer::withdrawn(id)));
                }
            }
            match files {
                Some(paths) => {
                    // 复制了文件：支持的设备收清单；旧版本照旧收文字（Finder 会附带文件名）。
                    if let Some(msg) = msg {
                        for p in peers.iter().filter(|p| !p.files) {
                            let _ = p.tx.send(msg.clone());
                        }
                    }
                    tokio::spawn(self.clone().offer_files(paths, epoch, false));
                }
                None if image => {
                    tokio::spawn(self.clone().offer_image(epoch));
                }
                None => {
                    if let Some(msg) = msg {
                        debug!(peers = peers.len(), "发送剪贴板");
                        for p in peers {
                            let _ = p.tx.send(msg.clone());
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
        // 流界不转发别人的剪贴板，来源一定是发来这条消息的设备自己。不核对的话，一台设备可以冒用
        // 另一台的名义发一个很大的序号，让那一台真正的复制之后都被当成旧消息丢掉。
        if origin != peer {
            return;
        }
        let st = self.lock();
        let allowed = st.config.settings.clipboard
            && !st.config.settings.sharing_paused
            && st.config.peer(peer).is_some_and(|p| !p.paused);
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
                f.unoffered = None;
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
            let transfers = {
                let st = self.lock();
                let name = |id: &DeviceId| {
                    st.config
                        .peer(id)
                        .map(|p| p.name.clone())
                        .unwrap_or_default()
                };
                self.inner
                    .files
                    .lock()
                    .expect("files")
                    .transfers
                    .views(name)
            };
            (self.inner.notify)(EngineEvent::Transfers(transfers));
        }
    }

    /// 本机剪贴板里可能是图片（截图、复制的图片）：读出来存成临时 PNG 文件，按图片提供给其他设备。
    /// `epoch` 是发现时剪贴板的代。
    async fn offer_image(self, epoch: u64) {
        let image_dir = self.inner.data_dir.join("clipboard-image");
        let staged = tokio::task::spawn_blocking(move || -> Result<Option<PathBuf>, String> {
            // 读取和转换可能要几百毫秒：单独开一个剪贴板访问，不占引擎的锁。
            let mut access = flow_platform::clipboard().map_err(|e| e.to_string())?;
            let Some(png) = access.get_image().map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            if png.len() as u64 > MAX_IMAGE_BYTES {
                return Err(format!(
                    "图片太大（超过 {} MB），请先存成文件再复制。",
                    MAX_IMAGE_BYTES >> 20
                ));
            }
            // 临时文件只留最新的一张；每次用新的子目录，不改动可能还在发送的旧文件名。
            // 放在流界自己的数据目录里，不放系统的临时目录：Linux 上 /tmp 所有用户共用，
            // 别的本机用户先建好同名目录就能读到截图，或者把文件换掉。
            let base = image_dir.clone();
            let _ = std::fs::remove_dir_all(&base);
            let dir = base.join(epoch.to_string());
            let path = dir.join(IMAGE_FILE_NAME);
            std::fs::create_dir_all(&dir)
                .and_then(|()| std::fs::write(&path, &png))
                .map_err(|e| transfer::io_text(&path, &e))?;
            Ok(Some(path))
        })
        .await;
        match staged {
            Ok(Ok(Some(path))) => self.offer_files(vec![path], epoch, true).await,
            Ok(Ok(None)) | Err(_) => {}
            Ok(Err(e)) => {
                warn!(error = %e, "读取剪贴板里的图片失败");
                self.notice(format!("复制的图片无法共享给其他电脑：{e}"));
            }
        }
    }

    /// 把本机这些文件作为当前复制的内容提供给其他设备：整理清单并发出去。
    /// `epoch` 是发现时剪贴板的代，整理期间剪贴板又变了就放弃。
    /// `image`：这是剪贴板里的一张图片存成的 PNG 文件，对方收完后写入剪贴板的是图片本身。
    pub(crate) async fn offer_files(self, paths: Vec<PathBuf>, epoch: u64, image: bool) {
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
        let st = self.lock();
        if st.config.settings.sharing_paused {
            return;
        }
        let offer = {
            let mut f = self.inner.files.lock().expect("files");
            if f.epoch != epoch {
                return;
            }
            f.next_offer += 1;
            let offer = FileOffer {
                id: f.next_offer,
                image: image && summary.items == 1 && summary.files == 1,
                ..summary
            };
            f.local = Some((offer.clone(), paths));
            offer
        };
        let peers: Vec<_> = Self::clipboard_peers(&st)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.files)
            .collect();
        info!(
            files = offer.files,
            bytes = offer.bytes,
            image = offer.image,
            peers = peers.len(),
            "复制了文件，已通知其他设备"
        );
        for p in peers {
            let _ = p.tx.send(offer.clone().into_message(p.images));
        }
    }

    /// 本机当前剪贴板的代（测试中模拟"复制了文件"时用）。
    #[cfg(test)]
    fn clipboard_epoch(&self) -> u64 {
        self.inner.files.lock().expect("files").epoch
    }

    fn on_file_offer(&self, peer: &DeviceId, offer: FileOffer) {
        let controlled = self.inner.controlled.lock().expect("controlled").is_some();
        debug!(device = %short(peer), items = offer.items, bytes = offer.bytes, "收到文件清单");
        let fetch = {
            let st = self.lock();
            if !st.files_allowed(peer) {
                return;
            }
            self.inner.files.lock().expect("files").book.on_offer(
                peer.clone(),
                offer,
                controlled,
                self.now_ms(),
            )
        };
        if let Some((from, offer)) = fetch {
            self.start_fetch(from, offer);
        }
        // 文件清单和传输列表只影响显示，不用重新同步控制器。
        self.emit_view();
    }

    /// 键鼠来到本机（开始被控制，或控制远端后回到本机）：有其他设备刚复制的文件就去取。
    fn on_arrive(&self) {
        let fetch = {
            let st = self.lock();
            if st.config.settings.sharing_paused {
                return;
            }
            let mut f = self.inner.files.lock().expect("files");
            let Some((from, offer)) = f.book.on_arrive(self.now_ms()) else {
                return;
            };
            if st.files_allowed(&from) {
                Some((from, offer))
            } else {
                // 单独暂停这台设备时留着清单，等用户手动接收；全局暂停会清空清单。
                f.book.restore(from, offer);
                None
            }
        };
        if let Some((from, offer)) = fetch {
            self.start_fetch(from, offer);
        }
    }

    /// 用户点了「接收」：取等待中的文件。
    pub fn accept_files(&self) {
        // 这台设备被单独暂停时清单留着（见 `on_arrive`），但现在不能收：先别把清单取走，
        // 否则 `start_fetch` 发现不允许就直接返回，清单也没了，用户什么都看不到。
        let blocked = {
            let st = self.lock();
            let f = self.inner.files.lock().expect("files");
            f.book
                .pending()
                .filter(|(from, _)| !st.files_allowed(from))
                .map(|(from, _)| {
                    st.config
                        .peer(from)
                        .map(|p| p.name.clone())
                        .unwrap_or_default()
                })
        };
        if let Some(name) = blocked {
            self.notice(format!(
                "现在不能接收「{name}」的文件：它被暂停了，或者「复制文件和图片」关着。恢复后再点「接收」。"
            ));
            return;
        }
        let pending = self.inner.files.lock().expect("files").book.take();
        if let Some((from, offer)) = pending {
            self.start_fetch(from, offer);
        }
        self.emit_view();
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
        self.emit_view();
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
            let st = self.lock();
            if !st.files_allowed(&peer) {
                return;
            }
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
                transfer::receive(
                    &mut tx,
                    &mut rx,
                    offer.id,
                    offer.bytes,
                    &root,
                    &ticket,
                    || engine.progress(),
                )
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
                let pasted = if offer.image {
                    self.paste_image(&paths, epoch)
                } else {
                    self.paste_files(&paths, epoch)
                };
                info!(
                    files = paths.len(),
                    image = offer.image,
                    pasted,
                    "文件接收完成"
                );
                self.inner
                    .files
                    .lock()
                    .expect("files")
                    .transfers
                    .finish(id, Ok(pasted));
                self.notice(match (offer.image, pasted) {
                    (true, true) => format!("已收到「{name}」复制的图片，可以直接粘贴。"),
                    (true, false) => {
                        format!("已收到「{name}」复制的图片，保存在下载文件夹的「流界」里。")
                    }
                    (false, true) => format!("已收到「{name}」的「{label}」，可以直接粘贴。"),
                    (false, false) => {
                        format!("已收到「{name}」的「{label}」，保存在下载文件夹的「流界」里。")
                    }
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
        self.emit_view();
    }

    /// 把收到的图片本身（不是文件）写入本机剪贴板，写入后删掉收到的文件：对方复制的是图片，
    /// 不该在下载文件夹里留下一堆截图。写不进去时文件留着，返回 false。
    fn paste_image(&self, paths: &[PathBuf], epoch: u64) -> bool {
        let [path] = paths else {
            return false;
        };
        if !self.inner.input {
            return false;
        }
        // 读文件和（Windows 上）解码都在持锁之前做。
        let image = match std::fs::metadata(path) {
            Ok(m) if m.len() <= MAX_IMAGE_BYTES => std::fs::read(path)
                .ok()
                .and_then(flow_platform::ClipboardImage::from_png),
            _ => None,
        };
        let Some(image) = image else {
            warn!("收到的图片无法识别，保留为文件");
            return false;
        };
        let written = self.write_clipboard(epoch, |access| access.set_image(&image));
        if written {
            let _ = std::fs::remove_file(path);
        }
        written
    }

    /// 把收到的文件写入本机剪贴板。期间用户复制了别的内容就不覆盖。返回是否写入。
    fn paste_files(&self, paths: &[PathBuf], epoch: u64) -> bool {
        if !self.inner.input || paths.is_empty() {
            return false;
        }
        self.write_clipboard(epoch, |access| access.set_files(paths))
    }

    /// 把收到的内容写入本机剪贴板；暂停了共享，或剪贴板的代已经不是 `epoch`（用户又复制了别的）时不写。
    fn write_clipboard(
        &self,
        epoch: u64,
        write: impl FnOnce(&mut dyn flow_platform::ClipboardAccess) -> Result<(), PlatformError>,
    ) -> bool {
        let st = self.lock();
        if st.config.settings.sharing_paused {
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
        match write(access.as_mut()) {
            Ok(()) => {
                // 记下写入后的计数，轮询时不会把收到的内容当成本机复制的再发出去。
                let count = access.change_count();
                cb.last_count = Some(count);
                true
            }
            Err(e) => {
                warn!(error = %e, "把收到的内容写入剪贴板失败");
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
        let transfer = {
            let st = self.lock();
            let mut f = self.inner.files.lock().expect("files");
            f.local
                .clone()
                .filter(|(o, _)| o.id == id && st.files_allowed(&peer))
                .map(|(offer, paths)| {
                    let (tid, ticket) =
                        f.transfers
                            .start(peer.clone(), false, offer.label(), offer.bytes);
                    (offer, paths, tid, ticket)
                })
        };
        let Some((offer, paths, tid, ticket)) = transfer else {
            let _ = tx
                .send(&Message::FileError {
                    reason: "复制的内容已经变了，请在那台电脑上重新复制文件。".into(),
                })
                .await;
            tx.finish();
            return;
        };
        info!(device = %short(&peer), files = offer.files, bytes = offer.bytes, "开始发送文件");
        self.emit_view();
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
        self.emit_view();
    }

    // -----------------------------------------------------------------------
    // 跨屏：被控制方
    // -----------------------------------------------------------------------

    fn inject(&self, f: impl FnOnce(&mut dyn InputInjector) -> Result<(), PlatformError>) {
        if !self.inner.input {
            return;
        }
        if self.inner.inject_blocked.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = self.inner.injector.lock().expect("injector");
        if guard.is_none() {
            match flow_platform::input_injector() {
                Ok(i) => *guard = Some(i),
                Err(e) => {
                    drop(guard);
                    self.inject_failed(&e);
                    return;
                }
            }
        }
        let Some(inj) = guard.as_mut() else {
            return;
        };
        match f(inj.as_mut()) {
            Ok(()) => {}
            // 授权没了（用户拒绝了弹窗、到时间没点、被系统收回）：丢掉这个注入器，下次需要时重建，
            // 重建时会重新申请授权。
            Err(e @ PlatformError::Denied(_)) => {
                *guard = None;
                drop(guard);
                self.inject_failed(&e);
            }
            Err(e) => debug!(error = %e, "键鼠注入失败"),
        }
    }

    /// 这台电脑上现在注入不了键鼠。只在第一次时提示；之后这次被控制期间不再尝试（否则每个键鼠
    /// 事件都重试、都弹窗），下次有电脑把键鼠切过来（`on_enter`）或用户点「重新授权」时再试。
    /// 调用时可能拿着 `state`，所以这里不推界面状态，由维护循环发现变化后推。
    fn inject_failed(&self, e: &PlatformError) {
        if self.inner.inject_blocked.swap(true, Ordering::SeqCst) {
            return;
        }
        warn!(error = %e, "无法注入键鼠");
        self.notice(match e {
            PlatformError::Denied(why) => format!(
                "流界没有拿到控制这台电脑键鼠的授权（{why}），其他电脑的键鼠暂时操作不了它。下次有电脑把键鼠切过来时会再询问一次，也可以现在点「重新授权」。"
            ),
            other => format!(
                "无法在这台电脑上模拟键鼠（{}），其他电脑的键鼠暂时操作不了它。",
                platform_reason(other)
            ),
        });
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
        let st = self.lock();
        if !Self::accepting(&st, peer) {
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
                // 不是真实的按键：对方在告知它那边大写锁定的状态。
                Message::Key { code, .. } if code.as_caps_state().is_some() => {
                    code.as_caps_state().map(Remote::Caps)
                }
                Message::Key { code, down, repeat } => {
                    let win_keys = *self.inner.win_keys.lock().expect("win_keys");
                    let now = map_key(c.by_os, Os::current(), win_keys, code);
                    let code = if down {
                        // 连发沿用按下时的换算。
                        *c.mapped.entry(code).or_insert(now)
                    } else {
                        c.mapped.remove(&code).unwrap_or(now)
                    };
                    c.pressed.on_key(code, down);
                    Some(Remote::Key(code, down, repeat))
                }
                Message::Leave { session } if session == c.session => Some(Remote::Release),
                // 现在的流界不发这条（结束控制时发逐个按键的抬起加 `Leave`）；协议里有，照着处理。
                Message::ReleaseAll => Some(Remote::ReleaseKeys),
                _ => None,
            }
        };
        // 真的键鼠操作（不含心跳、大写锁定状态这类通知）算作本机有人在用：
        // 键鼠一直停在本机、屏幕中途熄了的话，没有新的 `Enter`，要靠这里点亮。
        if matches!(
            action,
            Some(Remote::Move(..) | Remote::Button(..) | Remote::Wheel(..) | Remote::Key(..))
        ) {
            self.remote_activity();
        }
        match action {
            Some(Remote::Move(display, x, y)) => self.inject(|i| i.move_to(&display, x, y)),
            Some(Remote::Button(b, down)) => self.inject(|i| i.button(b, down)),
            Some(Remote::Wheel(dx, dy)) => self.inject(|i| i.wheel(dx, dy)),
            Some(Remote::Key(code, down, repeat)) => self.inject(|i| i.key(code, down, repeat)),
            Some(Remote::Caps(on)) => self.inject(|i| i.set_caps_lock(on)),
            Some(Remote::Release) => {
                drop(st);
                // 对方主动离开：键鼠跟着它回去了。界面上标到它那边，直到本机有输入为止。
                // 看不到本机输入的系统（Linux 的 Wayland）没法知道什么时候回来，就不做这个推断。
                if !flow_platform::limits().local_input {
                    self.set_input_at(Some(peer.clone()));
                }
                self.release_controlled(Some(peer));
            }
            Some(Remote::ReleaseKeys) => {
                let events = self
                    .inner
                    .controlled
                    .lock()
                    .expect("controlled")
                    .as_mut()
                    .map(|c| {
                        c.mapped.clear();
                        c.pressed.release_all()
                    })
                    .unwrap_or_default();
                self.inject_releases(events);
            }
            None => {}
        }
    }

    /// 快捷键映射：`peer` 请本机按一下这组键（修饰键在前）。键鼠不用先切过来。
    ///
    /// 正在控制本机的设备手上可能还按着修饰键（触发映射用的 Ctrl、Alt 已经发过来了），
    /// 直接按会和它们组合在一起：先把这些修饰键抬起，按完再按回去。
    fn on_keystroke(&self, peer: &DeviceId, peer_os: Os, keys: &[KeyCode]) {
        // 暂停共享或暂停了这台设备时，它不能操作本机。整个过程拿着 `state`（和 `on_remote_input`
        // 一样）：暂停完成后不会再注入；也不会和控制方的按键交错——"抬起修饰键、按键、再按回去"
        // 的中途对方松开了修饰键的话，最后那次"按回去"会让它一直卡在按下。
        let st = self.lock();
        if !Self::accepting(&st, peer) {
            return;
        }
        let win_keys = *self.inner.win_keys.lock().expect("win_keys");
        let keys: Vec<KeyCode> = keys
            .iter()
            .take(MAX_KEYSTROKE_KEYS)
            // 只接受真实的按键（HID 键码表以内），不接受大写锁定状态那类保留值。
            .filter(|k| (0x04..=0xE7).contains(&k.0))
            .map(|&k| map_key(peer_os, Os::current(), win_keys, k))
            .collect();
        if keys.is_empty() {
            return;
        }
        let held = self
            .inner
            .controlled
            .lock()
            .expect("controlled")
            .as_ref()
            .map(|c| c.pressed.modifier_keys())
            .unwrap_or_default();
        info!(device = %short(peer), keys = keys.len(), "对方通过快捷键映射发来按键");
        // 和别的电脑的键鼠进来一样算作有人在用：屏幕熄着的话点亮。
        self.remote_activity();
        for (k, down) in flow_core::hotkeys::tap_sequence(&held, &keys) {
            self.inject(|i| i.key(k, down, false));
        }
        drop(st);
    }

    /// `peer`（正被本机控制的电脑）请本机把键鼠切到 `device`：用户在那台电脑的托盘里点了
    /// 「切换到」。只有本机的键鼠确实在它那边时才理会，否则任何一台已配对的电脑都能随时
    /// 把本机的键鼠拽走。
    fn on_switch_request(&self, peer: &DeviceId, device: &DeviceId) {
        let mut ctl = self.controller();
        if ctl.active_device() != Some(peer) {
            debug!(device = %short(peer), "对方请求切换键鼠，但键鼠不在它那边，忽略");
            return;
        }
        let ok = ctl.switch_to_device(device);
        info!(from = %short(peer), to = %short(device), ok, "对方请求切换键鼠");
    }

    fn on_enter(&self, peer: &DeviceId, peer_os: Os, session: u32, display: &str, x: f64, y: f64) {
        // 暂停共享或暂停了这台设备时不接受；锁定只限制本机切出去。先判断，被拒绝时
        // 不能动正在控制本机的那台。之后它发来的输入因为不是当前控制者而被忽略。
        let refused = {
            let mut st = self.lock();
            let ok = Self::accepting(&st, peer);
            if !ok && let Some(s) = st.sessions.get_mut(peer) {
                // 对方可能还没收到"不接受"就切过来了（或者它是旧版本）：再告诉它一次。
                if s.acks {
                    let _ = s.tx.send(Message::Availability { accepting: false });
                    s.accepting_sent = Some(false);
                }
            }
            !ok
        };
        if refused {
            info!(device = %short(peer), "本机已暂停共享，拒绝被控制");
            return;
        }
        // 换了一台设备来控制：先把上一台按下的键释放掉。
        self.release_controlled(None);
        let st = self.lock();
        if !Self::accepting(&st, peer) {
            return;
        }
        // 本机正在控制别的设备时被接管（用户拿起了那台电脑自己的鼠标推过来）：
        // 先释放远端按键、解锁本机光标，再接受对方的控制，否则两边都以为自己是控制方。
        if self.controller().yield_control() {
            info!(device = %short(peer), "本机被接管");
        }
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
        // 又有电脑要操作本机了：上次注入不了的话，这次重新试（需要授权的话会再弹窗）。
        self.inner.inject_blocked.store(false, Ordering::SeqCst);
        // 屏幕熄着的话先点亮：外接显示器熄屏后可能已经断开，鼠标移动落不到它上面。
        self.remote_activity();
        self.set_input_at(None);
        *self.inner.controlled.lock().expect("controlled") = Some(Controlled {
            by: peer.clone(),
            by_os: peer_os,
            session,
            pressed: PressedState::default(),
            mapped: HashMap::new(),
            last_seen: Instant::now(),
        });
        self.inject(|i| i.move_to(display, x, y));
        self.edge_glow(&st, display, x, y);
        drop(st);
        // 键鼠来到本机：对方刚复制的文件可能要在这里粘贴。
        self.on_arrive();
        self.emit();
    }

    /// 键鼠来到了本机这块显示器的 `(x, y)`（显示器内的逻辑坐标）：在它进来的那条屏幕边上闪一道浮光。
    fn edge_glow(&self, st: &State, display: &str, x: f64, y: f64) {
        if !self.inner.input || !st.config.settings.edge_glow {
            return;
        }
        let Some(run) = &self.inner.main_thread else {
            return;
        };
        let Some(d) = st.local_displays.iter().find(|d| d.local_id == display) else {
            return;
        };
        // 落点贴着它穿过的那条边。
        let rect = Rect::new(0.0, 0.0, d.rect.width, d.rect.height);
        let p = rect.clamp(Point::new(x, y));
        let edge = rect.nearest_edge(p);
        let along = rect.edge_fraction(edge, p);
        let display = display.to_owned();
        // macOS 的窗口只能在主线程上建。
        run(Box::new(move || {
            flow_platform::edge_glow(&display, edge, along)
        }));
    }

    /// 键鼠从远端穿过屏幕边回到了本机的 `at`（布局坐标）：同样闪一道浮光。
    fn glow_on_return(&self, at: Point) {
        let st = self.lock();
        let (ox, oy) = st.config.local_offset;
        let p = Point::new(at.x - ox, at.y - oy);
        // 落点可能正好在屏幕边界上：取最近的一块。
        let dist = |r: &Rect| {
            let q = r.clamp(p);
            (q.x - p.x).abs() + (q.y - p.y).abs()
        };
        let Some(d) = st
            .local_displays
            .iter()
            .min_by(|a, b| dist(&a.rect).total_cmp(&dist(&b.rect)))
        else {
            return;
        };
        self.edge_glow(&st, &d.local_id, p.x - d.rect.x, p.y - d.rect.y);
    }

    /// 记下（或清掉）"键鼠被这台设备带走了"，并让控制器在本机有输入时通知。
    fn set_input_at(&self, device: Option<DeviceId>) {
        let away = device.is_some();
        let changed = {
            let mut at = self.inner.input_at.lock().expect("input at");
            let changed = *at != device;
            *at = device;
            changed
        };
        if changed {
            self.controller().set_away(away);
        }
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

    #[test]
    fn mappings_are_dropped_where_the_keyboard_is_not_visible() {
        use flow_core::hotkeys::{KeyCombo, KeyMapping};
        let mut h = Hotkeys::default();
        h.mappings.push(KeyMapping {
            trigger: KeyCombo::new(Modifiers::CTRL | Modifiers::ALT, KeyCode(0x16)),
            send: KeyCombo::new(Modifiers::NONE, KeyCode(0x16)),
            device: DeviceId("p".into()),
        });
        assert_eq!(usable_hotkeys(&h, flow_platform::Limits::default()), h);
        let wayland = flow_platform::Limits {
            local_hotkey: true,
            ..Default::default()
        };
        let used = usable_hotkeys(&h, wayland);
        assert!(used.mappings.is_empty());
        // 切换用的快捷键照常保留（键鼠切过去之后用它切回来）。
        assert_eq!(used.switch_left, h.switch_left);
        assert_eq!(used.switch_right, h.switch_right);
    }

    #[test]
    fn map_key_uses_win_keys_only_when_mac_is_controlled() {
        let pos = WinToMacKeys::MAC_POSITION;
        // PC 键盘控制 Mac：按设置换算。
        assert_eq!(
            map_key(Os::Windows, Os::MacOs, pos, KeyCode::LEFT_ALT),
            KeyCode::LEFT_META
        );
        assert_eq!(
            map_key(
                Os::Windows,
                Os::MacOs,
                WinToMacKeys::WINDOWS,
                KeyCode::LEFT_CTRL
            ),
            KeyCode::LEFT_META
        );
        // Mac 控制 PC：不受这项设置影响，仍是 ⌘ ↔ Ctrl。
        assert_eq!(
            map_key(Os::MacOs, Os::Windows, pos, KeyCode::LEFT_META),
            KeyCode::LEFT_CTRL
        );
        assert_eq!(
            map_key(Os::MacOs, Os::Windows, pos, KeyCode::LEFT_ALT),
            KeyCode::LEFT_ALT
        );
        // 同系统之间不换算。
        assert_eq!(
            map_key(Os::MacOs, Os::MacOs, pos, KeyCode::LEFT_ALT),
            KeyCode::LEFT_ALT
        );
        assert_eq!(
            map_key(Os::Windows, Os::Windows, pos, KeyCode::LEFT_CTRL),
            KeyCode::LEFT_CTRL
        );
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
        assert!(Config::load(&a.dir).unwrap().is_trusted(&idb));
        assert!(Config::load(&b.dir).unwrap().is_trusted(&ida));
        assert_eq!(
            Config::load(&a.dir).unwrap().peer(&idb).unwrap().last_addrs,
            vec![addr(&b.engine)],
            "双栈连接的 IPv4 地址应保存为原生 IPv4，兼容之后的 IPv4 回退"
        );

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

    #[tokio::test]
    async fn address_errors_do_not_hide_an_earlier_ipv4_timeout() {
        let a = node("address-errors").await;
        let silent = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ipv4 = silent.local_addr().unwrap();
        let invalid_ipv6 = "[::1]:0".parse().unwrap();
        let error = a
            .engine
            .try_connect(&DeviceId("peer".into()), &[ipv4, invalid_ipv6], false)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ConnectFailureKind::Timeout);
        assert_eq!(error.fault(), Fault::Unreachable);
        let error = error.to_string();
        assert!(error.contains(&ipv4.to_string()), "{error}");
        assert!(error.contains("UDP 握手未完成"), "{error}");
        assert!(error.contains(&invalid_ipv6.to_string()), "{error}");
        a.engine.shutdown();
    }

    /// 连接失败后，设备详情里能看到原因和排查步骤：看得到对方却连不上是防火墙，
    /// 看不到对方是找不到，地址不在同一网段是不在同一网络。
    #[tokio::test]
    async fn failed_connections_are_diagnosed_and_remembered() {
        let a = node("diagnose").await;
        let peer = DeviceId("ab".repeat(32));
        a.engine.lock().config.peers.push(PeerRecord {
            id: peer.clone(),
            name: "书房台式机".into(),
            os: Os::Windows,
            displays: vec![],
            offset: None,
            last_addrs: vec![],
            paused: false,
        });
        let problem = |e: &Engine| {
            e.view()
                .devices
                .into_iter()
                .find(|d| d.id == peer)
                .and_then(|d| d.problem)
        };
        assert_eq!(problem(&a.engine), None);

        // 测试里本机的网段固定是 192.168.1.0/24。同一网段、现在能发现它，但连不上：防火墙。
        let near: SocketAddr = "192.168.1.50:24830".parse().unwrap();
        a.engine.add_nearby(peer.clone(), "书房台式机", near);
        let d = a.engine.record_failure(&peer, &[near], Fault::Unreachable);
        assert_eq!(d.cause, Cause::Blocked);
        let shown = problem(&a.engine).unwrap();
        assert!(shown.summary.contains("防火墙"), "{}", shown.summary);
        assert!(shown.steps[0].contains("Windows 安全中心"));

        // 发现不了它了：找不到。
        a.engine.lock().nearby.clear();
        let d = a.engine.record_failure(&peer, &[near], Fault::Unreachable);
        assert_eq!(d.cause, Cause::NotFound);
        // 上次的地址在另一个网段：不在同一网络。
        let far: SocketAddr = "192.168.31.8:24830".parse().unwrap();
        let d = a.engine.record_failure(&peer, &[far], Fault::Unreachable);
        assert_eq!(d.cause, Cause::OtherNetwork);
        assert!(problem(&a.engine).unwrap().summary.contains("192.168.31.8"));

        // 用户点「连接」失败：提示里带原因和第一步，并指向设备详情。
        a.engine.lock().config.peer_mut(&peer).unwrap().last_addrs = vec![far];
        a.engine
            .clone()
            .connect_and_run(peer.clone(), vec![], false, true)
            .await;
        let notices = a.notices.lock().unwrap().clone();
        let last = notices.last().unwrap();
        assert!(last.contains("找不到「书房台式机」"), "{last}");
        assert!(last.contains("设备详情"), "{last}");
        a.engine.shutdown();
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
        wait_for_long("A 发现断开", || {
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
        wait_for_long("A 发现断开", || {
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
                .any(|n| n.contains("已暂停自动重试"))
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

    /// A 通过会话直接给 B 发 Enter（模拟 A 的键鼠切到 B）。
    fn enter(from: &Engine, to: &DeviceId, session: u32) {
        let st = from.lock();
        let _ = st.sessions[to].tx.send(Message::Enter {
            session,
            display: "1".into(),
            x: 10.0,
            y: 10.0,
        });
    }

    fn remote_paused(e: &Engine, id: &DeviceId) -> bool {
        e.view()
            .devices
            .iter()
            .any(|d| &d.id == id && d.remote_paused)
    }

    fn has_input(e: &Engine, id: &DeviceId) -> bool {
        e.view().devices.iter().any(|d| &d.id == id && d.has_input)
    }

    fn device(e: &Engine, id: &DeviceId) -> DeviceView {
        e.view().devices.into_iter().find(|d| &d.id == id).unwrap()
    }

    /// 给 `e` 的布局里放上 `peer` 的一块屏幕（测试里收不到真实的屏幕信息）。
    fn give_display(e: &Engine, peer: &DeviceId) {
        {
            let mut st = e.lock();
            let p = st.config.peer_mut(peer).unwrap();
            p.displays = vec![DisplayInfo {
                local_id: "1".into(),
                name: "屏幕".into(),
                rect: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                scale: 1.0,
                primary: true,
                native: None,
            }];
            p.offset = Some((10_000.0, 0.0));
        }
        e.emit();
    }

    /// 托盘里的「切换到」：在键鼠所在的电脑上直接切；在被控制的电脑上点，则请控制方来切。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn switching_from_the_menu_works_on_both_sides() {
        let (a, b) = paired("a-sw", "b-sw").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        give_display(&a.engine, &idb);
        assert!(device(&a.engine, &idb).can_switch);
        // 键鼠在本机时，「回到本机」没有意义。
        assert!(!device(&a.engine, &ida).can_switch);

        // 在 A 上点「切换到 B」：A 开始控制 B。
        a.engine.switch_to(&idb).unwrap();
        wait_for("B 被 A 控制", || {
            b.engine.view().controlled_by.as_ref() == Some(&ida)
        })
        .await;
        assert!(device(&a.engine, &idb).active);
        assert!(device(&a.engine, &ida).can_switch);

        // 在 B 上（用 A 的键鼠）点「切换到 A」：B 请 A 把键鼠收回去。
        assert!(device(&b.engine, &ida).can_switch);
        b.engine.switch_to(&ida).unwrap();
        wait_for("键鼠回到 A", || {
            device(&a.engine, &ida).active && b.engine.view().controlled_by.is_none()
        })
        .await;

        // B 没在被 A 控制时发来的切换请求不理会：不能随便把别人的键鼠拽走。
        let _ = b.engine.lock().sessions[&ida].tx.send(Message::SwitchTo {
            device: idb.clone(),
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(device(&a.engine, &ida).active);

        // 锁定在本机、暂停共享时切不出去，并说明原因。
        a.engine.set_locked(true);
        assert!(a.engine.switch_to(&idb).unwrap_err().contains("锁定"));
        assert!(!device(&a.engine, &idb).can_switch);
        a.engine.set_locked(false);
        let mut s = a.engine.settings();
        s.sharing_paused = true;
        a.engine.update_settings(s);
        assert!(a.engine.switch_to(&idb).unwrap_err().contains("暂停"));
    }

    /// `flowacross --switch <目标>`：方向、设备名、设备 ID 的前几位、回到本机。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn switch_command_understands_directions_names_and_home() {
        let (a, b) = paired("a-cmd", "b-cmd").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        // 测试环境里不一定枚举得到真实的显示器：给 A 放一块，B 的屏幕放在它右边。
        a.engine.lock().local_displays = vec![test_display(1440.0)];
        give_display(&a.engine, &idb);
        let on = |id: &DeviceId| device(&a.engine, id).active;
        assert!(
            a.engine
                .switch_command("left")
                .unwrap_err()
                .contains("左边没有")
        );
        a.engine.switch_command("right").unwrap();
        assert!(on(&idb));
        a.engine.switch_command("home").unwrap();
        assert!(on(&ida));
        // 设备名不分大小写。
        let name = device(&a.engine, &idb).name;
        a.engine.switch_command(&name.to_uppercase()).unwrap();
        assert!(on(&idb));
        a.engine.switch_command("本机").unwrap();
        assert!(on(&ida));
        // 设备 ID 的前几位。
        a.engine.switch_command(&idb.0[..8]).unwrap();
        assert!(on(&idb));
        // 键鼠在 B 上时，「左」是回到 A。
        a.engine.switch_command("左").unwrap();
        assert!(on(&ida));
        let err = a.engine.switch_command("不存在的电脑").unwrap_err();
        assert!(err.contains("没有叫"), "{err}");
    }

    /// 对方停在锁屏界面：设备上有标记，把键鼠切过去时提示一次要去那台电脑上解锁。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_locked_peer_is_marked_and_warned_about_once() {
        let (a, b) = paired("a-lock", "b-lock").await;
        let idb = b.engine.local_id().clone();
        give_display(&a.engine, &idb);
        assert!(!device(&a.engine, &idb).locked);
        b.engine.set_local_locked(Some(true));
        wait_for("A 看到 B 锁屏", || device(&a.engine, &idb).locked).await;
        let warned = |n: &Node| {
            n.notices
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.contains("锁屏界面"))
                .count()
        };
        assert_eq!(warned(&a), 0);
        a.engine.switch_to(&idb).unwrap();
        wait_for("提示去那台电脑上解锁", || warned(&a) == 1).await;
        // 回来再切过去：还锁着，不重复提示。
        a.engine.switch_to(a.engine.local_id()).unwrap();
        a.engine.switch_to(&idb).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(warned(&a), 1);
        // 解锁后标记消失；键鼠还在它那边时它又锁屏，马上再提示。
        b.engine.set_local_locked(Some(false));
        wait_for("A 看到 B 解锁", || !device(&a.engine, &idb).locked).await;
        b.engine.set_local_locked(Some(true));
        wait_for("再次提示", || warned(&a) == 2).await;
    }

    /// 这台电脑睡了一觉醒来：旧连接作废，提示正在重连，并且不等旧连接超时就重新连上。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn waking_from_sleep_reconnects_right_away() {
        let (a, b) = paired("a-wake", "b-wake").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        // 用发送通道认连接：旧连接释放后，新连接可能分配在同一个地址上，`stable_id` 会相同。
        let old = a.engine.lock().sessions[&idb].tx.clone();
        let renewed = |e: &Engine| {
            e.lock()
                .sessions
                .get(&idb)
                .is_some_and(|s| !s.tx.same_channel(&old))
        };
        // 自动重连之前已经停止过也不影响：醒来后重新开始。
        a.engine.lock().manual.insert(idb.clone());
        a.engine.on_resume(Duration::from_secs(600));
        assert!(
            a.notices
                .lock()
                .unwrap()
                .iter()
                .any(|n| n.contains("刚从睡眠中醒来"))
        );
        wait_for("旧连接断开", || {
            !a.engine.lock().sessions.contains_key(&idb) || renewed(&a.engine)
        })
        .await;
        if !a.engine.lock().sessions.contains_key(&idb) {
            let d = device(&a.engine, &idb);
            assert_eq!(d.status, DeviceStatus::Connecting);
            assert!(d.problem.unwrap().summary.contains("正在重新连接"));
        }
        wait_for_long("重新连上", || {
            renewed(&a.engine) && b.engine.lock().sessions.contains_key(&ida)
        })
        .await;
        assert_eq!(device(&a.engine, &idb).problem, None);
    }

    /// 对方的流界退出后，设备详情里说的是"已经退出"，不是笼统的"离线"。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_peer_that_quit_is_described_as_such() {
        let (a, b) = paired("a-quit", "b-quit").await;
        let idb = b.engine.local_id().clone();
        b.engine.shutdown();
        wait_for_long("A 发现 B 退出", || {
            !a.engine.lock().sessions.contains_key(&idb)
        })
        .await;
        let p = device(&a.engine, &idb).problem.unwrap();
        assert!(p.summary.contains("已经退出"), "{}", p.summary);
    }

    /// 移除设备时，指向它的快捷键一起删掉；界面传来的不合法的快捷键不会存进配置。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hotkeys_are_sanitized_and_follow_device_removal() {
        use flow_core::hotkeys::{DeviceHotkey, KeyCombo, KeyMapping};
        let (a, b) = paired("a-hk", "b-hk").await;
        let idb = b.engine.local_id().clone();
        let ctrl_alt = Modifiers::CTRL | Modifiers::ALT;
        let mut s = a.engine.settings();
        s.hotkeys.devices = vec![DeviceHotkey {
            device: idb.clone(),
            combo: KeyCombo::new(ctrl_alt, KeyCode(0x1E)),
        }];
        s.hotkeys.mappings = vec![
            KeyMapping {
                trigger: KeyCombo::new(ctrl_alt, KeyCode(0x16)),
                send: KeyCombo::new(Modifiers::NONE, KeyCode(0x16)),
                device: idb.clone(),
            },
            // 触发键不带 Ctrl / Alt / ⌘：不合法，丢掉。
            KeyMapping {
                trigger: KeyCombo::new(Modifiers::NONE, KeyCode(0x17)),
                send: KeyCombo::new(Modifiers::NONE, KeyCode(0x17)),
                device: idb.clone(),
            },
        ];
        a.engine.update_settings(s);
        let saved = a.engine.settings().hotkeys;
        assert_eq!((saved.devices.len(), saved.mappings.len()), (1, 1));
        assert!(device(&a.engine, &idb).can_send_keys);
        a.engine.remove_device(&idb);
        let saved = a.engine.settings().hotkeys;
        assert!(saved.devices.is_empty() && saved.mappings.is_empty());
        // 左右切换的快捷键不受影响。
        assert!(saved.switch_left.is_some() && saved.switch_right.is_some());
    }

    /// 按前台应用自动锁定：名单里的应用或全屏窗口在前台时切不出去，原因写在提示里；
    /// 键鼠已经在别的电脑上时不会被拽回来。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn foreground_apps_lock_the_input_to_this_computer() {
        let (a, b) = paired("a-auto", "b-auto").await;
        let idb = b.engine.local_id().clone();
        a.engine.lock().local_displays = vec![test_display(1440.0)];
        give_display(&a.engine, &idb);
        let fg = |app: &str, fullscreen: bool| {
            Some(Foreground {
                app: app.into(),
                fullscreen,
            })
        };
        let mut s = a.engine.settings();
        // 名单会被整理：去掉空的和重复的。
        s.auto_lock_apps = vec![" Steam ".into(), "steam".into(), "".into()];
        a.engine.update_settings(s);
        assert_eq!(a.engine.settings().auto_lock_apps, vec!["Steam"]);

        // 不在名单里的应用：不锁；记进最近的应用。
        a.engine.on_foreground(fg("Safari", true));
        assert_eq!(a.engine.view().auto_lock, None);
        assert_eq!(a.engine.view().recent_apps, vec!["Safari"]);
        // 名单里的应用到了前台（不分大小写）：切不出去，提示里说明原因。
        a.engine.on_foreground(fg("steam", false));
        assert_eq!(
            a.engine.view().auto_lock,
            Some(AutoLock::App("steam".into()))
        );
        assert!(!device(&a.engine, &idb).can_switch);
        let err = a.engine.switch_to(&idb).unwrap_err();
        assert!(err.contains("steam") && err.contains("自动锁定"), "{err}");
        // 手动锁定没有被打开。
        assert!(!a.engine.locked());
        // 把它移出名单：马上解除，不等下一次查询。
        let mut s = a.engine.settings();
        s.auto_lock_apps.clear();
        a.engine.update_settings(s);
        assert_eq!(a.engine.view().auto_lock, None);

        // 全屏锁定要打开开关才生效。
        a.engine.on_foreground(fg("Keynote", true));
        assert_eq!(a.engine.view().auto_lock, None);
        let mut s = a.engine.settings();
        s.auto_lock_fullscreen = true;
        a.engine.update_settings(s);
        assert_eq!(
            a.engine.view().auto_lock,
            Some(AutoLock::Fullscreen("Keynote".into()))
        );
        let err = a.engine.switch_to(&idb).unwrap_err();
        assert!(err.contains("全屏"), "{err}");
        // 退出全屏：可以切了。流界自己的窗口全屏也不算。
        a.engine.on_foreground(fg("Keynote", false));
        assert_eq!(a.engine.view().auto_lock, None);
        a.engine.on_foreground(fg("FlowAcross", true));
        assert_eq!(a.engine.view().auto_lock, None);
        a.engine.switch_to(&idb).unwrap();
        assert!(device(&a.engine, &idb).active);

        // 键鼠已经在 B 上：本机前台变成全屏也不锁，不把键鼠拽回来。
        a.engine.on_foreground(fg("Keynote", true));
        assert_eq!(a.engine.view().auto_lock, None);
        assert!(device(&a.engine, &idb).active);
        // 回到本机后的下一次查询才锁上。
        a.engine.switch_to(a.engine.local_id()).unwrap();
        a.engine.on_foreground(fg("Keynote", true));
        assert!(a.engine.view().auto_lock.is_some());
        // 查不到前台应用了：解除。
        a.engine.on_foreground(None);
        assert_eq!(a.engine.view().auto_lock, None);
    }

    /// 连着的设备带着延迟和分级；本机这一项、离线的设备没有。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn connected_devices_report_latency() {
        let (a, b) = paired("a-rtt", "b-rtt").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        let d = device(&a.engine, &idb);
        // 本机回环：延迟很小。
        assert!(
            d.latency_ms.is_some_and(|ms| ms < 1000),
            "{:?}",
            d.latency_ms
        );
        assert_eq!(d.latency_level, d.latency_ms.map(autolock::latency_level));
        let local = device(&a.engine, &ida);
        assert_eq!((local.latency_ms, local.latency_level), (None, None));
        // 第一次有读数时推一次界面，之后没有明显变化就不再推。
        a.engine.refresh_latency();
        assert!(a.engine.lock().latency_shown.contains_key(&idb));
    }

    fn test_display(width: f64) -> DisplayInfo {
        DisplayInfo {
            local_id: "1".into(),
            name: "屏幕".into(),
            rect: Rect::new(0.0, 0.0, width, 1000.0),
            scale: 1.0,
            primary: true,
            native: None,
        }
    }

    /// `of` 在 `e` 的布局里的位置（它的显示器整体的偏移）。
    fn offset_in(e: &Engine, of: &DeviceId) -> Option<(f64, f64)> {
        let st = e.lock();
        if of == e.local_id() {
            Some(st.config.local_offset)
        } else {
            st.config.peer(of).and_then(|p| p.offset)
        }
    }

    /// `to` 相对 `from` 的位置，按 `e` 的布局算。两台电脑的布局一致，就是这个值在两边相同。
    fn relative(e: &Engine, from: &DeviceId, to: &DeviceId) -> Option<(f64, f64)> {
        let (a, b) = (offset_in(e, from)?, offset_in(e, to)?);
        Some((b.0 - a.0, b.1 - a.1))
    }

    /// 两台各有一块屏幕的电脑配对好。测试环境里不一定枚举得到真实的显示器，所以先给它们各放一块。
    async fn paired_with_displays(ta: &str, tb: &str) -> (Node, Node) {
        let a = node(ta).await;
        let b = node(tb).await;
        a.engine.lock().local_displays = vec![test_display(1440.0)];
        b.engine.lock().local_displays = vec![test_display(1920.0)];
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        a.engine.add_nearby(idb.clone(), "B", addr(&b.engine));
        a.engine.pair_with(&idb);
        wait_for("配对请求", || !b.engine.view().pairings.is_empty()).await;
        a.engine.respond_pairing(&idb, true);
        b.engine.respond_pairing(&ida, true);
        wait_for("两边都有了对方的位置", || {
            offset_in(&a.engine, &idb).is_some() && offset_in(&b.engine, &ida).is_some()
        })
        .await;
        (a, b)
    }

    /// 配对后两边的布局统一成一份（以前各自把对方放在自己右边，互相矛盾）；之后在一台上拖动，
    /// 另一台跟着变；关掉同步的那台不跟。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn layout_is_shared_between_devices() {
        let (a, b) = paired_with_displays("a-lay", "b-lay").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        let agree = || {
            let (ra, rb) = (
                relative(&a.engine, &ida, &idb),
                relative(&b.engine, &ida, &idb),
            );
            ra.is_some() && ra == rb
        };
        wait_for("配对后两边的布局一致", agree).await;
        // 两块屏幕左右相贴（不管谁的默认摆法赢了）。
        let (dx, dy) = relative(&a.engine, &ida, &idb).unwrap();
        assert!(dx == 1440.0 || dx == -1920.0, "B 相对 A 的位置 {dx}");
        assert_eq!(dy, 0.0);

        // 在 A 上把 B 往下拖 200：B 那边跟着变。
        a.engine.move_device(&idb, 0.0, 200.0, None).unwrap();
        wait_for("B 跟上 A 的拖动", || {
            relative(&b.engine, &ida, &idb) == Some((dx, 200.0))
        })
        .await;
        // B 自己在它的坐标系里没动，动的是 A。
        assert_eq!(a.engine.lock().config.layout_rev, 1);
        assert_eq!(b.engine.lock().config.layout_rev, 1);
        assert_eq!(b.engine.lock().config.layout_by, Some(ida.clone()));

        // 反过来在 B 上拖 A：A 跟着变。
        b.engine.move_device(&ida, 0.0, 100.0, None).unwrap();
        wait_for("A 跟上 B 的拖动", agree).await;
        assert_eq!(relative(&a.engine, &ida, &idb), Some((dx, 100.0)));
        assert_eq!(a.engine.lock().config.layout_rev, 2);

        // B 关掉同步：A 再拖，B 不跟。
        let mut s = b.engine.settings();
        s.sync_layout = false;
        b.engine.update_settings(s.clone());
        a.engine.move_device(&idb, 0.0, 300.0, None).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(relative(&b.engine, &ida, &idb), Some((dx, 100.0)));
        // 再打开：B 把自己的发出去，A 的更新（改过 3 次），A 回给它，B 跟上。
        s.sync_layout = true;
        b.engine.update_settings(s);
        wait_for("重新打开同步后跟上", agree).await;
        assert_eq!(relative(&b.engine, &ida, &idb), Some((dx, 400.0)));
    }

    /// 对方发来的布局不合法、或者在本机摆出来会重叠时，整份不用。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bad_layouts_from_a_peer_are_ignored() {
        let (a, b) = paired_with_displays("a-badlay", "b-badlay").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        wait_for("布局一致", || {
            relative(&a.engine, &ida, &idb) == relative(&b.engine, &ida, &idb)
        })
        .await;
        let before = relative(&a.engine, &ida, &idb);
        let send = |offsets: Vec<(DeviceId, (f64, f64))>| {
            let _ = b.engine.lock().sessions[&ida]
                .tx
                .send(Message::Layout(SharedLayout {
                    rev: 99,
                    by: idb.clone(),
                    offsets,
                }));
        };
        // 两台电脑的屏幕叠在一起。
        send(vec![(ida.clone(), (0.0, 0.0)), (idb.clone(), (10.0, 10.0))]);
        // 带 NaN。
        send(vec![
            (ida.clone(), (0.0, 0.0)),
            (idb.clone(), (f64::NAN, 0.0)),
        ]);
        // 里面没有 A 自己。
        send(vec![(idb.clone(), (0.0, 0.0))]);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(relative(&a.engine, &ida, &idb), before);
        assert_ne!(a.engine.lock().config.layout_rev, 99);
    }

    /// 对端上报的显示器里带非法的数（NaN、离谱的尺寸）时不收：写进配置会让下次启动读不出配置。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn displays_with_bad_numbers_from_a_peer_are_dropped() {
        let (a, b) = paired_with_displays("a-nan", "b-nan").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        let mut bad = test_display(1920.0);
        bad.rect.x = f64::NAN;
        let mut huge = test_display(1920.0);
        huge.local_id = "2".into();
        huge.rect.width = f64::INFINITY;
        let _ = b.engine.lock().sessions[&ida]
            .tx
            .send(Message::Displays(vec![bad, huge]));
        tokio::time::sleep(Duration::from_millis(300)).await;
        // 全都不合法：沿用原来的那一块。
        let st = a.engine.lock();
        let displays = &st.config.peer(&idb).unwrap().displays;
        assert_eq!(displays.len(), 1);
        assert!(displays[0].rect.x.is_finite() && displays[0].rect.width == 1920.0);
        // 配置照常能存能读。
        let json = serde_json::to_string(&st.config).unwrap();
        assert!(serde_json::from_str::<Config>(&json).is_ok());
    }

    /// 对方退出后，局域网里它的 mDNS 记录还会留一阵；这期间重连失败不能把「已经退出」盖成「防火墙」。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stale_discovery_does_not_turn_a_quit_into_a_firewall_problem() {
        let (a, b) = paired("a-stale", "b-stale").await;
        let idb = b.engine.local_id().clone();
        b.engine.shutdown();
        wait_for_long("A 发现 B 退出", || {
            !a.engine.lock().sessions.contains_key(&idb)
        })
        .await;
        // B 还"看得到"，但连不上。
        let d = a
            .engine
            .record_failure(&idb, &[addr(&b.engine)], Fault::Unreachable);
        assert_eq!(d.cause, Cause::Blocked);
        let p = device(&a.engine, &idb).problem.unwrap();
        assert!(p.summary.contains("已经退出"), "{}", p.summary);
    }

    /// 被移除的设备（连接还没来得及关）不能再操作本机。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_removed_device_is_no_longer_accepted() {
        let (a, b) = paired("a-rm", "b-rm").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        assert!(Engine::accepting(&b.engine.lock(), &ida));
        b.engine.lock().config.peers.retain(|p| p.id != ida);
        assert!(!Engine::accepting(&b.engine.lock(), &ida));
        enter(&a.engine, &idb, 1);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(b.engine.view().controlled_by, None);
    }

    /// 睡前没有设备连着时，醒来不弹提示；那一轮重连试不通也不弹「已暂停自动重试」。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn waking_with_nothing_connected_stays_quiet() {
        let (a, b) = paired("a-quiet", "b-quiet").await;
        let idb = b.engine.local_id().clone();
        b.engine.shutdown();
        wait_for_long("A 发现 B 退出", || {
            !a.engine.lock().sessions.contains_key(&idb)
        })
        .await;
        a.notices.lock().unwrap().clear();
        a.engine.on_resume(Duration::from_secs(600));
        assert!(a.engine.lock().quiet_retry.contains(&idb));
        // 模拟这一轮重试次数用完。
        a.engine.lock().backoff.insert(
            idb.clone(),
            Backoff {
                next: Instant::now(),
                delay: Duration::from_secs(2),
                attempts: MAX_AUTO_ATTEMPTS,
            },
        );
        wait_for_long("停止自动连接", || device(&a.engine, &idb).manual).await;
        let notices = a.notices.lock().unwrap().clone();
        assert!(
            !notices
                .iter()
                .any(|n| n.contains("刚从睡眠中醒来") || n.contains("已暂停自动重试")),
            "{notices:?}"
        );
    }

    /// 对方结束控制后，界面上「键鼠在这里」标到对方那边；本机一有键鼠输入就标回本机。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn input_follows_the_controller_home_until_local_input() {
        let (a, b) = paired("a9", "b9").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());
        // 一开始各自认为键鼠在自己这里。
        assert!(has_input(&b.engine, &idb) && !has_input(&b.engine, &ida));

        enter(&a.engine, &idb, 1);
        wait_for("B 被 A 控制", || {
            b.engine.controlled_by() == Some(ida.clone())
        })
        .await;
        assert!(has_input(&b.engine, &idb), "被控制时键鼠就在 B 这里");

        // A 离开：B 认为键鼠跟着 A 回去了。
        {
            let st = a.engine.lock();
            let _ = st.sessions[&idb].tx.send(Message::Leave { session: 1 });
        }
        wait_for("B 结束被控制", || b.engine.controlled_by().is_none()).await;
        wait_for("键鼠标到 A 那边", || {
            has_input(&b.engine, &ida) && !has_input(&b.engine, &idb)
        })
        .await;
        // 这不等于 B 在控制 A。
        assert!(
            b.engine
                .view()
                .devices
                .iter()
                .all(|d| d.id != ida || !d.active)
        );

        // B 本机动了一下鼠标：标回 B。
        b.engine
            .controller()
            .handle(flow_platform::CapturedEvent::MouseMove {
                x: 100.0,
                y: 100.0,
                dx: 1.0,
                dy: 0.0,
            });
        wait_for("键鼠标回 B", || {
            has_input(&b.engine, &idb) && !has_input(&b.engine, &ida)
        })
        .await;

        // A 再来再走，然后断开：不再标在一台已经离线的设备上。
        enter(&a.engine, &idb, 2);
        wait_for("B 再次被 A 控制", || {
            b.engine.controlled_by() == Some(ida.clone())
        })
        .await;
        {
            let st = a.engine.lock();
            let _ = st.sessions[&idb].tx.send(Message::Leave { session: 2 });
        }
        wait_for("键鼠再次标到 A 那边", || has_input(&b.engine, &ida)).await;
        a.engine.shutdown();
        wait_for("A 离线后标回 B", || {
            has_input(&b.engine, &idb) && !has_input(&b.engine, &ida)
        })
        .await;
    }

    /// 暂停挡住双向控制，锁定只限制本机切出去，对方仍可进入。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_blocks_incoming_but_lock_allows_it() {
        let (a, b) = paired("a7", "b7").await;
        let (ida, idb) = (a.engine.local_id().clone(), b.engine.local_id().clone());

        // B 暂停共享：A 收到通知，显示「对方已暂停」；A 硬切过来也被拒绝。
        let mut s = b.engine.settings();
        s.sharing_paused = true;
        b.engine.update_settings(s.clone());
        wait_for("A 知道 B 暂停了", || remote_paused(&a.engine, &idb)).await;
        enter(&a.engine, &idb, 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(b.engine.controlled_by(), None);

        // 恢复后可以控制。
        s.sharing_paused = false;
        b.engine.update_settings(s.clone());
        wait_for("A 知道 B 恢复了", || !remote_paused(&a.engine, &idb)).await;
        enter(&a.engine, &idb, 2);
        wait_for("B 被 A 控制", || {
            b.engine.controlled_by() == Some(ida.clone())
        })
        .await;

        // 被控制时锁定：正在控制的 A 不受影响。
        b.engine.set_locked(true);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(b.engine.controlled_by(), Some(ida.clone()));
        assert!(!remote_paused(&a.engine, &idb));

        // 被控制时暂停这台设备：立即结束控制，A 也知道了。
        b.engine.set_device_paused(&ida, true);
        wait_for("B 结束被控制", || b.engine.controlled_by().is_none()).await;
        wait_for("A 知道被暂停", || remote_paused(&a.engine, &idb)).await;

        // 取消暂停但仍锁定：A 可以重新进入，锁定不限制对方的键鼠。
        b.engine.set_device_paused(&ida, false);
        wait_for("锁定时对方仍可进入", || {
            !remote_paused(&a.engine, &idb)
        })
        .await;
        enter(&a.engine, &idb, 3);
        wait_for("锁定后 B 仍能被 A 接管", || {
            b.engine.controlled_by() == Some(ida.clone())
        })
        .await;

        // 全局暂停会结束已有控制，即使仍保持锁定。
        s.sharing_paused = true;
        b.engine.update_settings(s.clone());
        assert!(b.engine.controlled_by().is_none());
        wait_for("暂停通知到 A", || remote_paused(&a.engine, &idb)).await;
        enter(&a.engine, &idb, 4);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(b.engine.controlled_by().is_none());
        s.sharing_paused = false;
        b.engine.update_settings(s);
        wait_for("恢复通知到 A", || !remote_paused(&a.engine, &idb)).await;
        enter(&a.engine, &idb, 5);
        wait_for("恢复后 B 再次被 A 控制", || {
            b.engine.controlled_by() == Some(ida.clone())
        })
        .await;
    }

    fn latest_transfer(e: &Engine) -> Option<TransferView> {
        e.view().transfers.into_iter().next()
    }

    /// A 复制文件和文件夹 → B 收到清单 → B 接收，内容一致；A 的剪贴板变了之后再取会失败并提示原因；
    /// 键鼠来到 B 时自动取回，同名文件改名。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copied_files_reach_the_other_device() {
        let (a, b) = paired("a5", "b5").await;
        a.engine.set_locked(true);
        b.engine.set_locked(true);
        assert!(Engine::clipboard_peers(&a.engine.lock()).is_some());
        assert!(Engine::clipboard_peers(&b.engine.lock()).is_some());
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
            .offer_files(vec![report.clone(), photos.clone()], epoch, false)
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
        a.engine
            .clone()
            .offer_files(vec![photos], epoch, false)
            .await;
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
        a.engine
            .clone()
            .offer_files(vec![report], epoch, false)
            .await;
        wait_for("第三份清单", || b.engine.view().file_offer.is_some()).await;
        b.engine.on_arrive();
        assert!(b.engine.view().file_offer.is_none(), "已经开始自动接收");
        wait_for("第三次收完", || {
            recv.join("报告 (1).txt").is_file()
                && latest_transfer(&b.engine).is_some_and(|t| t.status == TransferStatus::Done)
        })
        .await;
    }

    /// A 复制了一张图片（截图）→ B 收到的清单标着"图片" → 收完后按图片处理。
    /// 测试里不碰系统剪贴板，所以写不进去，图片留在接收文件夹里。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copied_image_is_offered_and_received_as_an_image() {
        let (a, b) = paired("img-a", "img-b").await;
        a.engine.set_locked(true);
        b.engine.set_locked(true);
        let shot = a.dir.join(flow_core::files::IMAGE_FILE_NAME);
        let png = b"\x89PNG\r\n\x1a\n fake image body".to_vec();
        std::fs::write(&shot, &png).unwrap();

        let epoch = a.engine.clipboard_epoch();
        a.engine.clone().offer_files(vec![shot], epoch, true).await;
        wait_for("B 收到图片清单", || {
            b.engine.view().file_offer.is_some()
        })
        .await;
        assert_eq!(b.engine.view().file_offer.unwrap().label, "图片");

        b.engine.accept_files();
        wait_for("B 收完图片", || {
            latest_transfer(&b.engine).is_some_and(|t| t.status == TransferStatus::Done)
        })
        .await;
        let t = latest_transfer(&b.engine).unwrap();
        assert!(t.label == "图片" && !t.pasteable);
        let saved = b
            .dir
            .join("received")
            .join(flow_core::files::IMAGE_FILE_NAME);
        assert_eq!(std::fs::read(saved).unwrap(), png);
        assert!(
            b.notices
                .lock()
                .unwrap()
                .iter()
                .any(|n| n.contains("复制的图片"))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_clears_offers_and_blocks_file_and_clipboard_sharing_until_resumed() {
        let (a, b) = paired("pause-offer-a", "pause-offer-b").await;
        let ida = a.engine.local_id().clone();
        let file = a.dir.join("暂停测试.txt");
        std::fs::write(&file, b"pause and resume").unwrap();
        let epoch = a.engine.clipboard_epoch();
        a.engine
            .clone()
            .offer_files(vec![file.clone()], epoch, false)
            .await;
        wait_for("B 收到文件清单", || {
            b.engine.view().file_offer.is_some()
        })
        .await;
        let stale = b
            .engine
            .inner
            .files
            .lock()
            .unwrap()
            .book
            .pending()
            .unwrap()
            .1
            .clone();
        let before = b.engine.clipboard_epoch();
        {
            let mut f = b.engine.inner.files.lock().unwrap();
            f.local = Some((stale.clone(), vec![file.clone()]));
            f.unoffered = Some((vec![file.clone()], before));
        }
        let mut settings = b.engine.settings();
        settings.sharing_paused = true;
        b.engine.update_settings(settings.clone());
        assert!(Engine::clipboard_peers(&b.engine.lock()).is_none());
        assert!(!b.engine.inner.clipboard.lock().unwrap().sync.enabled);
        {
            let f = b.engine.inner.files.lock().unwrap();
            assert!(f.epoch > before);
            assert!(f.local.is_none() && f.unoffered.is_none() && f.book.pending().is_none());
        }
        // 暂停前已排队的自动/手动请求和异步清单整理都不能启动共享。
        b.engine.start_fetch(ida.clone(), stale.clone());
        b.engine.on_file_offer(&ida, stale);
        b.engine.accept_files();
        b.engine.on_arrive();
        b.engine
            .clone()
            .offer_files(vec![file.clone()], before, false)
            .await;
        assert!(b.engine.view().file_offer.is_none());
        assert!(b.engine.view().transfers.is_empty());
        assert!(b.engine.inner.files.lock().unwrap().local.is_none());

        settings.sharing_paused = false;
        b.engine.update_settings(settings);
        assert!(Engine::clipboard_peers(&b.engine.lock()).is_some());
        assert!(b.engine.view().file_offer.is_none(), "恢复后不会重启旧清单");
        a.engine.clone().offer_files(vec![file], epoch, false).await;
        wait_for("恢复后能收到新的复制", || {
            b.engine.view().file_offer.is_some()
        })
        .await;
        b.engine.accept_files();
        wait_for("恢复后传输完成", || {
            latest_transfer(&b.engine).is_some_and(|t| t.status == TransferStatus::Done)
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pausing_either_side_cancels_active_file_transfers() {
        for pause_sender in [false, true] {
            let (a, b) = paired(
                &format!("pause-send-{pause_sender}"),
                &format!("pause-recv-{pause_sender}"),
            )
            .await;
            let file = a.dir.join("传输中.bin");
            std::fs::File::create(&file)
                .unwrap()
                .set_len(256 * 1024 * 1024)
                .unwrap();
            let epoch = a.engine.clipboard_epoch();
            a.engine.clone().offer_files(vec![file], epoch, false).await;
            wait_for("收到传输清单", || {
                b.engine.view().file_offer.is_some()
            })
            .await;
            b.engine.accept_files();
            wait_for("开始传输数据", || {
                latest_transfer(&b.engine)
                    .is_some_and(|t| t.status == TransferStatus::Running && t.done > 0)
            })
            .await;
            let paused = if pause_sender { &a.engine } else { &b.engine };
            let other = if pause_sender { &b.engine } else { &a.engine };
            let mut settings = paused.settings();
            settings.sharing_paused = true;
            paused.update_settings(settings);
            wait_for("暂停方取消传输", || {
                latest_transfer(paused).is_some_and(|t| t.status == TransferStatus::Canceled)
            })
            .await;
            wait_for("另一方也停止传输", || {
                latest_transfer(other).is_some_and(|t| t.status != TransferStatus::Running)
            })
            .await;
            assert!(b.engine.view().file_offer.is_none());
            assert_eq!(
                std::fs::read_dir(b.dir.join("received")).unwrap().count(),
                0,
                "暂停不留下半个文件"
            );
            a.engine.shutdown();
            b.engine.shutdown();
        }
    }

    /// 对方屏幕熄灭后发来空的屏幕列表（旧版本会这样）：本机沿用原来的，否则就切不过去唤醒它了。
    /// 「唤醒屏幕」只对连着的设备可用。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sleeping_peer_keeps_its_place_and_wake_can_be_requested() {
        let (a, b) = paired("wake-a", "wake-b").await;
        let ida = a.engine.local_id().clone();
        let idb = b.engine.local_id().clone();
        let screen = DisplayInfo {
            local_id: "1".into(),
            name: "外接显示器".into(),
            rect: Rect::new(0.0, 0.0, 2560.0, 1440.0),
            scale: 1.0,
            primary: true,
            native: None,
        };
        let send = |m: Message| {
            let st = a.engine.lock();
            st.sessions[&idb].tx.send(m).unwrap();
        };
        let displays_at_b = || {
            let st = b.engine.lock();
            st.config.peer(&ida).unwrap().displays.clone()
        };
        send(Message::Displays(vec![screen.clone()]));
        wait_for("收到屏幕列表", || {
            displays_at_b() == vec![screen.clone()]
        })
        .await;
        // 空列表之后再发一条改名：改名生效时，空列表肯定已经处理过了。
        send(Message::Displays(vec![]));
        send(Message::Profile {
            name: "熄屏的电脑".into(),
        });
        wait_for("改名生效", || {
            b.engine.lock().config.peer(&ida).unwrap().name == "熄屏的电脑"
        })
        .await;
        assert_eq!(displays_at_b(), vec![screen]);

        let can_wake =
            |e: &Engine, id: &DeviceId| e.view().devices.iter().any(|d| &d.id == id && d.can_wake);
        assert!(can_wake(&b.engine, &ida) && !can_wake(&b.engine, &idb));
        assert!(b.engine.wake_device(&ida).is_ok());
        // 本机不是"连着的设备"；断开之后也不能再唤醒。
        assert!(b.engine.wake_device(&idb).is_err());
        a.engine.shutdown();
        wait_for("断开", || !b.engine.lock().sessions.contains_key(&ida)).await;
        assert!(b.engine.wake_device(&ida).is_err() && !can_wake(&b.engine, &ida));
        b.engine.shutdown();
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
        a.engine.clone().offer_files(vec![file], epoch, false).await;
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

    /// 等得比连接的空闲超时（8 秒）久。对方退出时的关闭通知是尽力而为的，偶尔没送到，
    /// 这边要等空闲超时才发现断开，所以"等对方退出被发现"用这个，不用只等 5 秒的 `wait_for`。
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
