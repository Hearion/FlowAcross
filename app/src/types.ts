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
  sharing_paused: boolean;
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
  about: {
    version: string;
    protocol: number;
    data_dir: string;
    fingerprint: string;
  };
}
