//! 本地配置：设备名称、设置、已配对设备及其在布局中的位置。保存在应用数据目录的 config.json。

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use flow_core::keys::WinToMacKeys;
use flow_core::layout::{DeviceId, EdgeMapping};
use flow_core::protocol::{DisplayInfo, Os};
use serde::{Deserialize, Serialize};

const FILE: &str = "config.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldKey {
    #[default]
    None,
    Ctrl,
    Alt,
    Shift,
}

/// PC（Windows）键盘控制本机（Mac）时修饰键怎么对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WinKeysPreset {
    /// Ctrl → ⌘（Ctrl+C 复制），和以前的版本一致。
    #[default]
    Windows,
    /// 按 Mac 键盘位置：Alt → ⌘、Win → ⌥。
    MacPosition,
    /// 原样直通：Win → ⌘。
    Direct,
    /// 使用 `Settings::win_keys_custom`。
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 按住这个键才允许跨屏。
    pub hold_key: HoldKey,
    /// 贴边停留多少毫秒才切换。
    pub dwell_ms: u64,
    /// 屏幕角落不切换。
    pub corner_guard: bool,
    /// 尺寸不同的屏幕之间如何对应。
    pub mapping: EdgeMapping,
    /// 共享剪贴板文本。
    pub clipboard: bool,
    /// 共享剪贴板时也传复制的文件（需要 `clipboard` 同时打开）。
    pub clipboard_files: bool,
    /// 暂停共享（键鼠不再流转到其他设备，连接保持）。
    pub sharing_paused: bool,
    /// 启动时和之后每隔几小时检查一次新版本（只提示，不自动安装）。
    pub auto_check_updates: bool,
    /// 低延迟模式（仅 macOS）：有设备连着时关闭 AWDL，消除 Wi‑Fi 周期性停顿。
    pub low_latency: bool,
    /// 被 PC 键盘控制时（本机是 Mac）Ctrl、Win、Alt 对应哪个键。按键由被控制的一方换算，
    /// 所以这项设置在 Mac 上生效。
    pub win_keys: WinKeysPreset,
    /// `win_keys` 为 `Custom` 时的对应关系。
    pub win_keys_custom: WinToMacKeys,
}

impl Settings {
    /// 当前生效的 PC 键盘 → Mac 修饰键对应关系。
    pub fn win_to_mac_keys(&self) -> WinToMacKeys {
        match self.win_keys {
            WinKeysPreset::Windows => WinToMacKeys::WINDOWS,
            WinKeysPreset::MacPosition => WinToMacKeys::MAC_POSITION,
            WinKeysPreset::Direct => WinToMacKeys::DIRECT,
            WinKeysPreset::Custom => self.win_keys_custom,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hold_key: HoldKey::None,
            dwell_ms: 0,
            corner_guard: true,
            mapping: EdgeMapping::Continuous,
            clipboard: true,
            clipboard_files: true,
            sharing_paused: false,
            auto_check_updates: true,
            low_latency: false,
            win_keys: WinKeysPreset::Windows,
            win_keys_custom: WinToMacKeys::WINDOWS,
        }
    }
}

/// 一台已配对（可信）的设备。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerRecord {
    /// 设备 ID = 证书指纹，连接时据此判断是否可信。
    pub id: DeviceId,
    pub name: String,
    pub os: Os,
    /// 最近一次上报的显示器（设备离线时布局图仍按此显示）。
    #[serde(default)]
    pub displays: Vec<DisplayInfo>,
    /// 该设备显示器在布局空间中的整体偏移；`None` 表示还没放进布局。
    #[serde(default)]
    pub offset: Option<(f64, f64)>,
    /// 最近一次成功连接的地址，mDNS 暂时找不到对方时用来重连。
    #[serde(default)]
    pub last_addrs: Vec<SocketAddr>,
    /// 暂停向这台设备共享键鼠。
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 用户设置的设备名称；`None` 表示使用系统电脑名。
    pub device_name: Option<String>,
    /// 是否完成了首次引导。
    pub onboarded: bool,
    pub settings: Settings,
    pub peers: Vec<PeerRecord>,
    /// 本机显示器在布局空间中的整体偏移。
    pub local_offset: (f64, f64),
}

/// 配置文件存在但读不了或解析不了。
#[derive(Debug, Clone, PartialEq)]
pub struct LoadProblem {
    /// 读取或解析失败的原因。
    pub reason: String,
    /// 原文件改名后的备份；`None` 表示连改名都失败了，这时不能再写配置，否则会覆盖原文件。
    pub backup: Option<PathBuf>,
}

