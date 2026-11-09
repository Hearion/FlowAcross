import { useState } from "react";
import { api } from "./api";
import { NearbyList, PermissionList } from "./Dialogs";
import type { StateView } from "./types";
import { CheckIcon, CursorIcon, Logo } from "./ui";

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
      <div className="onboarding-desk" data-tauri-drag-region>
        <div className="onboarding-brand" data-tauri-drag-region>
          <Logo />
          <span>流界</span>
        </div>

        {/* 演示：光标从这台电脑滑过接缝，进入另一台。只在打开时播放一次。 */}
        <div className="scene" aria-hidden="true">
          <div className="screen hue-0 scene-a">
            <div className="screen-glass">
              <div className="screen-foot">
                <span className="screen-name">{name.trim() || "这台电脑"}</span>
              </div>
            </div>
          </div>
          <div className="screen hue-1 scene-b">
            <div className="screen-glass">
              <div className="screen-foot">
                <span className="screen-name">另一台电脑</span>
              </div>
            </div>
          </div>
          <span className="link v" />
          <span className="scene-cursor">
            <CursorIcon size={20} />
          </span>
        </div>

        <div className="onboarding-pitch">
          <h1>一套键鼠，自由跨屏</h1>
          <p>把屏幕按桌上的摆放排好，鼠标移过相贴的边，就进入了另一台电脑。键盘和剪贴板跟着一起过去。</p>
        </div>
      </div>

      <div className="onboarding-card">
        <ol className="steps" aria-label="设置步骤">
          {STEPS.map((s, i) => (
            <li key={s} className={i === step ? "on" : i < step ? "done" : ""} aria-current={i === step ? "step" : undefined}>
              <span className="step-no">{i < step ? <CheckIcon /> : i + 1}</span>
              {s}
            </li>
          ))}
        </ol>

        {step === 0 && (
          <section>
            <h2>给这台电脑起个名字</h2>
            <p>其他电脑会用这个名字找到它，之后可以在设置里修改。</p>
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
            <h2>授予系统权限</h2>
            <p>流界需要这些权限才能在电脑之间传递键盘和鼠标。授权后如果状态没有变化，重新打开流界即可。</p>
            <PermissionList permissions={state.permissions} hint={state.permission_hint} />
          </section>
        )}

        {step === 2 && (
          <section>
            <h2>连接其他电脑</h2>
            <p>在另一台电脑上也打开流界，它会出现在下面。也可以稍后在主界面点「添加设备」。</p>
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
