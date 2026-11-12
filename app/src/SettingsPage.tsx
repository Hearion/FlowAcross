import { useEffect, useState, type ReactNode } from "react";
import { api } from "./api";
import { PermissionList } from "./Dialogs";
import { ClipboardSetting, CrossScreenSettings, WinKeysSetting, useDeviceNameInput } from "./SettingsControls";
import type { Settings, StateView } from "./types";
import { ChevronLeftIcon, Logo, SETTINGS_ICONS, Toggle } from "./ui";
import { ProgressBar, type Updater } from "./Updater";

export type SettingsSection = "general" | "permissions" | "crossing" | "clipboard" | "updates" | "about";

const NAV: { id: SettingsSection; label: string }[] = [
  { id: "general", label: "通用" },
  { id: "permissions", label: "系统权限" },
  { id: "crossing", label: "跨屏与键鼠" },
  { id: "clipboard", label: "剪贴板" },
  { id: "updates", label: "软件更新" },
  { id: "about", label: "关于" },
];

interface Props {
  state: StateView;
  updater: Updater;
  section: SettingsSection;
  onSection: (s: SettingsSection) => void;
  onClose: () => void;
}

export function SettingsPage({ state, updater, section, onSection, onClose }: Props) {
  const update = (patch: Partial<Settings>) => api.updateSettings({ ...state.settings, ...patch });
  const missing = state.permissions.filter((p) => p.state !== "granted" && p.state !== "not_applicable").length;

  return (
    <div className="settings-page">
      <nav className="settings-nav">
        <button className="settings-back" onClick={onClose}>
          <ChevronLeftIcon />
          返回设备布局
        </button>
        <div className="settings-nav-title">设置</div>
        {NAV.map((n) => {
          const Icon = SETTINGS_ICONS[n.id];
          return (
            <button key={n.id} className={`settings-nav-item ${section === n.id ? "on" : ""}`} onClick={() => onSection(n.id)}>
              <span className="settings-nav-icon">
                <Icon />
              </span>
              <span className="settings-nav-label">{n.label}</span>
              {n.id === "permissions" && missing > 0 && <span className="nav-badge">{missing}</span>}
            </button>
          );
        })}
      </nav>

      <div className="settings-body">
        {section === "general" && (
          <Section title="通用">
            <Panel>
              <NameSetting current={state.local.name} />
              <AutostartSetting mac={state.local.os === "MacOs"} />
            </Panel>
            {state.local.os === "MacOs" && (
              <p className="settings-note">
                开启后「系统设置 › 通用 › 登录项」里会出现 FlowAcross。如果在那里关掉了它的「允许在后台」，开机时流界不会启动。
              </p>
            )}
          </Section>
        )}

        {section === "permissions" && (
          <Section title="系统权限" desc="流界需要这些权限才能在电脑之间传递键盘和鼠标。点「授权」后如果系统没有弹窗，会直接打开系统设置的对应页面，在列表里打开流界的开关即可。">
            <PermissionList permissions={state.permissions} hint={state.permission_hint} />
            <p className="settings-note">
              授予「输入监控」后，macOS 可能要求重新打开流界才会生效。设置页回到前台时会自动刷新授权状态。
            </p>
            <p className="settings-note">
              如果系统设置里流界的开关已经打开，这里仍显示未授权：在系统设置的列表里选中 FlowAcross，点「−」删掉，
              再点「+」重新添加「应用程序」里的 FlowAcross，然后退出并重新打开流界。更新过流界后常会出现这种情况。
            </p>
          </Section>
        )}

        {section === "crossing" && (
          <Section title="跨屏与键鼠">
            <Panel>
              <CrossScreenSettings settings={state.settings} update={update} />
            </Panel>
            {state.local.os === "MacOs" ? (
              <>
                <Panel>
                  <WinKeysSetting settings={state.settings} update={update} />
                </Panel>
                <p className="settings-note">
                  用 Windows 电脑的键盘控制这台 Mac 时生效，改完立即生效。Mac 控制 Windows 时不受影响，⌘ 始终对应 Ctrl。
                </p>
              </>
            ) : (
              <p className="settings-note">
                用这台电脑的键盘控制 Mac 时，Ctrl、Win、Alt 对应 Mac 的哪个键，在 Mac 上的流界里设置（设置 › 跨屏与键鼠）。
              </p>
            )}
            {state.low_latency.supported && <LowLatencySetting state={state} />}
          </Section>
        )}

        {section === "clipboard" && (
          <Section title="剪贴板" desc="内容通过加密连接直接在电脑之间传输，不经过任何服务器。">
            <Panel>
              <ClipboardSetting settings={state.settings} update={update} />
              <div className="setting inline">
                <div className="setting-text">
                  <span>收到的文件</span>
                  <small>{state.receive_dir ?? "找不到本机的下载文件夹，暂时无法接收文件"}</small>
                </div>
                {state.receive_dir && (
                  <button className="btn small" onClick={() => api.openReceivedFiles()}>
                    打开文件夹
                  </button>
                )}
              </div>
            </Panel>
            <p className="settings-note">
              {"在一台电脑上复制文件后，把键鼠切到另一台，文件会自动传过去并放进它的剪贴板，在 Finder 或资源管理器里粘贴即可。" +
                "传完之前粘贴的还是原来的内容，进度显示在主界面侧栏。两边都需要流界 0.1.5 或更新版本。"}
            </p>
          </Section>
        )}

        {section === "updates" && (
          <Section title="软件更新">
            <Panel>
              <UpdateStatus version={state.about.version} updater={updater} />
              <div className="setting inline">
                <div className="setting-text">
                  <span>自动检查更新</span>
                  <small>启动时和之后每隔 6 小时检查一次，有新版本时提示你，不会自动安装</small>
                </div>
                <Toggle
                  label="自动检查更新"
                  checked={state.settings.auto_check_updates}
                  onChange={(v) => update({ auto_check_updates: v })}
                />
              </div>
            </Panel>
            <p className="settings-note">
              更新时流界会先断开与其他电脑的键鼠共享，安装完成后自动重新打开并重新连接。两台电脑请尽量保持相同版本。
            </p>
          </Section>
        )}

        {section === "about" && (
          <Section title="关于">
            <div className="about-head">
              <Logo size={56} />
              <div>
                <span className="about-name">流界</span>
                <span className="about-latin">FlowAcross</span>
                <span className="muted">一套键鼠，自由跨屏</span>
              </div>
            </div>
            <Panel>
              <Row label="版本" value={`${state.about.version}（协议 v${state.about.protocol}）`} />
              <Row label="本机指纹" value={state.about.fingerprint} mono />
              <Row label="监听端口" value={`UDP ${state.port}`} />
              <div className="setting inline">
                <div className="setting-text">
                  <span>数据目录</span>
                  <small className="selectable">{state.about.data_dir}</small>
                </div>
                <button className="btn small" onClick={() => api.openDataDir()}>
                  打开
                </button>
              </div>
              <div className="setting inline">
                <div className="setting-text">
                  <span>日志</span>
                  <small>遇到连接问题时，把日志文件发给开发者</small>
                </div>
                <button className="btn small" onClick={() => api.openLogs()}>
                  打开日志文件夹
                </button>
              </div>
            </Panel>
          </Section>
        )}
      </div>
    </div>
  );
}

