//! macOS 低延迟模式：跨屏期间关闭 AWDL。
//!
//! Mac 用 Wi‑Fi 时，AWDL（隔空投送、接力、通用控制、随航用的点对点 Wi‑Fi）大约每 0.5 秒切换一次
//! 信道，网络停顿 60–100ms，跨屏就会周期性卡顿。关闭 `awdl0` 需要 root，所以第一次开启时请用户
//! 授权一次，安装一个很小的系统后台服务（launchd 守护进程）。它只做一件事：
//!
//! - 标志文件 [`FLAG`] 存在、且流界正在运行时，保持 `awdl0` 关闭（被重新打开会在半秒内再关掉）；
//! - 否则，如果是它关掉的，就恢复打开。
//!
//! 流界在"开启了低延迟模式且有设备连着"时创建标志文件，其余时候删除。标志文件放在 /tmp，
//! 重启后自动清掉；流界异常退出时，守护进程发现流界不在运行也会恢复 AWDL。

use std::path::Path;
use std::process::Command;

use crate::PlatformError;
use crate::root_install::{self, sh_quote};

const LABEL: &str = "app.flowacross.awdl-guard";
const PLIST_PATH: &str = "/Library/LaunchDaemons/app.flowacross.awdl-guard.plist";
const SCRIPT_DIR: &str = "/Library/Application Support/FlowAcross";
const SCRIPT_PATH: &str = "/Library/Application Support/FlowAcross/awdl-guard.sh";
const FLAG: &str = "/tmp/app.flowacross.lowlatency";

const SCRIPT: &str = r#"#!/bin/sh
# 流界低延迟模式：流界运行且标志文件存在时保持 awdl0 关闭，否则恢复。由流界安装，可在流界设置里移除。
FLAG=/tmp/app.flowacross.lowlatency
DOWNED=0
while true; do
  if [ -e "$FLAG" ] && pgrep -ix flowacross >/dev/null 2>&1; then
    if ifconfig awdl0 2>/dev/null | head -n 1 | grep -q '<UP'; then
      ifconfig awdl0 down && DOWNED=1
    fi
  elif [ "$DOWNED" = 1 ]; then
    ifconfig awdl0 up
    DOWNED=0
  fi
  sleep 0.5
done
"#;

const PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>app.flowacross.awdl-guard</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>/Library/Application Support/FlowAcross/awdl-guard.sh</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
"#;

/// 后台服务是否已安装。
pub fn installed() -> bool {
    Path::new(PLIST_PATH).exists() && Path::new(SCRIPT_PATH).exists()
}

/// 安装后台服务（弹出系统的管理员授权窗口）。用户取消时返回错误。
///
/// 脚本和 plist 的内容直接写在 root 命令里（见 [`root_install`]），由 root 写进只有 root 能写的
/// 系统目录。不经过任何用户可写的临时文件：否则授权弹窗停留期间，同一用户下的其他进程
/// 可以把临时文件换掉，换来以 root 身份常驻执行。
pub fn install() -> Result<(), PlatformError> {
    let cmd = format!(
        "{dir} && {script} && {plist} && \
         (launchctl bootout system/{LABEL} 2>/dev/null; true) && \
         launchctl bootstrap system {plist_path}",
        dir = root_install::ensure_root_dir(SCRIPT_DIR),
        script = root_install::write_file(SCRIPT_PATH, SCRIPT, "755"),
        plist = root_install::write_file(PLIST_PATH, PLIST, "644"),
        plist_path = sh_quote(PLIST_PATH),
    );
    run_as_admin(
        &cmd,
        "流界需要管理员权限，安装低延迟模式的后台服务（跨屏时关闭 AWDL）。",
    )
}

/// 移除后台服务，并恢复 AWDL（弹出管理员授权窗口）。
pub fn uninstall() -> Result<(), PlatformError> {
    set_active(false);
    let cmd = format!(
        "(launchctl bootout system/{LABEL} 2>/dev/null; true) && rm -f {plist} {script} && \
         (rmdir {dir} 2>/dev/null; true) && (ifconfig awdl0 up 2>/dev/null; true)",
        plist = sh_quote(PLIST_PATH),
        script = sh_quote(SCRIPT_PATH),
        dir = sh_quote(SCRIPT_DIR),
    );
    run_as_admin(&cmd, "流界需要管理员权限，移除低延迟模式的后台服务。")
}

/// 创建或删除标志文件，通知后台服务关闭 / 恢复 AWDL。
pub fn set_active(active: bool) {
    if active {
        let _ = std::fs::write(FLAG, b"");
    } else {
        let _ = std::fs::remove_file(FLAG);
    }
}

/// 放进 AppleScript 字符串字面量时转义反斜杠和双引号。
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

fn run_as_admin(cmd: &str, prompt: &str) -> Result<(), PlatformError> {
    let script = format!(
        "do shell script {} with administrator privileges with prompt {}",
        applescript_quote(cmd),
        applescript_quote(prompt)
    );
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| PlatformError::Os(format!("无法请求管理员授权：{e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // 用户点了取消：AppleScript 错误 -128。
    if err.contains("-128") {
        return Err(PlatformError::Os("已取消管理员授权".into()));
    }
    Err(PlatformError::Os(err.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(applescript_quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
