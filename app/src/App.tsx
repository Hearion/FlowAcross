import { useCallback, useEffect, useRef, useState } from "react";
import { api, inTauri } from "./api";
import { AddDeviceDialog, ConfirmDialog, OS_TEXT, PERMISSION_TEXT, PairingDialog } from "./Dialogs";
import { LayoutMap } from "./LayoutMap";
import { Onboarding } from "./Onboarding";
import { ClipboardSetting, CrossScreenSettings } from "./SettingsControls";
import { SettingsPage, type SettingsSection } from "./SettingsPage";
import type { DeviceView, Settings, StateView } from "./types";
import { UpdateBanner, useUpdater } from "./Updater";
import { ComputerIcon, CursorIcon, GearIcon, LockIcon, Logo, PauseIcon, PlusIcon, hueOf, isMac, resolution } from "./ui";

function statusChip(d: DeviceView) {
  if (d.active) return <span className="chip accent">控制中</span>;
  if (d.status === "local") return <span className="chip">本机</span>;
  if (d.paused) return <span className="chip muted">已暂停</span>;
  if (d.status === "connected") return <span className="chip ok">在线</span>;
  if (d.status === "connecting") return <span className="chip">连接中</span>;
  if (d.manual) return <span className="chip muted">已断开</span>;
  return <span className="chip muted">离线</span>;
}

interface Toast {
  id: number;
  text: string;
}

