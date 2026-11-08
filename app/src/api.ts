import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { PermissionKind, Settings, StateView } from "./types";
import { createMock } from "./mock";

/** 是否运行在 Tauri 窗口里。直接用浏览器打开 Vite 页面时为 false，改用内存中的演示数据。 */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

type Unlisten = () => void;

export interface Api {
  getState(): Promise<StateView>;
  setDeviceName(name: string): Promise<void>;
  finishOnboarding(): Promise<void>;
  updateSettings(settings: Settings): Promise<void>;
  setLocked(locked: boolean): Promise<void>;
  setDevicePaused(device: string, paused: boolean): Promise<void>;
  /** 失败时以字符串原因 reject。 */
  /** `snap`：吸附距离（布局逻辑点）。 */
  moveDevice(device: string, dx: number, dy: number, snap?: number): Promise<void>;
  pairWith(device: string): Promise<void>;
  respondPairing(device: string, accept: boolean): Promise<void>;
  removeDevice(device: string): Promise<void>;
  /** 手动连接离线的已配对设备。 */
  connectDevice(device: string): Promise<void>;
  requestPermission(kind: PermissionKind): Promise<StateView>;
  openPermissionSettings(kind: PermissionKind): Promise<void>;
  openLogs(): Promise<void>;
  openDataDir(): Promise<void>;
  onState(cb: (s: StateView) => void): Promise<Unlisten>;
  onNotice(cb: (msg: string) => void): Promise<Unlisten>;
}

const tauriApi: Api = {
  getState: () => invoke("get_state"),
  setDeviceName: (name) => invoke("set_device_name", { name }),
  finishOnboarding: () => invoke("finish_onboarding"),
  updateSettings: (settings) => invoke("update_settings", { settings }),
  setLocked: (locked) => invoke("set_locked", { locked }),
  setDevicePaused: (device, paused) => invoke("set_device_paused", { device, paused }),
  moveDevice: (device, dx, dy, snap) => invoke("move_device", { device, dx, dy, snap: snap ?? null }),
  pairWith: (device) => invoke("pair_with", { device }),
  respondPairing: (device, accept) => invoke("respond_pairing", { device, accept }),
  removeDevice: (device) => invoke("remove_device", { device }),
  connectDevice: (device) => invoke("connect_device", { device }),
  requestPermission: (kind) => invoke("request_permission", { kind }),
  openPermissionSettings: (kind) => invoke("open_permission_settings", { kind }),
  openLogs: () => invoke("open_logs"),
  openDataDir: () => invoke("open_data_dir"),
  onState: (cb) => listen<StateView>("state", (e) => cb(e.payload)),
  onNotice: (cb) => listen<string>("notice", (e) => cb(e.payload)),
};

export const api: Api = inTauri ? tauriApi : createMock();
