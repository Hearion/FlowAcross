// 浏览器预览用的内存模拟后端：不在 Tauri 窗口里时使用，方便调样式。行为尽量贴近真实引擎。

import type { Api } from "./api";
import type { StateView, UpdateProgress } from "./types";

export function createMock(): Api {
  // 预览开关：?onboarding 显示首次引导，?solo 只有本机（看空状态）。
  const params = new URLSearchParams(typeof location === "undefined" ? "" : location.search);
  let state: StateView = {
    local: { id: "local", name: "本机（浏览器预览）", os: "MacOs" },
    onboarded: true,
    port: 24830,
    layout: {
      displays: [
        { id: { device: "local", local: "1" }, name: "内建显示器", rect: { x: 0, y: 0, width: 1512, height: 982 }, scale: 2, native: [3024, 1964] },
        { id: { device: "pc", local: "0" }, name: "显示器 1", rect: { x: -2560, y: -458, width: 2560, height: 1440 }, scale: 1.5, native: [3840, 2160] },
        { id: { device: "pc", local: "1" }, name: "显示器 2", rect: { x: -4480, y: -98, width: 1920, height: 1080 }, scale: 1, native: [1920, 1080] },
      ],
    },
    devices: [
      { id: "local", name: "本机（浏览器预览）", os: "MacOs", status: "local", manual: false, active: true, paused: false, remote_paused: false },
      { id: "pc", name: "书房台式机", os: "Windows", status: "connected", manual: false, active: false, paused: false, remote_paused: false },
    ],
    nearby: [{ id: "mini", name: "Mac mini", os: "MacOs", connecting: false }],
    pairings: [],
    settings: { hold_key: "none", dwell_ms: 0, corner_guard: true, mapping: "Continuous", clipboard: true, clipboard_files: true, sharing_paused: false, auto_check_updates: true, low_latency: false, win_keys: "windows", win_keys_custom: { ctrl: "command", win: "control", alt: "option" } },
    locked: false,
    permissions: [
      { kind: "accessibility", state: "granted" },
      { kind: "input_monitoring", state: "denied" },
    ],
    permission_hint: null,
    controlled_by: null,
    low_latency: { supported: true, installed: false, active: false },
    about: { version: "0.1.0", protocol: 1, data_dir: "~/Library/Application Support/app.flowacross.desktop", fingerprint: "3F2A 91C0 7B44 E1D9 0A6C 55F2 8E13 B7A0" },
    transfers: [],
    file_offer: null,
    receive_dir: "~/Downloads/流界",
    config_warning: null,
  };
  if (params.has("onboarding")) state.onboarded = false;
  // ?config-error：看配置文件损坏的提示。
  if (params.has("config-error"))
    state.config_warning =
      "配置文件损坏，无法读取（expected value at line 1 column 1）。已把原文件备份为 ~/Library/Application Support/app.flowacross.desktop/config.json.bad-1794235000，配对的设备和布局需要重新设置。";
  // ?files：看跨设备复制文件的各种状态。
  if (params.has("files")) {
    state.file_offer = { peer: "pc", peer_name: "书房台式机", label: "项目素材 等 3 项", files: 214, bytes: 3_480_000_000 };
    state.transfers = [
      { id: 3, peer: "pc", peer_name: "书房台式机", incoming: true, label: "季度报告.pdf 等 2 项", done: 31_400_000, total: 86_000_000, status: "running", error: null, pasteable: false },
      { id: 2, peer: "pc", peer_name: "书房台式机", incoming: false, label: "截图 2026-11-08.png", done: 2_100_000, total: 2_100_000, status: "done", error: null, pasteable: false },
      { id: 1, peer: "pc", peer_name: "书房台式机", incoming: true, label: "照片", done: 120_000_000, total: 512_000_000, status: "failed", error: "磁盘空间不足，请清理后重试。", pasteable: false },
    ];
  }
  if (params.has("solo")) {
    state.devices = state.devices.filter((d) => d.id === "local");
    state.layout = { displays: state.layout.displays.filter((d) => d.id.device === "local") };
  }
  const stateListeners = new Set<(s: StateView) => void>();
  const noticeListeners = new Set<(m: string) => void>();
  const set = (next: Partial<StateView>) => {
    state = { ...state, ...next };
    stateListeners.forEach((cb) => cb(structuredClone(state)));
  };
  const notice = (m: string) => noticeListeners.forEach((cb) => cb(m));
  const progressListeners = new Set<(p: UpdateProgress) => void>();
  // ?autostart-error：看开机启动开关失败时的提示。
  let autostart = false;
  const mockUpdate ={ version: "0.2.0", current: "0.1.0", notes: "（浏览器预览）示例更新说明：\n- 修复断网后偶尔卡键\n- 布局图支持缩放", date: "2026-11-09" };

  return {
    getState: async () => structuredClone(state),
    setDeviceName: async (name) => {
      const n = name.trim() || "本机（浏览器预览）";
      set({ local: { ...state.local, name: n }, devices: state.devices.map((d) => (d.id === "local" ? { ...d, name: n } : d)) });
    },
    finishOnboarding: async () => set({ onboarded: true }),
    updateSettings: async (settings) => set({ settings }),
    setLocked: async (locked) => set({ locked }),
    setDevicePaused: async (device, paused) =>
      set({ devices: state.devices.map((d) => (d.id === device ? { ...d, paused } : d)) }),
    moveDevice: async (device, dx, dy) =>
      set({
        layout: {
          displays: state.layout.displays.map((d) =>
            d.id.device === device ? { ...d, rect: { ...d.rect, x: Math.round(d.rect.x + dx), y: Math.round(d.rect.y + dy) } } : d,
          ),
        },
      }),
    pairWith: async (device) => {
      const n = state.nearby.find((d) => d.id === device);
      if (!n) return;
      set({
        pairings: [{ id: n.id, name: n.name, os: n.os, code: "482 913", outgoing: true, local_confirmed: false, remote_confirmed: false }],
      });
    },
    respondPairing: async (device, accept) => {
      const p = state.pairings.find((x) => x.id === device);
      if (!p) return;
      if (!accept) {
        set({ pairings: [] });
        return;
      }
      set({ pairings: [{ ...p, local_confirmed: true }] });
      setTimeout(() => {
        const right = Math.max(...state.layout.displays.map((d) => d.rect.x + d.rect.width));
        set({
          pairings: [],
          nearby: state.nearby.filter((d) => d.id !== device),
          devices: [...state.devices, { id: p.id, name: p.name, os: p.os, status: "connected", manual: false, active: false, paused: false, remote_paused: false }],
          layout: {
            displays: [
              ...state.layout.displays,
              { id: { device: p.id, local: "0" }, name: "Studio Display", rect: { x: right, y: 0, width: 2560, height: 1440 }, scale: 2, native: [5120, 2880] },
            ],
          },
        });
        notice(`已与「${p.name}」配对，之后两台电脑会自动连接。`);
      }, 900);
    },
    connectDevice: async (device) =>
      set({ devices: state.devices.map((d) => (d.id === device ? { ...d, status: "connected", manual: false } : d)) }),
    removeDevice: async (device) =>
      set({
        devices: state.devices.filter((d) => d.id !== device),
        layout: { displays: state.layout.displays.filter((d) => d.id.device !== device) },
      }),
    requestPermission: async (kind) => {
      set({ permissions: state.permissions.map((p) => (p.kind === kind ? { ...p, state: "granted" } : p)) });
      return structuredClone(state);
    },
    openPermissionSettings: async () => notice("（浏览器预览）这里会打开系统设置"),
    openLogs: async () => notice("（浏览器预览）这里会打开日志文件夹"),
    openDataDir: async () => notice("（浏览器预览）这里会打开数据目录"),
    checkUpdate: () => new Promise((resolve) => setTimeout(() => resolve(mockUpdate), 600)),
    installUpdate: () =>
      new Promise((_, reject) => {
        const total = 12_000_000;
        let downloaded = 0;
        const timer = setInterval(() => {
          downloaded = Math.min(total, downloaded + 1_500_000);
          progressListeners.forEach((cb) => cb({ downloaded, total }));
          if (downloaded >= total) {
            clearInterval(timer);
            reject({ message: "（浏览器预览）这里会安装更新并重新打开流界", restart: false });
          }
        }, 250);
      }),
    restartApp: async () => notice("（浏览器预览）这里会重新打开流界"),
    setLowLatency: async (enabled) => {
      await new Promise((r) => setTimeout(r, 500));
      const connected = state.devices.some((d) => d.status === "connected");
      set({
        settings: { ...state.settings, low_latency: enabled },
        low_latency: { supported: true, installed: state.low_latency.installed || enabled, active: enabled && connected },
      });
    },
    removeLowLatency: async () =>
      set({ settings: { ...state.settings, low_latency: false }, low_latency: { supported: true, installed: false, active: false } }),
    acceptFiles: async () => {
      const offer = state.file_offer;
      if (!offer) return;
      const id = Date.now();
      set({
        file_offer: null,
        transfers: [
          { id, peer: offer.peer, peer_name: offer.peer_name, incoming: true, label: offer.label, done: 0, total: offer.bytes, status: "running", error: null, pasteable: false },
          ...state.transfers,
        ],
      });
      const timer = setInterval(() => {
        const t = state.transfers.find((x) => x.id === id);
        if (!t || t.status !== "running") return clearInterval(timer);
        const done = Math.min(t.total, t.done + t.total / 12);
        const finished = done >= t.total;
        set({ transfers: state.transfers.map((x) => (x.id === id ? { ...x, done, status: finished ? "done" : "running", pasteable: finished } : x)) });
        if (finished) {
          clearInterval(timer);
          notice(`已收到「${t.peer_name}」的「${t.label}」，可以直接粘贴。`);
        }
      }, 250);
    },
    cancelTransfer: async (id) =>
      set({ transfers: state.transfers.map((t) => (t.id === id && t.status === "running" ? { ...t, status: "canceled" } : t)) }),
    dismissTransfer: async (id) => set({ transfers: state.transfers.filter((t) => t.id !== id || t.status === "running") }),
    openReceivedFiles: async () => notice("（浏览器预览）这里会打开下载文件夹里的「流界」"),
    dismissConfigWarning: async () => set({ config_warning: null }),
    getAutostart: async () => autostart,
    setAutostart: async (enabled) => {
      await new Promise((r) => setTimeout(r, 300));
      if (params.has("autostart-error")) {
        throw "（浏览器预览）开启开机启动失败（拒绝访问）。可能是安全软件拦截了修改启动项，请在安全软件里放行流界，或打开「任务管理器 › 启动应用」手动设置 FlowAcross。";
      }
      autostart = enabled;
      return autostart;
    },
    // 每次订阅包一层新函数：React 开发模式会"订阅-退订-再订阅"同一个回调，直接存回调会被误删。
    onState: async (cb) => {
      const h = (st: StateView) => cb(st);
      stateListeners.add(h);
      return () => stateListeners.delete(h);
    },
    onNotice: async (cb) => {
      const h = (m: string) => cb(m);
      noticeListeners.add(h);
      return () => noticeListeners.delete(h);
    },
    // 浏览器预览里不会自动推送新版本，在设置页点「检查更新」即可看到示例。
    onUpdateAvailable: async () => () => {},
    onUpdateProgress: async (cb) => {
      const h = (p: UpdateProgress) => cb(p);
      progressListeners.add(h);
      return () => progressListeners.delete(h);
    },
  };
}
