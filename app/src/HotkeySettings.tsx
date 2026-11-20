// 设置 › 快捷键：切到左右相邻的电脑、切到指定的电脑、快捷键映射。

import { useEffect, useRef, useState } from "react";
import { api } from "./api";
import { TRIGGER_RULE, comboFromEvent, comboLabels, isModifierCode, isTrigger, sameCombo } from "./keys";
import type { DeviceView, Hotkeys, KeyCombo, KeyMapping, StateView } from "./types";
import { PlusIcon, isMac } from "./ui";

/** 一组按键的键帽。`mac`：按哪个系统的习惯写修饰键（默认手边这台电脑）。 */
export function ComboKeys({ combo, mac = isMac }: { combo: KeyCombo; mac?: boolean }) {
  return (
    <span className="keys">
      {comboLabels(combo, mac).map((k, i) => (
        <kbd key={i}>{k}</kbd>
      ))}
    </span>
  );
}

interface RecorderProps {
  value: KeyCombo | null;
  /** `null`：清除。 */
  onChange: (c: KeyCombo | null) => void;
  /** 触发键要带修饰键、不能和别的快捷键重复；发送的键没有这些限制。 */
  kind: "trigger" | "send";
  /** 这组键已经用在别处时返回用在哪里（只对触发键检查）。 */
  conflict?: (c: KeyCombo) => string | null;
  /** 键帽按哪个系统的习惯显示。 */
  mac?: boolean;
  label: string;
  placeholder?: string;
}

/**
 * 录制一组按键：点一下开始，按下想要的组合键即可；Esc 取消，退格 / Delete 清除。
 * 录制期间通知引擎暂停响应快捷键，否则刚按下的组合键（比如 Ctrl+Alt+→）会直接把键鼠切走。
 */
export function KeyRecorder({ value, onChange, kind, conflict, mac, label, placeholder = "未设置" }: RecorderProps) {
  const [recording, setRecording] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // 界面每次收到新状态都会重新渲染、换一批回调：录制中的监听器只认最新的那一批，
  // 不随它们重装（重装一次就会让引擎的快捷键恢复又暂停一次）。
  const button = useRef<HTMLButtonElement>(null);
  const latest = useRef({ value, onChange, conflict });
  latest.current = { value, onChange, conflict };

  useEffect(() => {
    if (!recording) return;
    void api.setHotkeysSuspended(true);
    const onKey = (e: KeyboardEvent) => {
      const { value, onChange, conflict } = latest.current;
      e.preventDefault();
      // 同一个目标上别的监听器（比如另一个还没结束的录制框）也不该再收到这次按键。
      e.stopImmediatePropagation();
      if (e.repeat || isModifierCode(e.code)) return;
      const plain = !e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey;
      if (plain && e.code === "Escape") {
        setRecording(false);
        return;
      }
      if (plain && (e.code === "Backspace" || e.code === "Delete") && kind === "trigger") {
        onChange(null);
        setRecording(false);
        return;
      }
      const combo = comboFromEvent(e);
      if (!combo) {
        setError("这个键不能用，换一个试试");
        return;
      }
      if (kind === "trigger") {
        if (!isTrigger(combo)) {
          setError(TRIGGER_RULE);
          return;
        }
        const used = sameCombo(combo, value) ? null : conflict?.(combo);
        if (used) {
          setError(`已经用在「${used}」上了`);
          return;
        }
      }
      onChange(combo);
      setRecording(false);
    };
    // 点了别处、或窗口失去焦点（切到别的应用）就结束录制。不能只靠按钮的 blur：macOS 的 WebView 里
    // 点按钮不会让它获得焦点，也就不会有 blur，引擎的快捷键会一直停着。
    const stop = () => setRecording(false);
    const onPointer = (e: PointerEvent) => {
      if (!button.current?.contains(e.target as Node)) stop();
    };
    // 捕获阶段监听：抢在页面上其他快捷键处理之前。
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("pointerdown", onPointer, true);
    window.addEventListener("blur", stop);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("pointerdown", onPointer, true);
      window.removeEventListener("blur", stop);
      void api.setHotkeysSuspended(false);
    };
  }, [recording, kind]);

  const hint = kind === "trigger" ? "按下新的快捷键，Esc 取消，退格清除" : "按下要发送的键，Esc 取消";
  return (
    <span className="recorder-wrap">
      <button
        ref={button}
        type="button"
        className={`recorder ${recording ? "on" : ""}`}
        aria-label={label}
        title={recording ? undefined : "点一下，然后按下想要的键"}
        onClick={() => {
          setError(null);
          setRecording((r) => !r);
        }}
        onBlur={() => setRecording(false)}
      >
        {recording ? <span className="recorder-hint">{hint}</span> : value ? <ComboKeys combo={value} mac={mac} /> : <span className="recorder-empty">{placeholder}</span>}
      </button>
      {recording && error && <small className="text-danger">{error}</small>}
    </span>
  );
}

