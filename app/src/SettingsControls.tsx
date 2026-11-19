// 跨屏与剪贴板的设置控件：主界面侧栏和设置页共用同一套。

import { useEffect, useState, type ChangeEvent, type KeyboardEvent } from "react";
import { api } from "./api";
import type { HoldKey, MacModifier, Settings, UnsupportedView, WinKeysPreset, WinToMacKeys } from "./types";
import { comboLabels } from "./keys";
import { Segmented, Toggle, isMac } from "./ui";

const HOLD_OPTIONS: { value: HoldKey; label: string }[] = [
  { value: "none", label: "不需要" },
  { value: "ctrl", label: isMac ? "⌃" : "Ctrl" },
  { value: "alt", label: isMac ? "⌥" : "Alt" },
  { value: "shift", label: isMac ? "⇧" : "Shift" },
];

/** 设备名称输入框的行为（侧栏和设置页共用）：失去焦点或按回车时保存，名字没变不保存。 */
export function useDeviceNameInput(current: string) {
  const [value, setValue] = useState(current);
  useEffect(() => setValue(current), [current]);
  return {
    value,
    maxLength: 40,
    onChange: (e: ChangeEvent<HTMLInputElement>) => setValue(e.target.value),
    onBlur: () => {
      const name = value.trim();
      if (name !== current) api.setDeviceName(value);
      // 清空（改回系统电脑名）或只多了首尾空格时，保存后名字可能没变，不会有新状态把输入框改回来。
      if (name === "" || name === current) setValue(current);
    },
    onKeyDown: (e: KeyboardEvent<HTMLInputElement>) => {
      // 输入法用回车上屏时不算（WebKit 这时报的 isComposing 是 false，只能看 keyCode 229）。
      if (e.nativeEvent.isComposing || e.keyCode === 229) return;
      if (e.key === "Enter") e.currentTarget.blur();
    },
  };
}

/** 滚轮速度的档位（百分比）。 */
const WHEEL_SPEEDS: { value: string; label: string }[] = [
  { value: "50", label: "0.5×" },
  { value: "100", label: "1×" },
  { value: "150", label: "1.5×" },
  { value: "200", label: "2×" },
  { value: "300", label: "3×" },
];

/** 配置文件里的值不在档位上时（手动改过），显示成最接近的一档。 */
function nearestWheelSpeed(percent: number): string {
  return WHEEL_SPEEDS.reduce((best, o) =>
    Math.abs(Number(o.value) - percent) < Math.abs(Number(best.value) - percent) ? o : best,
  ).value;
}

interface Props {
  settings: Settings;
  update: (patch: Partial<Settings>) => void;
}

/**
 * `unsupported`：这台电脑的桌面做不到的切换方式和效果（目前只有 Linux 的 Wayland 会话会遇到），
 * 对应的设置直接不显示，引擎也会忽略它们。
 */
export function CrossScreenSettings({
  settings: s,
  update,
  unsupported: no,
  onHotkeys,
}: Props & { unsupported: UnsupportedView; onHotkeys?: () => void }) {
  const { switch_left: left, switch_right: right } = s.hotkeys;
  // 两个方向的修饰键相同时合起来写（⌃ ⌥ ← →），否则各写各的。
  const shared = left && right && left.mods === right.mods;
  return (
    <>
      {!no.hold_key && (
        <div className="setting">
          <div className="setting-text">
            <span>按住按键才跨屏</span>
            <small>防止鼠标甩到边上时意外切换</small>
          </div>
          <Segmented label="按住按键才跨屏" value={s.hold_key} options={HOLD_OPTIONS} onChange={(v) => update({ hold_key: v })} />
        </div>
      )}
      {!no.dwell && (
        <div className="setting">
          <div className="setting-text">
            <span>贴边停留</span>
            <small>到达边缘后停一下再切换</small>
          </div>
          <Segmented
            label="贴边停留"
            value={String(s.dwell_ms) as "0" | "300" | "600"}
            options={[
              { value: "0", label: "立即" },
              { value: "300", label: "0.3 秒" },
              { value: "600", label: "0.6 秒" },
            ]}
            onChange={(v) => update({ dwell_ms: Number(v) })}
          />
        </div>
      )}
      <div className="setting">
        <div className="setting-text">
          <span>尺寸不同的屏幕</span>
          <small>{s.mapping === "Continuous" ? "只能从两块屏幕相贴的部分穿过" : "整条边都能穿过，按比例对应"}</small>
        </div>
        <Segmented
          label="跨边映射"
          value={s.mapping}
          options={[
            { value: "Continuous", label: "对齐" },
            { value: "Proportional", label: "按比例" },
          ]}
          onChange={(v) => update({ mapping: v })}
        />
      </div>
      <div className="setting">
        <div className="setting-text">
          <span>滚轮速度</span>
          <small>控制其他电脑时滚轮滚得多快，在这台电脑上设置</small>
        </div>
        <Segmented
          label="滚轮速度"
          value={nearestWheelSpeed(s.wheel_speed)}
          options={WHEEL_SPEEDS}
          onChange={(v) => update({ wheel_speed: Number(v) })}
        />
      </div>
      <div className="setting inline">
        <div className="setting-text">
          <span>屏幕角落不切换</span>
          <small>避免误触触发角、开始菜单</small>
        </div>
        <Toggle label="屏幕角落不切换" checked={s.corner_guard} onChange={(v) => update({ corner_guard: v })} />
      </div>
      {!no.edge_glow && (
        <div className="setting inline">
          <div className="setting-text">
            <span>跨屏时显示边缘浮光</span>
            <small>键鼠来到这台电脑时，进来的那条屏幕边亮一下</small>
          </div>
          <Toggle label="跨屏时显示边缘浮光" checked={s.edge_glow} onChange={(v) => update({ edge_glow: v })} />
        </div>
      )}
      {!no.local_hotkey && (
        <div className="setting inline">
          <div className="setting-text">
            <span>快捷键切换</span>
            <small>
              切到左右相邻的电脑
              {onHotkeys && (
                <>
                  ，
                  <button type="button" className="link" onClick={onHotkeys}>
                    修改快捷键
                  </button>
                </>
              )}
            </small>
          </div>
          {shared ? (
            <span className="keys">
              {[...comboLabels(left).slice(0, -1), comboLabels(left).at(-1), comboLabels(right).at(-1)].map((k, i) => (
                <kbd key={i}>{k}</kbd>
              ))}
            </span>
          ) : left || right ? (
            <span className="keys-stack">
              {[left, right].map(
                (c, i) =>
                  c && (
                    <span className="keys" key={i}>
                      {comboLabels(c).map((k, j) => (
                        <kbd key={j}>{k}</kbd>
                      ))}
                    </span>
                  ),
              )}
            </span>
          ) : (
            <span className="row-value">未设置</span>
          )}
        </div>
      )}
    </>
  );
}

