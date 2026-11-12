// 界面共用的小组件与工具。

import type { ReactNode } from "react";
import logoUrl from "./assets/logo.png";
import type { DeviceView, Display, Os } from "./types";

export const isMac = typeof navigator !== "undefined" && /Mac/i.test(navigator.userAgent);

/** 每台设备一个颜色，用于屏幕壁纸和侧栏图标，帮助用户把屏幕和设备对应起来。 */
export const HUE_COUNT = 5;
export function hueOf(devices: DeviceView[], id: string): number {
  const i = devices.findIndex((d) => d.id === id);
  return (i < 0 ? 0 : i) % HUE_COUNT;
}

/** 设备的连接状态文字，侧栏和布局图共用（"键鼠在这里 / 控制中"由各处自己显示）。 */
export function statusText(d: DeviceView): string {
  if (d.status === "local") return "本机";
  if (d.paused) return "已暂停";
  if (d.status === "connected") return d.remote_paused ? "对方已暂停" : "在线";
  if (d.status === "connecting") return "连接中";
  return d.manual ? "已断开" : "离线";
}

const size = (w: number, h: number) => `${Math.round(w)}×${Math.round(h)}`;

/**
 * 按各系统设置里的习惯描述分辨率：
 * - macOS：显示"看起来像"的分辨率（逻辑尺寸，如 2560×1440），详细模式附上面板原生分辨率；
 * - Windows 等：显示物理分辨率和缩放比例（如 3840×2160（150%），详细模式写成「缩放 150%」）。
 */
export function resolution(d: Display, os: Os, detailed = false): string {
  if (os === "MacOs") {
    const looks = size(d.rect.width, d.rect.height);
    if (!detailed || !d.native) return looks;
    const [w, h] = d.native;
    return w === Math.round(d.rect.width) ? looks : `${looks}，原生 ${size(w, h)}`;
  }
  const [w, h] = d.native ?? [d.rect.width * d.scale, d.rect.height * d.scale];
  const pct = Math.round(d.scale * 100);
  return detailed ? `${size(w, h)}，缩放 ${pct}%` : `${size(w, h)}（${pct}%）`;
}

export function ComputerIcon({ size = 16, className }: { size?: number; className?: string }) {
  return (
    <svg className={className} width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
      <rect x="3" y="4" width="18" height="12" rx="2" fill="none" stroke="currentColor" strokeWidth="1.8" />
      <path d="M9 20h6M12 16v4" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
    </svg>
  );
}

const APPLE_PATH =
  "M12.152 6.896c-.948 0-2.415-1.078-3.96-1.04-2.04.027-3.91 1.183-4.961 3.014-2.117 3.675-.546 9.103 1.519 12.09 1.013 1.454 2.208 3.09 3.792 3.039 1.52-.065 2.09-.987 3.935-.987 1.831 0 2.35.987 3.96.948 1.637-.026 2.676-1.48 3.676-2.948 1.156-1.688 1.636-3.325 1.662-3.415-.039-.013-3.182-1.221-3.22-4.857-.026-3.04 2.48-4.494 2.597-4.559-1.429-2.09-3.623-2.324-4.39-2.376-2-.156-3.675 1.09-4.61 1.09zM15.53 3.83c.843-1.012 1.4-2.427 1.245-3.83-1.207.052-2.662.805-3.532 1.818-.78.896-1.454 2.338-1.273 3.714 1.338.104 2.715-.688 3.559-1.701";
const WINDOWS_PATH = "M3 3h8.4v8.4H3zM12.6 3H21v8.4h-8.4zM3 12.6h8.4V21H3zM12.6 12.6H21V21h-8.4z";

/** 设备的系统标识：macOS 用苹果标志，Windows 用视窗标志，其他系统用通用电脑图标。单色，跟随文字颜色。 */
export function OsIcon({ os, size = 16, className }: { os: Os; size?: number; className?: string }) {
  if (os === "MacOs" || os === "Windows") {
    return (
      <svg className={className} width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
        <path d={os === "MacOs" ? APPLE_PATH : WINDOWS_PATH} fill="currentColor" />
      </svg>
    );
  }
  return <ComputerIcon size={size} className={className} />;
}