/** 每个触发键现在用在哪里，用来检查重复。`skip`：正在改的那一项不算。 */
function triggerUses(h: Hotkeys, deviceName: (id: string) => string): { combo: KeyCombo; what: string; slot: string }[] {
  const out: { combo: KeyCombo; what: string; slot: string }[] = [];
  if (h.switch_left) out.push({ combo: h.switch_left, what: "切到左边的电脑", slot: "left" });
  if (h.switch_right) out.push({ combo: h.switch_right, what: "切到右边的电脑", slot: "right" });
  h.devices.forEach((d) => out.push({ combo: d.combo, what: `切到 ${deviceName(d.device)}`, slot: `device:${d.device}` }));
  h.mappings.forEach((m, i) => out.push({ combo: m.trigger, what: `第 ${i + 1} 条映射`, slot: `mapping:${i}` }));
  return out;
}

export function HotkeySettings({ state, update }: { state: StateView; update: (h: Hotkeys) => void }) {
  const h = state.settings.hotkeys;
  const name = (id: string) => state.devices.find((d) => d.id === id)?.name ?? "已移除的电脑";
  const uses = triggerUses(h, name);
  const conflictFor = (slot: string) => (c: KeyCombo) => uses.find((u) => u.slot !== slot && sameCombo(u.combo, c))?.what ?? null;
  const peers = state.devices.filter((d) => d.status !== "local");
  const macOf = (id: string) => state.devices.find((d) => d.id === id)?.os === "MacOs";

  const setDevice = (device: string, combo: KeyCombo | null) =>
    update({
      ...h,
      devices: [...h.devices.filter((d) => d.device !== device), ...(combo ? [{ device, combo }] : [])],
    });
  const setMapping = (i: number, patch: Partial<KeyMapping>) => update({ ...h, mappings: h.mappings.map((m, j) => (j === i ? { ...m, ...patch } : m)) });

  // 新映射先在本地攒着：触发键和发送的键都录好了才存进设置（引擎会丢掉不完整的）。
  const [draft, setDraft] = useState<{ trigger: KeyCombo | null; send: KeyCombo | null; device: string } | null>(null);
  const draftDevice = draft && peers.some((p) => p.id === draft.device) ? draft.device : (peers[0]?.id ?? "");
  const commitDraft = (next: { trigger: KeyCombo | null; send: KeyCombo | null; device: string }) => {
    if (next.trigger && next.send && next.device) {
      update({ ...h, mappings: [...h.mappings, { trigger: next.trigger, send: next.send, device: next.device }] });
      setDraft(null);
    } else {
      setDraft(next);
    }
  };

  return (
    <>
      {state.unsupported.local_hotkey && (
        <p className="settings-note warn">
          这台电脑用的是 Wayland，键鼠在本机时流界看不到键盘：下面的快捷键只有在键鼠已经切到别的电脑之后才起作用（比如用它切回来）。
          要在本机上用快捷键切出去，可以在桌面自己的快捷键设置里绑一条命令：<code>flowacross --switch right</code>（还可以是 <code>left</code>、<code>home</code> 或设备名称）。
        </p>
      )}

      <div className="panel settings-panel">
        <div className="setting inline">
          <div className="setting-text">
            <span>切到左边的电脑</span>
            <small>按布局里的位置，切到左边最近的一台</small>
          </div>
          <KeyRecorder label="切到左边的电脑的快捷键" kind="trigger" value={h.switch_left} conflict={conflictFor("left")} onChange={(c) => update({ ...h, switch_left: c })} />
        </div>
        <div className="setting inline">
          <div className="setting-text">
            <span>切到右边的电脑</span>
            <small>切到右边最近的一台</small>
          </div>
          <KeyRecorder label="切到右边的电脑的快捷键" kind="trigger" value={h.switch_right} conflict={conflictFor("right")} onChange={(c) => update({ ...h, switch_right: c })} />
        </div>
        {state.devices.map((d) => (
          <div className="setting inline" key={d.id}>
            <div className="setting-text">
              <span>{d.status === "local" ? "回到这台电脑" : `切到 ${d.name}`}</span>
              <small>{d.status === "local" ? "键鼠在别的电脑上时，直接切回来" : "不管它在布局的哪一边，直接切过去"}</small>
            </div>
            <KeyRecorder
              label={`切到 ${d.name} 的快捷键`}
              kind="trigger"
              value={h.devices.find((x) => x.device === d.id)?.combo ?? null}
              conflict={conflictFor(`device:${d.id}`)}
              onChange={(c) => setDevice(d.id, c)}
            />
          </div>
        ))}
      </div>
      <p className="settings-note">
        点右边的框，再按下想用的组合键。快捷键要在键盘所在的这台电脑上设置；键鼠切到别的电脑之后照样能用。暂停共享时不起作用，锁定在本机时不能切出去。
      </p>

      <h2 className="settings-subtitle">快捷键映射</h2>
      {state.unsupported.local_hotkey ? (
        <p className="settings-note warn">
          这台电脑用的是 Wayland，不支持快捷键映射：键鼠在本机时流界看不到键盘，按了没有反应。别的电脑设置的映射照常可以发到这台电脑。
        </p>
      ) : (
        <>
          <p className="settings-desc">
            在这台电脑上按下一组键，让另一台电脑收到另一组键，键鼠不用切过去。比如按 {isMac ? "⌃ ⌥ S" : "Ctrl + Alt + S"}，让另一台电脑收到 S；对方屏幕熄着时，发一个键过去也能把它点亮。
          </p>
          <div className="panel settings-panel">
            {h.mappings.length === 0 && !draft && <div className="setting muted">还没有映射。</div>}
            {h.mappings.map((m, i) => (
              <MappingRow
                key={i}
                peers={peers}
                target={state.devices.find((d) => d.id === m.device)}
                trigger={m.trigger}
                send={m.send}
                device={m.device}
                sendMac={macOf(m.device)}
                conflict={conflictFor(`mapping:${i}`)}
                onTrigger={(c) => (c ? setMapping(i, { trigger: c }) : update({ ...h, mappings: h.mappings.filter((_, j) => j !== i) }))}
                onSend={(c) => c && setMapping(i, { send: c })}
                onDevice={(device) => setMapping(i, { device })}
                onRemove={() => update({ ...h, mappings: h.mappings.filter((_, j) => j !== i) })}
              />
            ))}
            {draft && (
              <MappingRow
                peers={peers}
                target={state.devices.find((d) => d.id === draftDevice)}
                trigger={draft.trigger}
                send={draft.send}
                device={draftDevice}
                sendMac={macOf(draftDevice)}
                conflict={conflictFor("draft")}
                onTrigger={(c) => commitDraft({ ...draft, device: draftDevice, trigger: c })}
                onSend={(c) => commitDraft({ ...draft, device: draftDevice, send: c })}
                onDevice={(device) => commitDraft({ ...draft, device })}
                onRemove={() => setDraft(null)}
              />
            )}
          </div>
          <div className="settings-actions">
            <button className="btn small" disabled={peers.length === 0 || !!draft} onClick={() => setDraft({ trigger: null, send: null, device: peers[0]?.id ?? "" })}>
              <PlusIcon />
              添加映射
            </button>
            {peers.length === 0 && <small className="muted">先配对一台电脑，才有地方可发。</small>}
          </div>
          <p className="settings-note">
            发过去的是按键的位置，对方照自己的键盘布局和输入法处理；Mac 和 Windows 之间按「跨屏与键鼠」里的设置换算修饰键（默认 ⌘ 和 Ctrl 互换）。对方的流界需要 0.2.4 或更新版本；对方没连着、被暂停或版本太旧时，这组键不会被拦下，照常交给当前的电脑。
          </p>
        </>
      )}
    </>
  );
}

