//! macOS 低延迟模式：跨屏期间关闭 AWDL。
//!
//! Mac 用 Wi‑Fi 时，AWDL（隔空投送、接力、通用控制、随航用的点对点 Wi‑Fi）大约每 0.5 秒切换一次
//! 信道，网络停顿 60–100ms，跨屏就会周期性卡顿。关闭 `awdl0` 需要 root，所以第一次开启时请用户
//! 授权一次，安装一个很小的系统后台服务（launchd 守护进程）。它只做一件事：
//!
//! - 标志文件 [`FLAG`] 存在（且属于当前登录用户）、流界正在运行时，保持 `awdl0` 关闭（被重新打开会在半秒内再关掉）；
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
# /tmp 谁都能写：只认当前登录用户创建的标志文件，别的用户放一个同名文件不能让 AWDL 一直关着。
mine() {
  [ "$(stat -f %u "$FLAG" 2>/dev/null)" = "$(stat -f %u /dev/console 2>/dev/null)" ]
}
while true; do
  if [ -e "$FLAG" ] && mine && pgrep -ix flowacross >/dev/null 2>&1; then
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
    let _ = set_active(false);
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
///
/// 创建失败时返回错误（多半是别的用户先占了这个名字，/tmp 谁都能写）：低延迟模式这次不会生效，
/// 调用方应当让用户知道，而不是悄悄不起作用。
pub fn set_active(active: bool) -> Result<(), PlatformError> {
    let flag = std::path::Path::new(FLAG);
    if active {
        create_flag(flag).map_err(|e| PlatformError::Os(format!("无法创建标志文件 {FLAG}（{e}）")))
    } else {
        let _ = std::fs::remove_file(flag);
        Ok(())
    }
}

/// 新建一个空的标志文件。/tmp 谁都能写：别的本地用户可以预先放一个同名的符号链接指向本用户的
/// 文件，直接"写入"会顺着链接把那个文件清空。所以先删掉已有的（删的是链接本身），再用
/// "必须新建、不跟随符号链接"的方式创建；删不掉（别人的文件）就失败，不碰它。
fn create_flag(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    /// `O_NOFOLLOW`（macOS）。
    const O_NOFOLLOW: i32 = 0x0100;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .map(|_| ())
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

    /// 标志文件的位置上是一个指向别的文件的符号链接时，不能顺着它把那个文件清空。
    #[test]
    fn creating_the_flag_never_writes_through_a_symlink() {
        let dir = std::env::temp_dir().join(format!("flowacross-flag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, b"important").unwrap();
        let flag = dir.join("flag");
        std::os::unix::fs::symlink(&victim, &flag).unwrap();
        create_flag(&flag).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"important");
        // 现在是一个普通的空文件，不再是链接；重复创建也没问题。
        let meta = std::fs::symlink_metadata(&flag).unwrap();
        assert!(meta.file_type().is_file() && meta.len() == 0);
        create_flag(&flag).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quoting() {
        assert_eq!(applescript_quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
