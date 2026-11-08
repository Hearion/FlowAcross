//! 本地配置：设备名称、设置、已配对设备及其在布局中的位置。保存在应用数据目录的 config.json。

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
    /// 暂停共享（键鼠不再流转到其他设备，连接保持）。
    pub sharing_paused: bool,
    /// 启动时和之后每隔几小时检查一次新版本（只提示，不自动安装）。
    pub auto_check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hold_key: HoldKey::None,
            dwell_ms: 0,
            corner_guard: true,
            mapping: EdgeMapping::Continuous,
            clipboard: true,
            sharing_paused: false,
            auto_check_updates: true,
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

impl Config {
    pub fn load(dir: &Path) -> Config {
        fs::read(dir.join(FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
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
        assert_eq!(Config::load(&dir), c);

        // 旧版本配置缺字段时用默认值补齐
        fs::write(dir.join(FILE), br#"{"onboarded":true}"#).unwrap();
        let old = Config::load(&dir);
        assert!(old.onboarded && old.settings.clipboard && old.settings.auto_check_updates);
        fs::remove_dir_all(&dir).unwrap();
    }
}