function MappingRow({
  peers,
  target,
  trigger,
  send,
  device,
  sendMac,
  conflict,
  onTrigger,
  onSend,
  onDevice,
  onRemove,
}: {
  peers: DeviceView[];
  target: DeviceView | undefined;
  trigger: KeyCombo | null;
  send: KeyCombo | null;
  device: string;
  sendMac: boolean;
  conflict: (c: KeyCombo) => string | null;
  onTrigger: (c: KeyCombo | null) => void;
  onSend: (c: KeyCombo | null) => void;
  onDevice: (id: string) => void;
  onRemove: () => void;
}) {
  // 连着却发不了：对方的版本太旧。没连着时不提示，等它连上再说。
  const tooOld = target?.status === "connected" && !target.can_send_keys;
  return (
    <div className="setting mapping">
      <div className="mapping-row">
        <KeyRecorder label="在这台电脑上按下的快捷键" kind="trigger" value={trigger} conflict={conflict} onChange={onTrigger} placeholder="按下的快捷键" />
        <span className="mapping-arrow" aria-hidden="true">
          →
        </span>
        <KeyRecorder label="对方收到的键" kind="send" value={send} mac={sendMac} onChange={onSend} placeholder="对方收到的键" />
        <span className="mapping-to">发给</span>
        <select className="select" aria-label="发给哪台电脑" value={device} onChange={(e) => onDevice(e.target.value)}>
          {!peers.some((p) => p.id === device) && <option value={device}>已移除的电脑</option>}
          {peers.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        <button className="btn small ghost-link" onClick={onRemove}>
          删除
        </button>
      </div>
      {tooOld && <small className="text-danger">「{target?.name}」的流界版本太旧，收不到。把它升级到 0.2.4 或更新版本后生效。</small>}
    </div>
  );
}
