import type { ReactNode } from "react";
import { api } from "./api";
import type { NearbyView, Os, PairingView, PermissionKind, PermissionView } from "./types";
import { AlertIcon, CheckIcon, DeviceIcon } from "./ui";

export const OS_TEXT: Record<Os, string> = {
  MacOs: "macOS",
  Windows: "Windows",
  Linux: "Linux",
  Other: "其他系统",
};

export function Modal({ children, onClose, wide }: { children: ReactNode; onClose?: () => void; wide?: boolean }) {
  return (
    <div className="modal-backdrop" onPointerDown={(e) => e.target === e.currentTarget && onClose?.()}>
      <div className={`modal ${wide ? "wide" : ""}`} role="dialog" aria-modal="true">
        {children}
      </div>
    </div>
  );
}

export function Spinner() {
  return <span className="spinner" aria-hidden="true" />;
}

/** 附近运行流界、尚未配对的电脑。 */
export function NearbyList({ nearby }: { nearby: NearbyView[] }) {
  if (nearby.length === 0) {
    return (
      <div className="nearby-empty">
        <div className="searching">
          <Spinner />
          正在查找同一网络中运行流界的电脑…
        </div>
        <ul className="tips">
          <li>在另一台电脑上安装并打开流界</li>
          <li>两台电脑需要连接同一个 Wi‑Fi 或局域网</li>
          <li>公司或公共网络可能屏蔽设备发现，可以换成家用网络试试</li>
        </ul>
      </div>
    );
  }
  return (
    <ul className="nearby-list">
      {nearby.map((d) => (
        <li key={d.id}>
          <DeviceIcon os={d.os} hue={4} />
          <span className="device-info">
            <span className="device-name">{d.name}</span>
            <span className="device-sub">{OS_TEXT[d.os]}</span>
          </span>
          <button className="btn small primary" disabled={d.connecting} onClick={() => api.pairWith(d.id)}>
            {d.connecting ? "连接中…" : "配对"}
          </button>
        </li>
      ))}
    </ul>
  );
}

export function AddDeviceDialog({ nearby, onClose }: { nearby: NearbyView[]; onClose: () => void }) {
  return (
    <Modal onClose={onClose}>
      <h2>添加设备</h2>
      <p className="modal-sub">选择要配对的电脑。配对时两台电脑会显示同一个 6 位配对码，核对一致后分别确认即可。</p>
      <NearbyList nearby={nearby} />
      <div className="modal-actions">
        <button className="btn" onClick={onClose}>
          关闭
        </button>
      </div>
    </Modal>
  );
}

export function PairingDialog({ pairing }: { pairing: PairingView }) {
  const p = pairing;
  const waiting = p.local_confirmed && !p.remote_confirmed;
  return (
    <Modal>
      <h2>{p.outgoing ? `与「${p.name}」配对` : `「${p.name}」请求配对`}</h2>
      <p className="modal-sub">
        请确认两台电脑上显示的配对码完全一致。
        {p.outgoing ? "然后在两台电脑上都点「确认配对」。" : "如果你没有发起配对，请点「拒绝」。"}
      </p>
      <div className="pair-code" aria-label={`配对码 ${p.code}`}>
        {[...p.code].map((c, i) =>
          c === " " ? <span key={i} className="pair-gap" /> : <span key={i} className="pair-digit">{c}</span>,
        )}
      </div>
      <div className="pair-status">
        <span className={p.local_confirmed ? "ok" : ""}>
          {p.local_confirmed ? <CheckIcon /> : <span className="pair-pending" />}
          {p.local_confirmed ? "本机已确认" : "本机尚未确认"}
        </span>
        <span className={p.remote_confirmed ? "ok" : ""}>
          {p.remote_confirmed ? <CheckIcon /> : <span className="pair-pending" />}
          {p.remote_confirmed ? `${p.name} 已确认` : `等待 ${p.name} 确认`}
        </span>
      </div>
      <div className="modal-actions">
        <button className="btn" onClick={() => api.respondPairing(p.id, false)}>
          {p.outgoing ? "取消" : "拒绝"}
        </button>
        <button className="btn primary" disabled={p.local_confirmed} onClick={() => api.respondPairing(p.id, true)}>
          {waiting ? (
            <>
              <Spinner /> 等待对方确认
            </>
          ) : (
            "确认配对"
          )}
        </button>
      </div>
    </Modal>
  );
}

export function ConfirmDialog({
  title,
  body,
  confirm,
  danger,
  onConfirm,
  onCancel,
}: {
  title: string;
  body: string;
  confirm: string;
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <Modal onClose={onCancel}>
      <h2>{title}</h2>
      <p className="modal-sub">{body}</p>
      <div className="modal-actions">
        <button className="btn" onClick={onCancel}>
          取消
        </button>
        <button className={`btn ${danger ? "danger-fill" : "primary"}`} onClick={onConfirm}>
          {confirm}
        </button>
      </div>
    </Modal>
  );
}

export const PERMISSION_TEXT: Record<PermissionKind, { name: string; why: string }> = {
  accessibility: { name: "辅助功能", why: "在本机模拟和拦截键盘鼠标，用来控制这台电脑、把键鼠交给其他电脑" },
  input_monitoring: { name: "输入监控", why: "读取键盘输入，转发给正在控制的电脑" },
  local_network: { name: "本地网络", why: "发现并连接同一网络中的其他电脑" },
};

export function PermissionList({ permissions, hint }: { permissions: PermissionView[]; hint?: string | null }) {
  if (permissions.length === 0) {
    return <p className="muted">当前系统不需要额外授权。</p>;
  }
  const allOk = permissions.every((p) => p.state === "granted" || p.state === "not_applicable");
  return (
    <>
      {hint && !allOk && <div className="perm-hint">{hint}</div>}
      <ul className="perm-list">
      {permissions.map((p) => {
        const t = PERMISSION_TEXT[p.kind];
        const ok = p.state === "granted" || p.state === "not_applicable";
        return (
          <li key={p.kind}>
            <span className={`perm-dot ${ok ? "ok" : ""}`}>{ok ? <CheckIcon /> : <AlertIcon />}</span>
            <span className="perm-text">
              <strong>{t.name}</strong>
              <small>{t.why}</small>
            </span>
            {ok ? (
              <span className="chip ok">已授权</span>
            ) : (
              <span className="perm-actions">
                <button className="btn small primary" onClick={() => api.requestPermission(p.kind)}>
                  授权
                </button>
                <button className="btn small" onClick={() => api.openPermissionSettings(p.kind)}>
                  打开设置
                </button>
              </span>
            )}
          </li>
        );
      })}
      </ul>
    </>
  );
}