export default function App() {
  const [state, setState] = useState<StateView | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<DeviceView | null>(null);
  const [settingsSection, setSettingsSection] = useState<SettingsSection | null>(null);
  const toastId = useRef(0);
  const updater = useUpdater();

  const toast = useCallback((text: string) => {
    const id = ++toastId.current;
    setToasts((t) => [...t.slice(-2), { id, text }]);
    window.setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 4200);
  }, []);

  useEffect(() => {
    const unsubs: Promise<() => void>[] = [api.onState(setState), api.onNotice(toast)];
    api.getState().then(setState);
    // 从系统设置授权回来后刷新权限状态。
    const onFocus = () => api.getState().then(setState);
    window.addEventListener("focus", onFocus);
    return () => {
      window.removeEventListener("focus", onFocus);
      unsubs.forEach((p) => p.then((f) => f()));
    };
  }, [toast]);

  // 配对开始后关闭"添加设备"对话框，避免两个弹窗叠在一起。
  const pairingCount = state?.pairings.length ?? 0;
  useEffect(() => {
    if (pairingCount > 0) setAdding(false);
  }, [pairingCount]);

  if (!state) return <div className="loading">正在启动…</div>;

  const s = state.settings;
  const update = (patch: Partial<Settings>) => api.updateSettings({ ...s, ...patch });
  const active = state.devices.find((d) => d.active);
  const controller = state.controlled_by ? state.devices.find((d) => d.id === state.controlled_by) : undefined;
  const sel = state.devices.find((d) => d.id === selected) ?? null;
  const selDisplays = sel ? state.layout.displays.filter((d) => d.id.device === sel.id) : [];
  const missingPerms = state.permissions.filter((p) => p.state !== "granted" && p.state !== "not_applicable");
  const onlyLocal = state.devices.length === 1;
  const pairing = state.pairings[0];

  const handleDrop = (device: string, dx: number, dy: number, snap: number) => {
    api.moveDevice(device, dx, dy, snap).catch((e) => toast(String(e)));
  };

  return (
    <div className={`app ${inTauri && isMac ? "mac-overlay" : ""}`}>
      <header className="topbar" data-tauri-drag-region>
        <div className="brand" data-tauri-drag-region>
          <Logo />
          <span className="brand-name">流界</span>
        </div>

        <div className={`status ${s.sharing_paused ? "paused" : ""}`}>
          <span className="status-live">
            <span className="pulse" />
            {s.sharing_paused ? "已暂停" : state.locked ? "已锁定" : "共享中"}
          </span>
          <span className="status-sep" />
          <span className="status-where">
            <CursorIcon size={12} />
            {controller ? (
              <>
                正在被<strong>{controller.name}</strong>控制
              </>
            ) : (
              <>
                键鼠在<strong>{active?.name ?? state.local.name}</strong>
              </>
            )}
          </span>
        </div>

        <div className="top-actions">
          <button className={`btn ${state.locked ? "on" : ""}`} onClick={() => api.setLocked(!state.locked)}>
            <LockIcon />
            {state.locked ? "已锁定" : "锁定在本机"}
          </button>
          <button className={`btn ${s.sharing_paused ? "on" : ""}`} onClick={() => update({ sharing_paused: !s.sharing_paused })}>
            <PauseIcon />
            {s.sharing_paused ? "恢复共享" : "暂停共享"}
          </button>
          <button
            className={`btn icon ${settingsSection ? "on" : ""}`}
            title="设置"
            aria-label="设置"
            onClick={() => setSettingsSection(settingsSection ? null : "general")}
          >
            <GearIcon />
            {missingPerms.length > 0 && <span className="badge-dot" />}
          </button>
        </div>
      </header>

      {settingsSection ? (
        <SettingsPage
          state={state}
          updater={updater}
          section={settingsSection}
          onSection={setSettingsSection}
          onClose={() => setSettingsSection(null)}
        />
      ) : (
      <main className="content">
        <section className="stage">
          <div className="stage-head">
            <h1>设备布局</h1>
            <p>按真实桌面的摆放拖动屏幕，相贴的边就是鼠标跨过去的地方。</p>
          </div>

          {missingPerms.length > 0 && (
            <div className="banner">
              <span>
                还缺少「{missingPerms.map((p) => PERMISSION_TEXT[p.kind].name).join("」「")}」权限，暂时无法控制其他电脑。
              </span>
              <button className="btn small primary" onClick={() => setSettingsSection("permissions")}>
                去授权
              </button>
            </div>
          )}

          <UpdateBanner updater={updater} />

          <LayoutMap
            layout={state.layout}
            devices={state.devices}
            selected={selected}
            onSelect={setSelected}
            onDrop={handleDrop}
            reserveBottom={onlyLocal ? 130 : 0}
          />

          {onlyLocal && (
            <div className="map-empty">
              <strong>还没有连接其他电脑</strong>
              <span>在另一台电脑上安装并打开流界，然后点「添加」完成配对。</span>
              <button className="btn primary" onClick={() => setAdding(true)}>
                <PlusIcon />
                添加设备
              </button>
            </div>
          )}

          <div className="toasts">
            {toasts.map((t) => (
              <div key={t.id} className="toast">
                {t.text}
              </div>
            ))}
          </div>
        </section>

        <aside className="side">
          <section className="panel">
            <div className="panel-head">
              <h2>
                设备 <span className="count">{state.devices.length}</span>
              </h2>
              <button className="btn small" onClick={() => setAdding(true)}>
                <PlusIcon />
                添加
                {state.nearby.length > 0 && <span className="badge-dot" />}
              </button>
            </div>
            <ul className="device-list">
              {state.devices.map((d) => {
                const screens = state.layout.displays.filter((x) => x.id.device === d.id).length;
                return (
                  <li key={d.id}>
                    <button
                      type="button"
                      className={`device-row ${selected === d.id ? "selected" : ""} ${d.status}`}
                      onClick={() => setSelected(selected === d.id ? null : d.id)}
                    >
                      <span className={`device-icon hue-${hueOf(state.devices, d.id)}`}>
                        <ComputerIcon />
                      </span>
                      <span className="device-info">
                        <span className="device-name">{d.name}</span>
                        <span className="device-sub">
                          {OS_TEXT[d.os]} · {screens} 块屏幕
                        </span>
                      </span>
                      {statusChip(d)}
                    </button>
                  </li>
                );
              })}
            </ul>

            {sel && (
              <div className="device-detail">
                {sel.status === "local" && <RenameField current={sel.name} />}
                <ul className="display-list">
                  {selDisplays.length === 0 && <li className="muted">还没有收到屏幕信息</li>}
                  {selDisplays.map((d) => (
                    <li key={d.id.local}>
                      <span>{d.name}</span>
                      <span className="muted">{resolution(d, sel.os, true)}</span>
                    </li>
                  ))}
                </ul>
                {sel.status === "offline" && (
                  <p className="detail-note">
                    {sel.manual
                      ? "多次自动连接没有成功，已停止自动连接。确认对方的流界已打开后点「连接」。"
                      : "正在等待对方上线，会自动重新连接。"}
                  </p>
                )}
                {sel.status !== "local" && (
                  <div className="detail-actions">
                    {sel.status === "offline" && (
                      <button className="btn small primary" onClick={() => api.connectDevice(sel.id)}>
                        连接
                      </button>
                    )}
                    <button className="btn small" onClick={() => api.setDevicePaused(sel.id, !sel.paused)}>
                      {sel.paused ? "恢复此设备" : "暂停此设备"}
                    </button>
                    <button className="btn small danger" onClick={() => setRemoving(sel)}>
                      移除设备
                    </button>
                  </div>
                )}
              </div>
            )}
          </section>

          <section className="panel">
            <div className="panel-head">
              <h2>跨屏</h2>
              <button className="btn small ghost-link" onClick={() => setSettingsSection("crossing")}>
                更多设置
              </button>
            </div>
            <CrossScreenSettings settings={s} update={update} />
          </section>

          <section className="panel">
            <ClipboardSetting settings={s} update={update} title />
          </section>
        </aside>
      </main>
      )}

      {!state.onboarded && <Onboarding state={state} />}
      {adding && <AddDeviceDialog nearby={state.nearby} onClose={() => setAdding(false)} />}
      {removing && (
        <ConfirmDialog
          title={`移除「${removing.name}」？`}
          body="移除后两台电脑都会删除彼此的配对记录，不再自动连接。之后想再用需要重新配对。"
          confirm="移除"
          danger
          onCancel={() => setRemoving(null)}
          onConfirm={() => {
            api.removeDevice(removing.id);
            setRemoving(null);
            setSelected(null);
          }}
        />
      )}
      {pairing && <PairingDialog pairing={pairing} />}
    </div>
  );
}

function RenameField({ current }: { current: string }) {
  const [value, setValue] = useState(current);
  useEffect(() => setValue(current), [current]);
  const commit = () => {
    if (value.trim() !== current) api.setDeviceName(value);
  };
  return (
    <label className="field compact">
      <span>设备名称</span>
      <input
        value={value}
        maxLength={40}
        onChange={(e) => setValue(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
      />
    </label>
  );
}
