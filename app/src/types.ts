// 与 Rust 端 serde 序列化结构一一对应（flow-core / src-tauri/src/engine.rs / config.rs）。

export type Os = "MacOs" | "Windows" | "Linux" | "Other";

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface DisplayId {
  device: string;
  local: string;
}

export interface Display {
  id: DisplayId;
  name: string;
  rect: Rect;
  scale: number;
  /** 屏幕物理分辨率（macOS 为面板原生分辨率，Windows 为当前分辨率）。 */
  native: [number, number] | null;
}

export interface Layout {
  displays: Display[];
}

export type DeviceStatus = "local" | "connected" | "connecting" | "offline";

export interface DeviceView {
  id: string;
  name: string;
  os: Os;
  status: DeviceStatus;
  /** 自动重连已停止，需要手动连接。 */
  manual: boolean;
  /** 本机的键鼠当前在这台设备上（对远端设备来说就是"正在控制它"）。 */
  active: boolean;
  /**
   * 「键鼠在这里」标在这台设备上。通常和 `active` 一样；例外是别的设备刚结束对本机的控制，
   * 键鼠跟着它回去了，这时标在它上面，直到本机有键鼠输入。
   */
  has_input: boolean;
  paused: boolean;
  /** 对方暂停了共享或暂停了本机，暂时不能切过去。 */
  remote_paused: boolean;
  /** 可以请它点亮屏幕：连着，而且对方的版本支持。 */
  can_wake: boolean;
  /** 对方停在锁屏界面（它自己报告的；旧版本不报告）。 */
  locked: boolean;
  /** 现在可以把键鼠切到这台设备（本机这一项表示可以回到本机）。 */
  can_switch: boolean;
  /** 快捷键映射可以把按键发给它：连着，而且对方的版本支持（0.2.4 起）。 */
  can_send_keys: boolean;
  /** 到这台设备的网络延迟（往返时间，毫秒；不到 1 毫秒时是 0）。没连着、本机这一项为 null。 */
  latency_ms: number | null;
  latency_level: LatencyLevel | null;
  /** 没连上的原因和处理办法；连着、或没有可说的时为 null。 */
  problem: ProblemView | null;
}

/** 延迟算好、一般还是差（flow-core autolock::latency_level）。 */
export type LatencyLevel = "good" | "fair" | "poor";

/** 键鼠被自动锁定在本机的原因：名单里的应用在前台，或前台窗口全屏。`app` 是那个应用的名字。 */
export interface AutoLock {
  kind: "app" | "fullscreen";
  app: string;
}

/** 设备连不上 / 离线的原因和按顺序排查的步骤。 */
export interface ProblemView {
  summary: string;
  steps: string[];
}

export interface NearbyView {
  id: string;
  name: string;
  os: Os;
  /** 正在连接或配对中。 */
  connecting: boolean;
  /** 上次配对没连上的原因和处理办法。 */
  problem: ProblemView | null;
}

export interface PairingView {
  id: string;
  name: string;
  os: Os;
  code: string;
  outgoing: boolean;
  local_confirmed: boolean;
  remote_confirmed: boolean;
}

export type PermissionKind = "accessibility" | "input_monitoring" | "local_network";
export type PermissionState = "granted" | "denied" | "not_determined" | "not_applicable";

export interface PermissionView {
  kind: PermissionKind;
  state: PermissionState;
}

export type HoldKey = "none" | "ctrl" | "alt" | "shift";
export type EdgeMapping = "Continuous" | "Proportional";

export interface Settings {
  hold_key: HoldKey;
  dwell_ms: number;
  corner_guard: boolean;
  mapping: EdgeMapping;
  /** 键鼠从别的电脑来到本机时，在进来的那条屏幕边上闪一道浮光。 */
  edge_glow: boolean;
  clipboard: boolean;
  /** 共享剪贴板时也传复制的文件。 */
  clipboard_files: boolean;
  sharing_paused: boolean;
  auto_check_updates: boolean;
  /** 低延迟模式（仅 macOS）：有设备连着时关闭 AWDL。 */
  low_latency: boolean;
  /** 被 PC 键盘控制时（本机是 Mac）Ctrl、Win、Alt 怎么对应。 */
  win_keys: WinKeysPreset;
  /** `win_keys` 为 "custom" 时的对应关系。 */
  win_keys_custom: WinToMacKeys;
  /** 控制其他电脑时滚轮的速度，百分比：100 是原样，200 是两倍。在手边这台电脑上设置。 */
  wheel_speed: number;
  /** 有设备连着时，不让这台电脑的屏幕自动熄灭。 */
  keep_awake: boolean;
  /** 这台电脑上的快捷键。 */
  hotkeys: Hotkeys;
  /** 设备布局在各台电脑之间同步：在任意一台上拖动屏幕，其他电脑跟着变。 */
  sync_layout: boolean;
  /** 前台窗口全屏时自动把键鼠锁定在本机。 */
  auto_lock_fullscreen: boolean;
  /** 这些应用在前台时自动把键鼠锁定在本机（应用名，不分大小写）。 */
  auto_lock_apps: string[];
}

/** 一组按键（flow-core hotkeys::KeyCombo）：`mods` 是修饰键位掩码（见 keys.ts 的 MOD），`key` 是 HID 键码。 */
export interface KeyCombo {
  mods: number;
  key: number;
}

/** 这台电脑上的快捷键（flow-core hotkeys::Hotkeys）。只有物理键盘所在的电脑看得到按键，各台电脑各设各的。 */
export interface Hotkeys {
  /** 切到左边相邻的电脑；null 表示不用。 */
  switch_left: KeyCombo | null;
  switch_right: KeyCombo | null;
  /** 切到指定的电脑（可以是本机：回到本机）。 */
  devices: { device: string; combo: KeyCombo }[];
  /** 快捷键映射：按下 trigger，让 device 收到 send。 */
  mappings: KeyMapping[];
}