function Section({ title, desc, children }: { title: string; desc?: string; children: ReactNode }) {
  return (
    <section className="settings-section">
      <h1>{title}</h1>
      {desc && <p className="settings-desc">{desc}</p>}
      {children}
    </section>
  );
}

function Panel({ children }: { children: ReactNode }) {
  return <div className="panel settings-panel">{children}</div>;
}

function Row({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="setting inline">
      <div className="setting-text">
        <span>{label}</span>
      </div>
      <span className={`row-value ${mono ? "mono" : ""}`}>{value}</span>
    </div>
  );
}

/** 开机启动：状态每次从系统读取（用户可能在任务管理器或系统设置里改过），窗口回到前台时刷新。 */
function AutostartSetting({ mac }: { mac: boolean }) {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const load = () =>
      api
        .getAutostart()
        .then((v) => {
          setEnabled(v);
          setError(null);
        })
        .catch((e) => setError(String(e)));
    load();
    window.addEventListener("focus", load);
    return () => window.removeEventListener("focus", load);
  }, []);

  const toggle = (v: boolean) => {
    setBusy(true);
    setError(null);
    api
      .setAutostart(v)
      .then(setEnabled)
      .catch((e) => {
        setError(String(e));
        // 失败后以系统实际状态为准。
        api.getAutostart().then(setEnabled, () => {});
      })
      .finally(() => setBusy(false));
  };

  const status = `登录系统后在后台运行，只显示在${mac ? "菜单栏" : "托盘"}，已配对的电脑自动连上`;
  return (
    <div className="setting inline">
      <div className="setting-text">
        <span>开机时自动启动</span>
        <small className={error ? "text-danger" : undefined}>{error ?? status}</small>
      </div>
      <Toggle label="开机时自动启动" checked={enabled ?? false} disabled={busy} onChange={toggle} />
    </div>
  );
}

