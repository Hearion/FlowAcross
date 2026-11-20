//! 按前台应用自动把键鼠锁定在本机（纯逻辑），以及连接延迟的分级。
//!
//! 全屏游戏、演示、远程桌面时，鼠标甩到屏幕边就切到别的电脑很烦人。两种触发方式，都由用户打开：
//!
//! - 前台窗口是全屏的；
//! - 前台应用在用户列出的名单里（按应用名，不分大小写）。
//!
//! 前台应用是什么由平台层查（`flow_platform::foreground`），这里只做判定。

use serde::{Deserialize, Serialize};

/// 名单最多这么多个应用，每个名字最多这么多个字符（配置文件被手动改坏时不至于拖慢每次判定）。
pub const MAX_APPS: usize = 32;
pub const MAX_APP_NAME: usize = 64;
/// 「最近在前台的应用」最多记这么多个，给设置页添加名单时挑选。
pub const MAX_RECENT: usize = 8;

/// 现在在前台的应用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreground {
    /// 应用名：macOS 是应用的显示名，Windows 是可执行文件名（不带 `.exe`），Linux 是窗口的类名。
    pub app: String,
    /// 前台窗口是全屏的。
    pub fullscreen: bool,
}

/// 自动锁定的原因，带着应用名（给界面和提示用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "app", rename_all = "snake_case")]
pub enum AutoLock {
    /// 这个应用在名单里。
    App(String),
    /// 这个应用正全屏显示。
    Fullscreen(String),
}

impl AutoLock {
    pub fn app(&self) -> &str {
        match self {
            Self::App(a) | Self::Fullscreen(a) => a,
        }
    }
}

fn same_app(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// 整理一个应用名：去掉首尾空白和控制字符，截到 [`MAX_APP_NAME`] 个字符；空的返回 `None`。
pub fn clean_app_name(name: &str) -> Option<String> {
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_APP_NAME)
        .collect();
    let name = name.trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// 整理界面或配置文件给的名单：逐个 [`clean_app_name`]，去掉重复的（不分大小写），最多 [`MAX_APPS`] 个。
pub fn sanitize_apps(apps: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in apps.iter().filter_map(|a| clean_app_name(a)) {
        if out.len() >= MAX_APPS {
            break;
        }
        if !out.iter().any(|o| same_app(o, &name)) {
            out.push(name);
        }
    }
    out
}

/// 现在要不要自动锁定。`own`：流界自己的应用名——流界的窗口在前台时不算（否则打开设置页
/// 把自己加进名单就再也切不出去，全屏看流界的窗口也不该锁）。名单优先于全屏：原因里说得更具体。
pub fn auto_lock(
    on_fullscreen: bool,
    apps: &[String],
    foreground: Option<&Foreground>,
    own: &str,
) -> Option<AutoLock> {
    let fg = foreground?;
    if fg.app.trim().is_empty() || same_app(&fg.app, own) {
        return None;
    }
    if apps.iter().any(|a| same_app(a, &fg.app)) {
        return Some(AutoLock::App(fg.app.clone()));
    }
    (on_fullscreen && fg.fullscreen).then(|| AutoLock::Fullscreen(fg.app.clone()))
}

/// 把刚看到的前台应用记进「最近在前台的应用」：最新的在最前，不重复，最多 [`MAX_RECENT`] 个。
/// 流界自己不记。返回列表有没有变化。
pub fn remember_app(recent: &mut Vec<String>, app: &str, own: &str) -> bool {
    let Some(app) = clean_app_name(app) else {
        return false;
    };
    if same_app(&app, own) || recent.first().is_some_and(|r| same_app(r, &app)) {
        return false;
    }
    recent.retain(|r| !same_app(r, &app));
    recent.insert(0, app);
    recent.truncate(MAX_RECENT);
    true
}

/// 连接延迟（往返时间）大致算好、一般还是差。键鼠操作对延迟很敏感：有线局域网通常 1–3 毫秒，
/// 好的 Wi‑Fi 在 10 毫秒以内；到几十毫秒光标就明显跟手慢。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyLevel {
    Good,
    Fair,
    Poor,
}

/// 延迟不超过这么多毫秒算好 / 一般。
pub const LATENCY_GOOD_MS: u32 = 20;
pub const LATENCY_FAIR_MS: u32 = 60;

pub fn latency_level(ms: u32) -> LatencyLevel {
    if ms <= LATENCY_GOOD_MS {
        LatencyLevel::Good
    } else if ms <= LATENCY_FAIR_MS {
        LatencyLevel::Fair
    } else {
        LatencyLevel::Poor
    }
}

