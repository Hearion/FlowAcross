import { useEffect, useState, type ReactNode } from "react";
import { api } from "./api";
import { PermissionList } from "./Dialogs";
import { ClipboardSetting, CrossScreenSettings } from "./SettingsControls";
import type { Settings, StateView } from "./types";
import { Logo, Toggle } from "./ui";

export type SettingsSection = "general" | "permissions" | "crossing" | "clipboard" | "updates" | "account" | "about";

const NAV: { id: SettingsSection; label: string }[] = [
  { id: "general", label: "通用" },
  { id: "permissions", label: "系统权限" },
  { id: "crossing", label: "跨屏与键鼠" },
  { id: "clipboard", label: "剪贴板" },
  { id: "updates", label: "软件更新" },
  { id: "account", label: "账号" },
  { id: "about", label: "关于" },
];

interface Props {
  state: StateView;
  section: SettingsSection;
  onSection: (s: SettingsSection) => void;
  onClose: () => void;
}

export function SettingsPage({ state, section, onSection, onClose }: Props) {
  const update = (patch: Partial<Settings>) => api.updateSettings({ ...state.settings, ...patch });
  const missing = state.permissions.filter((p) => p.state !== "granted" && p.state !== "not_applicable").length;

  return (
    <div className="settings-page">
      <nav className="settings-nav">
        <button className="settings-back" onClick={onClose}>
          ‹ 返回设备布局
        </button>
        {NAV.map((n) => (
          <button key={n.id} className={`settings-nav-item ${section === n.id ? "on" : ""}`} onClick={() => onSection(n.id)}>
            {n.label}
            {n.id === "permissions" && missing > 0 && <span className="nav-badge">{missing}</span>}
          </button>
        ))}
      </nav>

      <div className="settings-body">
        {section === "general" && (
          <Section title="通用">
            <Panel>
              <NameSetting current={state.local.name} />
              <div className="setting inline">
                <div className="setting-text">
                  <span>开机时自动启动</span>
                  <small>登录系统后在后台运行，已配对的电脑自动连上（即将支持）</small>
                </div>
                <Toggle label="开机时自动启动" checked={false} onChange={() => {}} disabled />
              </div>
            </Panel>
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
          </Section>
        )}

        {section === "clipboard" && (
          <Section title="剪贴板" desc="内容通过加密连接直接在电脑之间传输，不经过任何服务器。">
            <Panel>
              <ClipboardSetting settings={state.settings} update={update} />
            </Panel>
          </Section>
        )}

        {section === "updates" && (
          <Section title="软件更新">
            <Panel>
              <div className="setting inline">
                <div className="setting-text">
                  <span>当前版本</span>
                  <small>流界 {state.about.version}（开发版）</small>
                </div>
                <button className="btn small" disabled title="正式版提供">
                  检查更新
                </button>
              </div>
              <div className="setting inline">
                <div className="setting-text">
                  <span>自动下载并安装更新</span>
                  <small>正式发布后开启。两台电脑版本不一致时，流界会提示升级</small>
                </div>
                <Toggle label="自动更新" checked={false} onChange={() => {}} disabled />
              </div>
            </Panel>
          </Section>
        )}

        {section === "account" && (
          <Section title="账号">
            <div className="account-empty">
              <Logo size={40} />
              <strong>无需账号即可使用全部基础功能</strong>
              <span>同一局域网内的电脑直接配对、加密直连。账号与跨网络同步将在后续版本提供。</span>
              <button className="btn" disabled>
                登录（即将推出）
              </button>
            </div>
          </Section>
        )}

        {section === "about" && (
          <Section title="关于">
            <div className="about-head">
              <Logo size={56} />
              <div>
                <strong>流界 FlowAcross</strong>
                <span>一套键鼠，自由跨屏</span>
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

function NameSetting({ current }: { current: string }) {
  const [value, setValue] = useState(current);
  useEffect(() => setValue(current), [current]);
  const commit = () => {
    if (value.trim() !== current) api.setDeviceName(value);
  };
  return (
    <div className="setting">
      <div className="setting-text">
        <span>设备名称</span>
        <small>其他电脑会用这个名字找到这台电脑</small>
      </div>
      <input
        className="text-input"
        value={value}
        maxLength={40}
        onChange={(e) => setValue(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
      />
    </div>
  );
}
