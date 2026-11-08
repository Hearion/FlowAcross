// 跨屏与剪贴板的设置控件：主界面侧栏和设置页共用同一套。

import type { HoldKey, Settings } from "./types";
import { Segmented, Toggle, isMac } from "./ui";

const HOLD_OPTIONS: { value: HoldKey; label: string }[] = [
  { value: "none", label: "不需要" },
  { value: "ctrl", label: isMac ? "⌃" : "Ctrl" },
  { value: "alt", label: isMac ? "⌥" : "Alt" },
  { value: "shift", label: isMac ? "⇧" : "Shift" },
];

interface Props {
  settings: Settings;
  update: (patch: Partial<Settings>) => void;
}

export function CrossScreenSettings({ settings: s, update }: Props) {
  return (
    <>
      <div className="setting">
        <div className="setting-text">
          <span>按住按键才跨屏</span>
          <small>防止鼠标甩到边上时意外切换</small>
        </div>
        <Segmented label="按住按键才跨屏" value={s.hold_key} options={HOLD_OPTIONS} onChange={(v) => update({ hold_key: v })} />
      </div>
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
      <div className="setting inline">
        <div className="setting-text">
          <span>屏幕角落不切换</span>
          <small>避免误触触发角、开始菜单</small>
        </div>
        <Toggle label="屏幕角落不切换" checked={s.corner_guard} onChange={(v) => update({ corner_guard: v })} />
      </div>
      <div className="setting inline">
        <div className="setting-text">
          <span>快捷键切换</span>
          <small>按住后用方向键切到左右相邻的电脑</small>
        </div>
        <span className="keys">
          <kbd>{isMac ? "⌃" : "Ctrl"}</kbd>
          <kbd>{isMac ? "⌥" : "Alt"}</kbd>
          <kbd>←</kbd>
          <kbd>→</kbd>
        </span>
      </div>
    </>
  );
}

export function ClipboardSetting({ settings: s, update, title = false }: Props & { title?: boolean }) {
  return (
    <div className="setting inline">
      <div className="setting-text">
        <span className={title ? "panel-title" : undefined}>共享剪贴板</span>
        <small>在一台电脑复制文字，切到另一台直接粘贴</small>
      </div>
      <Toggle label="共享剪贴板" checked={s.clipboard} onChange={(v) => update({ clipboard: v })} />
    </div>
  );
}
