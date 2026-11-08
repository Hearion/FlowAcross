// 界面共用的小组件与工具。

import logoUrl from "./assets/logo.png";
import type { DeviceView, Display, Os } from "./types";

export const isMac = typeof navigator !== "undefined" && /Mac/i.test(navigator.userAgent);

/** 每台设备一个颜色，用于屏幕壁纸和侧栏图标，帮助用户把屏幕和设备对应起来。 */
export const HUE_COUNT = 5;
export function hueOf(devices: DeviceView[], id: string): number {
  const i = devices.findIndex((d) => d.id === id);
  return (i < 0 ? 0 : i) % HUE_COUNT;
}

const size = (w: number, h: number) => `${Math.round(w)}×${Math.round(h)}`;

/**
 * 按各系统设置里的习惯描述分辨率：
 * - macOS：显示"看起来像"的分辨率（逻辑尺寸，如 2560×1440），详细模式附上面板原生分辨率；
 * - Windows 等：显示物理分辨率和缩放比例（如 3840×2160 · 150%）。
 */
export function resolution(d: Display, os: Os, detailed = false): string {
  if (os === "MacOs") {
    const looks = size(d.rect.width, d.rect.height);
    if (!detailed || !d.native) return looks;
    const [w, h] = d.native;
    return w === Math.round(d.rect.width) ? looks : `${looks} · 原生 ${size(w, h)}`;
  }
  const [w, h] = d.native ?? [d.rect.width * d.scale, d.rect.height * d.scale];
  return `${size(w, h)} · ${Math.round(d.scale * 100)}%`;
}

export function ComputerIcon({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
      <rect x="3" y="4" width="18" height="12" rx="2" fill="none" stroke="currentColor" strokeWidth="1.8" />
      <path d="M9 20h6M12 16v4" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
    </svg>
  );
}

export function CursorIcon({ size = 14 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
      <path
        d="M5 3l14 7.5-6.2 1.6L10 18.5z"
        fill="currentColor"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinejoin="round"
      />
    </svg>
  );
}

export function LockIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <rect x="5" y="11" width="14" height="9" rx="2" fill="none" stroke="currentColor" strokeWidth="1.8" />
      <path d="M8 11V8a4 4 0 0 1 8 0v3" fill="none" stroke="currentColor" strokeWidth="1.8" />
    </svg>
  );
}

export function PauseIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <path d="M9 6v12M15 6v12" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
    </svg>
  );
}

export function GearIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true">
      <circle cx="12" cy="12" r="3.2" fill="none" stroke="currentColor" strokeWidth="1.8" />
      <path
        d="M12 2.8l1.6 2.3 2.7-.7.7 2.7 2.6 1-.6 2.7 1.9 2-1.9 2 .6 2.7-2.6 1-.7 2.7-2.7-.7L12 21.2l-1.6-2.3-2.7.7-.7-2.7-2.6-1 .6-2.7-1.9-2 1.9-2-.6-2.7 2.6-1 .7-2.7 2.7.7z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinejoin="round"
      />
    </svg>
  );
}

export function PlusIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" aria-hidden="true">
      <path d="M12 5v14M5 12h14" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
    </svg>
  );
}

export function Logo({ size = 26 }: { size?: number }) {
  return <img className="logo" src={logoUrl} width={size} height={size} alt="" draggable={false} />;
}

export function Toggle({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      className={`toggle ${checked ? "on" : ""}`}
      onClick={() => onChange(!checked)}
    >
      <span className="toggle-knob" />
    </button>
  );
}

export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
  label: string;
}) {
  return (
    <div className="segmented" role="radiogroup" aria-label={label}>
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          className={value === o.value ? "on" : ""}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