/// 界面上显示的延迟要不要更新：分级变了，或数值变化超过 2 毫秒且超过两成。
/// 延迟每时每刻都在抖，每次都推给界面没有意义。
pub fn latency_changed(shown: Option<u32>, now: Option<u32>) -> bool {
    match (shown, now) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            let diff = a.abs_diff(b);
            latency_level(a) != latency_level(b) || (diff > 2 && diff * 5 > a.max(b))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fg(app: &str, fullscreen: bool) -> Foreground {
        Foreground {
            app: app.into(),
            fullscreen,
        }
    }

    #[test]
    fn listed_apps_lock_regardless_of_case_and_fullscreen() {
        let apps = vec!["Parallels Desktop".to_owned(), "steam".to_owned()];
        assert_eq!(
            auto_lock(false, &apps, Some(&fg("Steam", false)), "流界"),
            Some(AutoLock::App("Steam".into()))
        );
        // 名单优先于全屏。
        assert_eq!(
            auto_lock(true, &apps, Some(&fg("parallels desktop", true)), "流界"),
            Some(AutoLock::App("parallels desktop".into()))
        );
        assert_eq!(
            auto_lock(false, &apps, Some(&fg("Safari", true)), "流界"),
            None
        );
        assert_eq!(auto_lock(false, &apps, None, "流界"), None);
    }

    #[test]
    fn fullscreen_locks_only_when_turned_on() {
        assert_eq!(
            auto_lock(true, &[], Some(&fg("Keynote", true)), "流界"),
            Some(AutoLock::Fullscreen("Keynote".into()))
        );
        assert_eq!(
            auto_lock(true, &[], Some(&fg("Keynote", false)), "流界"),
            None
        );
        assert_eq!(
            auto_lock(false, &[], Some(&fg("Keynote", true)), "流界"),
            None
        );
        // 查不到应用名的窗口不算。
        assert_eq!(auto_lock(true, &[], Some(&fg(" ", true)), "流界"), None);
    }

    #[test]
    fn our_own_window_never_locks() {
        let apps = vec!["FlowAcross".to_owned()];
        assert_eq!(
            auto_lock(true, &apps, Some(&fg("flowacross", true)), "FlowAcross"),
            None
        );
    }

    #[test]
    fn app_lists_are_cleaned_and_capped() {
        let cleaned = sanitize_apps(vec![
            "  Steam ".into(),
            "steam".into(),
            "".into(),
            "a\u{0}b\n".into(),
            "x".repeat(200),
        ]);
        assert_eq!(cleaned[0], "Steam");
        assert_eq!(cleaned[1], "ab");
        assert_eq!(cleaned[2].chars().count(), MAX_APP_NAME);
        assert_eq!(cleaned.len(), 3);
        let many = sanitize_apps((0..100).map(|i| format!("app{i}")).collect());
        assert_eq!(many.len(), MAX_APPS);
    }

    #[test]
    fn recent_apps_keep_the_newest_first_without_repeats() {
        let mut recent = Vec::new();
        assert!(remember_app(&mut recent, "Safari", "流界"));
        assert!(remember_app(&mut recent, "Xcode", "流界"));
        // 还在前台：没有变化。
        assert!(!remember_app(&mut recent, "xcode", "流界"));
        // 流界自己、空名字不记。
        assert!(!remember_app(&mut recent, "流界", "流界"));
        assert!(!remember_app(&mut recent, "  ", "流界"));
        assert!(remember_app(&mut recent, "Safari", "流界"));
        assert_eq!(recent, vec!["Safari", "Xcode"]);
        for i in 0..20 {
            remember_app(&mut recent, &format!("app{i}"), "流界");
        }
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0], "app19");
    }

    #[test]
    fn latency_levels_and_when_to_refresh() {
        assert_eq!(latency_level(0), LatencyLevel::Good);
        assert_eq!(latency_level(20), LatencyLevel::Good);
        assert_eq!(latency_level(21), LatencyLevel::Fair);
        assert_eq!(latency_level(60), LatencyLevel::Fair);
        assert_eq!(latency_level(61), LatencyLevel::Poor);
        // 小抖动不更新。
        assert!(!latency_changed(Some(3), Some(4)));
        assert!(!latency_changed(Some(3), Some(5)));
        assert!(!latency_changed(Some(40), Some(45)));
        assert!(!latency_changed(None, None));
        // 变化明显、跨了分级、连上 / 断开：更新。
        assert!(latency_changed(Some(3), Some(8)));
        assert!(latency_changed(Some(40), Some(55)));
        assert!(latency_changed(Some(20), Some(22)));
        assert!(latency_changed(None, Some(1)));
        assert!(latency_changed(Some(1), None));
    }
}
