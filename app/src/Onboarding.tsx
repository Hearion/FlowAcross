import { useState } from "react";
import { api } from "./api";
import { NearbyList, PermissionList } from "./Dialogs";
import type { StateView } from "./types";
import { Logo } from "./ui";

const STEPS = ["设备名称", "系统权限", "连接电脑"] as const;

export function Onboarding({ state }: { state: StateView }) {
  const [step, setStep] = useState(0);
  const [name, setName] = useState(state.local.name);
  const allGranted = state.permissions.every((p) => p.state === "granted" || p.state === "not_applicable");

  const next = async () => {
    if (step === 0) await api.setDeviceName(name);
    if (step < STEPS.length - 1) setStep(step + 1);
    else await api.finishOnboarding();
  };

  return (
    <div className="onboarding">
      <div className="onboarding-card">
        <div className="onboarding-brand">
          <Logo />
          <span>流界</span>
        </div>
        <ol className="steps">
          {STEPS.map((s, i) => (
            <li key={s} className={i === step ? "on" : i < step ? "done" : ""}>
              <span className="step-no">{i < step ? "✓" : i + 1}</span>
              {s}
            </li>
          ))}
        </ol>

        {step === 0 && (
          <section>
            <h1>一套键鼠，自由跨屏</h1>
            <p className="muted">先给这台电脑起个名字，其他电脑会用这个名字找到它。</p>
            <label className="field">
              <span>设备名称</span>
              <input
                autoFocus
                value={name}
                maxLength={40}
                onChange={(e) => setName(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && next()}
              />
            </label>
          </section>
        )}

        {step === 1 && (
          <section>
            <h1>授予系统权限</h1>
            <p className="muted">
              流界需要这些权限才能在电脑之间传递键盘和鼠标。授权后如果状态没有变化，可以重新打开流界。
            </p>
            <PermissionList permissions={state.permissions} hint={state.permission_hint} />
          </section>
        )}

        {step === 2 && (
          <section>
            <h1>连接其他电脑</h1>
            <p className="muted">在另一台电脑上也打开流界，它会出现在下面。也可以稍后在主界面点「添加」。</p>
            <NearbyList nearby={state.nearby} />
          </section>
        )}

        <div className="onboarding-actions">
          {step > 0 && (
            <button className="btn" onClick={() => setStep(step - 1)}>
              上一步
            </button>
          )}
          <button className="btn primary" onClick={next}>
            {step === STEPS.length - 1 ? "开始使用" : step === 1 && !allGranted ? "稍后再说" : "下一步"}
          </button>
        </div>
      </div>
    </div>
  );
}