function LowLatencySetting({ state }: { state: StateView }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ll = state.low_latency;
  const on = state.settings.low_latency;

  const run = (p: Promise<void>) => {
    setBusy(true);
    setError(null);
    p.catch((e) => setError(String(e))).finally(() => setBusy(false));
  };

  let status = "Mac 用 Wi‑Fi 时，跨屏如果每秒卡一下，打开这个开关";
  if (busy) status = "正在等待系统授权…";
  else if (on && ll.active) status = "生效中：已关闭 AWDL，断开所有设备后自动恢复";
  else if (on) status = "已开启：有设备连上后关闭 AWDL";

  return (
    <>
      <Panel>
        <div className="setting">
          <div className="update-row">
            <div className="setting-text">
              <span>低延迟模式</span>
              <small className={error ? "text-danger" : undefined}>{error ?? status}</small>
            </div>
            <Toggle label="低延迟模式" checked={on} disabled={busy} onChange={(v) => run(api.setLowLatency(v))} />
          </div>
        </div>
        {ll.installed && (
          <div className="setting inline">
            <div className="setting-text">
              <span>后台服务</span>
              <small>已安装。不用低延迟模式时可以移除，需要管理员授权</small>
            </div>
            <button className="btn small" disabled={busy} onClick={() => run(api.removeLowLatency())}>
              移除
            </button>
          </div>
        )}
      </Panel>
      <p className="settings-note">
        Mac 的 AWDL（隔空投送、接力、通用控制、随航用的点对点 Wi‑Fi）大约每半秒切换一次信道，网络会停顿几十到上百毫秒。
        开启后，有电脑连着时流界会关闭 AWDL，这期间上述功能不可用；全部断开、关闭开关或退出流界后自动恢复。
        第一次开启需要输入一次 Mac 的密码，用来安装一个很小的后台服务。Mac 接有线网络时不需要开启。
      </p>
    </>
  );
}

function UpdateStatus({ version, updater }: { version: string; updater: Updater }) {
  const { phase, info, progress, error, needsRestart, installFailed } = updater;
  const busy = phase === "checking" || phase === "downloading" || phase === "installing";

  let status: string;
  switch (phase) {
    case "checking":
      status = "正在检查更新…";
      break;
    case "latest":
      status = "已是最新版本";
      break;
    case "available":
      status = `新版本 ${info?.version} 可以安装${info?.date ? `（${info.date} 发布）` : ""}`;
      break;
    case "downloading":
      status = `正在下载 ${info?.version}${progress !== null ? `（${Math.round(progress * 100)}%）` : "…"}`;
      break;
    case "installing":
      status = "正在安装，完成后流界会自动重新打开";
      break;
    case "error":
      status = error ?? "更新失败";
      break;
    default:
      status = import.meta.env.DEV ? "开发版" : "";
  }

  let action = (
    <button className="btn small" onClick={updater.check} disabled={busy}>
      {phase === "checking" ? "检查中…" : "检查更新"}
    </button>
  );
  if (phase === "available" || (phase === "error" && installFailed && !needsRestart)) {
    action = (
      <button className="btn small primary" onClick={updater.install}>
        {phase === "error" ? "重试" : "立即更新"}
      </button>
    );
  } else if (needsRestart) {
    action = (
      <button className="btn small primary" onClick={updater.restart}>
        重新打开流界
      </button>
    );
  }

  return (
    <div className="setting">
      <div className="update-row">
        <div className="setting-text">
          <span>当前版本 {version}</span>
          {status && <small className={phase === "error" ? "text-danger" : undefined}>{status}</small>}
        </div>
        {action}
      </div>
      {(phase === "downloading" || phase === "installing") && (
        <ProgressBar value={phase === "installing" ? null : progress} />
      )}
      {info?.notes && (phase === "available" || phase === "downloading") && (
        <pre className="update-notes">{info.notes}</pre>
      )}
    </div>
  );
}

function NameSetting({ current }: { current: string }) {
  const input = useDeviceNameInput(current);
  return (
    <div className="setting inline">
      <div className="setting-text">
        <span>设备名称</span>
        <small>其他电脑会用这个名字找到这台电脑</small>
      </div>
      <input className="text-input" {...input} />
    </div>
  );
}
