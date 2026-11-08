import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import type { UpdateError, UpdateInfo } from "./types";

export type UpdatePhase = "idle" | "checking" | "latest" | "available" | "downloading" | "installing" | "error";

export interface Updater {
  phase: UpdatePhase;
  info: UpdateInfo | null;
  /** 下载进度 0–1；不知道总大小时为 null。 */
  progress: number | null;
  error: string | null;
  /** 失败发生在下载安装阶段（而不是检查阶段）。 */
  installFailed: boolean;
  /** 引擎已关闭，需要重新打开流界。 */
  needsRestart: boolean;
  /** 用户在主界面点了「稍后」。 */
  dismissed: boolean;
  check(): void;
  install(): void;
  restart(): void;
  dismiss(): void;
}

function errorText(e: unknown): UpdateError {
  if (e && typeof e === "object" && "message" in e) return e as UpdateError;
  return { message: String(e), restart: false };
}

/** 更新状态放在 App 里，主界面横幅和设置页共用同一份。 */
export function useUpdater(): Updater {
  const [phase, setPhase] = useState<UpdatePhase>("idle");
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [progress, setProgress] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [needsRestart, setNeedsRestart] = useState(false);
  const [installFailed, setInstallFailed] = useState(false);
  const [dismissed, setDismissed] = useState(false);

  useEffect(() => {
    const unsubs = [
      api.onUpdateAvailable((u) => {
        setInfo(u);
        setDismissed(false);
        setPhase((p) => (p === "idle" || p === "latest" || p === "error" ? "available" : p));
      }),
      api.onUpdateProgress((p) => setProgress(p.total ? p.downloaded / p.total : null)),
    ];
    return () => unsubs.forEach((p) => p.then((f) => f()));
  }, []);

  const check = useCallback(() => {
    setPhase("checking");
    setError(null);
    setInstallFailed(false);
    api
      .checkUpdate()
      .then((u) => {
        setInfo(u);
        setDismissed(false);
        setPhase(u ? "available" : "latest");
      })
      .catch((e) => {
        setError(String(e));
        setPhase("error");
      });
  }, []);

  const install = useCallback(() => {
    setPhase("downloading");
    setProgress(0);
    setError(null);
    api
      .installUpdate()
      // 正常情况下程序会直接重新打开，走不到这里。
      .then(() => setPhase("installing"))
      .catch((e) => {
        const err = errorText(e);
        setError(err.message);
        setNeedsRestart(err.restart);
        setInstallFailed(true);
        setDismissed(false);
        setPhase("error");
      });
  }, []);

  return {
    phase,
    info,
    progress,
    error,
    installFailed,
    needsRestart,
    dismissed,
    check,
    install,
    restart: () => void api.restartApp(),
    dismiss: () => setDismissed(true),
  };
}

export function ProgressBar({ value }: { value: number | null }) {
  return (
    <div className={`progress ${value === null ? "indeterminate" : ""}`}>
      <div className="progress-fill" style={value === null ? undefined : { width: `${Math.round(value * 100)}%` }} />
    </div>
  );
}

/** 主界面顶部的更新横幅：有新版本、下载中、失败时显示。 */
export function UpdateBanner({ updater }: { updater: Updater }) {
  const { phase, info, progress, error, installFailed, needsRestart, dismissed } = updater;

  if (phase === "available" && info && !dismissed) {
    return (
      <div className="banner info">
        <span>
          流界 {info.version} 已发布。更新时会先断开键鼠共享，安装完成后自动重新打开。
        </span>
        <span className="banner-actions">
          <button className="btn small" onClick={updater.dismiss}>
            稍后
          </button>
          <button className="btn small primary" onClick={updater.install}>
            立即更新
          </button>
        </span>
      </div>
    );
  }
  if (phase === "downloading" || phase === "installing") {
    return (
      <div className="banner info">
        <span className="banner-progress">
          {phase === "installing" ? "正在安装更新…" : `正在下载流界 ${info?.version ?? ""}${progress !== null ? `（${Math.round(progress * 100)}%）` : "…"}`}
          <ProgressBar value={phase === "installing" ? null : progress} />
        </span>
      </div>
    );
  }
  // 引擎已关闭时不允许关掉横幅：必须重新打开流界才能继续使用。
  if (phase === "error" && installFailed && (needsRestart || !dismissed)) {
    return (
      <div className="banner">
        <span>{error}</span>
        <span className="banner-actions">
          {!needsRestart && (
            <button className="btn small" onClick={updater.dismiss}>
              关闭
            </button>
          )}
          <button className="btn small primary" onClick={needsRestart ? updater.restart : updater.install}>
            {needsRestart ? "重新打开流界" : "重试"}
          </button>
        </span>
      </div>
    );
  }
  return null;
}
