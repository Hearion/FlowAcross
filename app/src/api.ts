import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { PermissionKind, Settings, StateView, UpdateInfo, UpdateProgress } from "./types";
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
  /** `snap`：吸附距离（布局逻辑点）。失败时以字符串原因 reject。 */
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
  /** 没有新版本时返回 null；失败时以字符串原因 reject。 */
  checkUpdate(): Promise<UpdateInfo | null>;
  /** 下载并安装最近一次检查到的版本，成功后程序会重新打开。失败时以 `UpdateError` reject。 */
  installUpdate(): Promise<void>;
  restartApp(): Promise<void>;
  /** 开启 / 关闭低延迟模式；第一次开启会弹出系统管理员授权。失败时以字符串原因 reject。 */
  setLowLatency(enabled: boolean): Promise<void>;
  /** 关闭低延迟模式并移除后台服务。失败时以字符串原因 reject。 */
  removeLowLatency(): Promise<void>;
  /** 接收等待中的文件（`StateView.file_offer`）。 */
  acceptFiles(): Promise<void>;
  cancelTransfer(id: number): Promise<void>;
  /** 从列表中移除已结束的传输。 */
  dismissTransfer(id: number): Promise<void>;
  /** 打开收到的文件所在的文件夹。失败时以字符串原因 reject。 */
  openReceivedFiles(): Promise<void>;
  /** 用户看过配置损坏的提示了。 */
  dismissConfigWarning(): Promise<void>;
  /** 系统里是否已注册开机启动。失败时以字符串原因 reject。 */
  getAutostart(): Promise<boolean>;
  /** 开启 / 关闭开机启动，返回系统里实际的注册状态。失败时以字符串原因 reject。 */
  setAutostart(enabled: boolean): Promise<boolean>;
  onState(cb: (s: StateView) => void): Promise<Unlisten>;
  onNotice(cb: (msg: string) => void): Promise<Unlisten>;
  onUpdateAvailable(cb: (info: UpdateInfo) => void): Promise<Unlisten>;
  onUpdateProgress(cb: (p: UpdateProgress) => void): Promise<Unlisten>;
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
  checkUpdate: () => invoke("check_update"),
  installUpdate: () => invoke("install_update"),
  restartApp: () => invoke("restart_app"),
  setLowLatency: (enabled) => invoke("set_low_latency", { enabled }),
  removeLowLatency: () => invoke("remove_low_latency"),
  acceptFiles: () => invoke("accept_files"),
  cancelTransfer: (id) => invoke("cancel_transfer", { id }),
  dismissTransfer: (id) => invoke("dismiss_transfer", { id }),
  openReceivedFiles: () => invoke("open_received_files"),
  dismissConfigWarning: () => invoke("dismiss_config_warning"),
  getAutostart: () => invoke("get_autostart"),
  setAutostart: (enabled) => invoke("set_autostart", { enabled }),
  onState: (cb) => listen<StateView>("state", (e) => cb(e.payload)),
  onNotice: (cb) => listen<string>("notice", (e) => cb(e.payload)),
  onUpdateAvailable: (cb) => listen<UpdateInfo>("update-available", (e) => cb(e.payload)),
  onUpdateProgress: (cb) => listen<UpdateProgress>("update-progress", (e) => cb(e.payload)),
};

export const api: Api = inTauri ? tauriApi : createMock();
