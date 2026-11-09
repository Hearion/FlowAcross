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
  active: boolean;
  paused: boolean;
}

export interface NearbyView {
  id: string;
  name: string;
  os: Os;
  /** 正在连接或配对中。 */
  connecting: boolean;
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
  clipboard: boolean;
  /** 共享剪贴板时也传复制的文件。 */
  clipboard_files: boolean;
  sharing_paused: boolean;
  auto_check_updates: boolean;
  /** 低延迟模式（仅 macOS）：有设备连着时关闭 AWDL。 */
  low_latency: boolean;
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
}

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
  local: { id: string; name: string; os: Os };
  onboarded: boolean;
  port: number;
  layout: Layout;
  devices: DeviceView[];
  nearby: NearbyView[];
  pairings: PairingView[];
  settings: Settings;
  locked: boolean;
  permissions: PermissionView[];
  /** 权限相关的额外说明（例如开发模式下权限归属于终端）。 */
  permission_hint: string | null;
  /** 本机正在被哪台设备控制。 */
  controlled_by: string | null;
  low_latency: LowLatencyView;
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
}