/** 侧栏里的设备图标：一块迷你屏幕，壁纸色和布局图里这台设备的屏幕一致，中间是系统标志。 */
export function DeviceIcon({ os, hue }: { os: Os; hue: number }) {
  return (
    <span className={`device-icon hue-${hue}`}>
      <span className="device-icon-screen">
        <OsIcon os={os} size={13} />
      </span>
      <span className="device-icon-stand" />
    </span>
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

export function PlayIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <path d="m8 5 11 7-11 7Z" fill="currentColor" stroke="currentColor" strokeWidth="1.8" strokeLinejoin="round" />
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

/** 线条图标的统一外壳：24 网格、1.7 线宽、圆头圆角。 */
function Line({ size = 16, children }: { size?: number; children: ReactNode }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {children}
    </svg>
  );
}

export const ChevronLeftIcon = () => (
  <Line size={15}>
    <path d="M14.5 6l-6 6 6 6" />
  </Line>
);

export const WarnIcon = () => (
  <Line size={16}>
    <path d="M12 4.5l8.5 14.5h-17z" />
    <path d="M12 10.5v3.5M12 16.8v.2" />
  </Line>
);

export const CheckIcon = ({ size = 12 }: { size?: number }) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
    <path d="M5 12.5l4.5 4.5L19 7.5" />
  </svg>
);

export const AlertIcon = ({ size = 12 }: { size?: number }) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" aria-hidden="true">
    <path d="M12 6v7.5M12 18v.1" />
  </svg>
);

/** 接收（向下箭头落到底线）/ 发送（向上箭头离开底线）。 */
export const TransferIcon = ({ incoming }: { incoming: boolean }) => (
  <Line size={16}>
    {incoming ? <path d="M12 4v11M7.5 10.5L12 15l4.5-4.5" /> : <path d="M12 15V4M7.5 8.5L12 4l4.5 4.5" />}
    <path d="M5 19.5h14" />
  </Line>
);

export const CloseIcon = () => (
  <Line size={14}>
    <path d="M7 7l10 10M17 7L7 17" />
  </Line>
);

export const FolderIcon = () => (
  <Line size={14}>
    <path d="M3.5 7.5c0-1.1.9-2 2-2h3.8l2 2.2h7.2c1.1 0 2 .9 2 2v7.8c0 1.1-.9 2-2 2h-13c-1.1 0-2-.9-2-2z" />
  </Line>
);

export const InfoIcon = () => (
  <Line size={16}>
    <circle cx="12" cy="12" r="8.5" />
    <path d="M12 11v5M12 7.8v.2" />
  </Line>
);

/** 设置页侧栏各分区的图标。 */
export const SETTINGS_ICONS: Record<string, () => ReactNode> = {
  general: () => (
    <Line>
      <path d="M4 7h9M17 7h3M4 17h3M11 17h9" />
      <circle cx="15" cy="7" r="2" />
      <circle cx="9" cy="17" r="2" />
    </Line>
  ),
  permissions: () => (
    <Line>
      <path d="M12 3.5l7 2.8v5.2c0 4.3-3 7.6-7 9-4-1.4-7-4.7-7-9V6.3z" />
      <path d="M9 12l2.2 2.2L15.5 10" />
    </Line>
  ),
  crossing: () => (
    <Line>
      <rect x="2.5" y="5" width="8" height="11" rx="1.6" />
      <rect x="13.5" y="5" width="8" height="11" rx="1.6" />
      <path d="M8 20h8M10.5 10.5h3" />
    </Line>
  ),
  clipboard: () => (
    <Line>
      <rect x="5.5" y="4.5" width="13" height="16" rx="2" />
      <path d="M9.5 4.5V3.8c0-.7.5-1.3 1.2-1.3h2.6c.7 0 1.2.6 1.2 1.3v.7M9 11h6M9 15h4" />
    </Line>
  ),
  updates: () => (
    <Line>
      <path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3" />
      <path d="M19.5 4.5v4h-4" />
    </Line>
  ),
  about: () => (
    <Line>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 11v5M12 7.8v.2" />
    </Line>
  ),
};

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