impl LoadProblem {
    /// 给用户看的说明（原因 + 处理建议）。
    pub fn message(&self) -> String {
        match &self.backup {
            Some(b) => format!(
                "配置文件损坏，无法读取（{}）。已把原文件备份为 {}，配对的设备和布局需要重新设置。",
                self.reason,
                b.display()
            ),
            None => format!(
                "配置文件无法读取（{}），也无法备份。为了不覆盖原文件，这次运行的设置不会保存。请检查流界数据文件夹的权限后重新打开流界。",
                self.reason
            ),
        }
    }
}

impl Config {
    /// 读取配置。文件不存在时返回默认配置；文件损坏时先把它改名备份，再返回默认配置和问题说明，
    /// 而不是静默丢弃——否则下次保存就会用空配置覆盖掉所有配对。
    pub fn load(dir: &Path) -> Result<Config, LoadProblem> {
        let path = dir.join(FILE);
        let reason = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(c) => return Ok(c),
                Err(e) => e.to_string(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => e.to_string(),
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = dir.join(format!("{FILE}.bad-{stamp}"));
        let backup = fs::rename(&path, &backup).ok().map(|()| backup);
        Err(LoadProblem { reason, backup })
    }

    /// 先写临时文件再改名，避免写到一半断电导致配置损坏。
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let tmp: PathBuf = dir.join(format!("{FILE}.tmp"));
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )?;
        fs::rename(tmp, dir.join(FILE))
    }

    pub fn peer(&self, id: &DeviceId) -> Option<&PeerRecord> {
        self.peers.iter().find(|p| &p.id == id)
    }

    pub fn peer_mut(&mut self, id: &DeviceId) -> Option<&mut PeerRecord> {
        self.peers.iter_mut().find(|p| &p.id == id)
    }

    pub fn is_trusted(&self, id: &DeviceId) -> bool {
        self.peer(id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tolerates_missing_fields() {
        let dir = std::env::temp_dir().join(format!("flowacross-cfg-{}", std::process::id()));
        let mut c = Config {
            onboarded: true,
            ..Default::default()
        };
        c.peers.push(PeerRecord {
            id: DeviceId("ab".into()),
            name: "书房台式机".into(),
            os: Os::Windows,
            displays: vec![],
            offset: Some((1.0, 2.0)),
            last_addrs: vec![],
            paused: false,
        });
        c.save(&dir).unwrap();
        assert_eq!(Config::load(&dir), Ok(c));

        // 旧版本配置缺字段时用默认值补齐
        fs::write(dir.join(FILE), br#"{"onboarded":true}"#).unwrap();
        let old = Config::load(&dir).unwrap();
        assert!(old.onboarded && old.settings.clipboard && old.settings.auto_check_updates);
        // 旧配置没有键位设置：保持以前的 Ctrl → ⌘。
        assert_eq!(old.settings.win_to_mac_keys(), WinToMacKeys::WINDOWS);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_is_default_and_corrupt_is_backed_up() {
        let dir = std::env::temp_dir().join(format!("flowacross-cfg-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 第一次启动：没有配置文件，不算问题。
        assert_eq!(Config::load(&dir), Ok(Config::default()));

        // 写坏的配置：改名备份，内容原样保留，不再占着 config.json。
        let broken = br#"{"onboarded":true,"peers":[{"id":"ab""#;
        fs::write(dir.join(FILE), broken).unwrap();
        let problem = Config::load(&dir).unwrap_err();
        let backup = problem.backup.clone().expect("应当备份");
        assert_eq!(fs::read(&backup).unwrap(), broken);
        assert!(!dir.join(FILE).exists());
        assert!(problem.message().contains("已把原文件备份"));
        // 之后保存的新配置不会覆盖备份。
        Config::default().save(&dir).unwrap();
        assert_eq!(fs::read(&backup).unwrap(), broken);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn win_keys_json_and_custom() {
        use flow_core::keys::MacModifier;
        let s: Settings = serde_json::from_str(
            r#"{"win_keys":"custom","win_keys_custom":{"ctrl":"command","win":"option","alt":"control"}}"#,
        )
        .unwrap();
        assert_eq!(
            s.win_to_mac_keys(),
            WinToMacKeys {
                ctrl: MacModifier::Command,
                win: MacModifier::Option,
                alt: MacModifier::Control,
            }
        );
        // 选了预设时忽略自定义内容。
        let s = Settings {
            win_keys: WinKeysPreset::MacPosition,
            ..s
        };
        assert_eq!(s.win_to_mac_keys(), WinToMacKeys::MAC_POSITION);
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["win_keys"], "mac_position");
        assert_eq!(json["win_keys_custom"]["ctrl"], "command");
    }
}