export interface KeyMapping {
  trigger: KeyCombo;
  send: KeyCombo;
  device: string;
}

export type MacModifier = "control" | "option" | "command";
export type WinKeysPreset = "windows" | "mac_position" | "direct" | "custom";

/** PC 键盘的 Ctrl、Win、Alt 分别当作 Mac 的哪个键（flow-core keys::WinToMacKeys）。 */
export interface WinToMacKeys {
  ctrl: MacModifier;
  win: MacModifier;
  alt: MacModifier;
}

/** 当前系统（桌面会话）做不到的切换方式和效果。`true` 表示在这台电脑上用不了，对应的设置不显示。 */
export interface UnsupportedView {
  /** 「按住按键才跨屏」。 */
  hold_key: boolean;
  /** 在这台电脑上按快捷键切到别的电脑（切过去之后用它切回来不受影响）。 */
  local_hotkey: boolean;
  /** 「贴边停留」。 */
  dwell: boolean;
  /** 边缘浮光。 */
  edge_glow: boolean;
  /** 按前台应用 / 全屏自动锁定在本机（查不到前台是哪个应用）。 */
  auto_lock: boolean;
}

export interface LowLatencyView {
  /** 本机系统是否支持（只有 macOS）。 */
  supported: boolean;
  /** 后台服务是否已安装。 */
  installed: boolean;
  /** 现在是否正在关闭 AWDL。 */
  active: boolean;
}

/** 检查到的新版本（src-tauri/src/updater.rs）。 */
export interface UpdateInfo {
  version: string;
  current: string;
  notes: string | null;
  date: string | null;
  /** 这份流界的安装方式，决定更新怎么装。 */
  kind: InstallKind;
  /** 不能自动安装（Linux 上认不出安装方式）：改为显示「打开下载页」。 */
  manual: boolean;
}

/** bundled：Windows 安装包 / macOS 应用；其余是 Linux 的三种安装包和认不出来的情况。 */
export type InstallKind = "bundled" | "app_image" | "deb" | "pacman" | "manual";

export interface UpdateProgress {
  downloaded: number;
  total: number | null;
}

/** 安装更新失败。`restart` 为 true 表示键鼠共享已停止，需要重新打开流界。 */
export interface UpdateError {
  message: string;
  restart: boolean;
}

export type TransferStatus = "running" | "done" | "failed" | "canceled";

/** 一次跨设备复制文件（src-tauri/src/transfer.rs）。 */
export interface TransferView {
  id: number;
  peer: string;
  peer_name: string;
  /** true：从对方接收；false：发给对方。 */
  incoming: boolean;
  /** 「报告.pdf」或「报告.pdf 等 3 项」。 */
  label: string;
  done: number;
  total: number;
  status: TransferStatus;
  error: string | null;
  /** 接收完成并已写入本机剪贴板，可以直接粘贴。 */
  pasteable: boolean;
}

/** 其他设备复制了文件、等待手动接收（太大，没有自动接收）。 */
export interface FileOfferView {
  peer: string;
  peer_name: string;
  label: string;
  files: number;
  bytes: number;
}

export interface StateView {
  /** 这份状态的序号，越大越新。并发推送时先算出来的可能后到，界面丢掉比手里旧的。 */
  seq: number;
  local: { id: string; name: string; os: Os };
  onboarded: boolean;
  port: number;
  layout: Layout;
  devices: DeviceView[];
  nearby: NearbyView[];
  pairings: PairingView[];
  settings: Settings;
  locked: boolean;
  /** 键鼠被自动锁定在本机的原因；没有自动锁定时为 null。和手动的 `locked` 互不影响。 */
  auto_lock: AutoLock | null;
  /** 最近在前台的应用（最新的在前），往自动锁定的名单里加应用时挑选。 */
  recent_apps: string[];
  permissions: PermissionView[];
  /** 权限相关的额外说明（例如开发模式下权限归属于终端）。 */
  permission_hint: string | null;
  /** 本机正在被哪台设备控制。 */
  controlled_by: string | null;
  low_latency: LowLatencyView;
  unsupported: UnsupportedView;
  /** 这台电脑上做不到「保持屏幕常亮」的原因；做得到时为 null。 */
  keep_awake_problem: string | null;
  about: {
    version: string;
    protocol: number;
    data_dir: string;
    fingerprint: string;
  };
  /** 正在进行和最近结束的文件传输，最新的在前。 */
  transfers: TransferView[];
  file_offer: FileOfferView | null;
  /** 收到的文件保存在这里；null 表示找不到下载文件夹。 */
  receive_dir: string | null;
  /** 启动时配置文件损坏的说明（原因和备份位置）；用户点「知道了」前一直显示。 */
  config_warning: string | null;
  /** 系统的授权弹窗正在等用户点（只有 Linux 的 GNOME / KDE 会有），显示倒计时。 */
  auth_prompt: AuthPromptView | null;
  /** 没拿到授权的事：提示并给「重新授权」。 */
  auth_denied: AuthKind[];
}

/** capture：捕获这台电脑的键鼠（切到别的电脑要用）；inject：控制这台电脑的键鼠（别的电脑过来要用）。 */
export type AuthKind = "capture" | "inject";

export interface AuthPromptView {
  kind: AuthKind;
  /** 收到这份状态时还剩多少秒；到时间没点，流界会把系统弹窗关掉。 */
  seconds_left: number;
}