const WIN_KEY_PRESETS: { value: WinKeysPreset; label: string }[] = [
  { value: "windows", label: "Windows" },
  { value: "mac_position", label: "Mac 键位" },
  { value: "direct", label: "直通" },
  { value: "custom", label: "自定义" },
];

const PRESET_MAPS: Record<Exclude<WinKeysPreset, "custom">, WinToMacKeys> = {
  windows: { ctrl: "command", win: "control", alt: "option" },
  mac_position: { ctrl: "control", win: "option", alt: "command" },
  direct: { ctrl: "control", win: "command", alt: "option" },
};

const PRESET_DESC: Record<WinKeysPreset, string> = {
  windows: "Ctrl+C 复制、Ctrl+V 粘贴，和在 Windows 上一样",
  mac_position: "按 Mac 键盘的位置：空格旁边的 Alt 当 ⌘ 用",
  direct: "不做转换，和把 PC 键盘直接插在 Mac 上一样",
  custom: "分别指定 Ctrl、Win、Alt 对应 Mac 的哪个键",
};

const MAC_KEYS: { value: MacModifier; label: string }[] = [
  { value: "control", label: "⌃ Control" },
  { value: "option", label: "⌥ Option" },
  { value: "command", label: "⌘ Command" },
];

const PC_KEYS: { key: keyof WinToMacKeys; label: string }[] = [
  { key: "ctrl", label: "Ctrl" },
  { key: "win", label: "Win" },
  { key: "alt", label: "Alt" },
];

/** 被 PC 键盘控制时修饰键的对应。按键由被控制的 Mac 换算，所以只在 Mac 上显示。 */
export function WinKeysSetting({ settings: s, update }: Props) {
  const preset = s.win_keys;
  const custom = preset === "custom";
  const current = preset === "custom" ? s.win_keys_custom : PRESET_MAPS[preset];
  const pick = (v: WinKeysPreset) =>
    // 切到自定义时从当前预设开始改，不会突然变成别的键位。
    v === "custom" && !custom ? update({ win_keys: v, win_keys_custom: current }) : update({ win_keys: v });

  return (
    <>
      <div className="setting">
        <div className="setting-text">
          <span>Windows 键盘的修饰键</span>
          <small>{PRESET_DESC[s.win_keys]}</small>
        </div>
        <Segmented label="Windows 键盘的修饰键" value={s.win_keys} options={WIN_KEY_PRESETS} onChange={pick} />
      </div>
      {PC_KEYS.map(({ key, label }) => (
        <div className="setting inline" key={key}>
          <span className="keys">
            <kbd>{label}</kbd>
          </span>
          {custom ? (
            <Segmented
              label={`${label} 对应 Mac 的`}
              value={current[key]}
              options={MAC_KEYS}
              onChange={(v) => update({ win_keys_custom: { ...current, [key]: v } })}
            />
          ) : (
            <span className="row-value">{MAC_KEYS.find((k) => k.value === current[key])?.label}</span>
          )}
        </div>
      ))}
    </>
  );
}

export function ClipboardSetting({ settings: s, update, title = false }: Props & { title?: boolean }) {
  return (
    <>
      <div className="setting inline">
        <div className="setting-text">
          <span className={title ? "panel-title" : undefined}>共享剪贴板</span>
          <small>在一台电脑复制文字、文件或截图，切到另一台直接粘贴</small>
        </div>
        <Toggle label="共享剪贴板" checked={s.clipboard} onChange={(v) => update({ clipboard: v })} />
      </div>
      <div className="setting inline">
        <div className="setting-text">
          <span>复制文件和图片</span>
          <small>键鼠切过去时自动传输，超过 1 GB 需要手动接收</small>
        </div>
        <Toggle
          label="复制文件和图片"
          checked={s.clipboard && s.clipboard_files}
          disabled={!s.clipboard}
          onChange={(v) => update({ clipboard_files: v })}
        />
      </div>
    </>
  );
}
