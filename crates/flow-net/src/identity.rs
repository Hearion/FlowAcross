//! 设备长期身份：首次启动生成自签名证书和私钥，保存在应用数据目录。

use std::fs;
use std::path::Path;

use flow_core::layout::DeviceId;
use flow_core::pairing::{Fingerprint, device_id, fingerprint};

use crate::NetError;

const CERT_FILE: &str = "identity.cert.der";
const KEY_FILE: &str = "identity.key.der";

#[derive(Clone)]
pub struct Identity {
    /// 证书 DER。
    pub cert_der: Vec<u8>,
    /// PKCS#8 私钥 DER。
    pub key_der: Vec<u8>,
    pub fingerprint: Fingerprint,
    pub device_id: DeviceId,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印私钥。
        f.debug_struct("Identity")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn from_der(cert_der: Vec<u8>, key_der: Vec<u8>) -> Self {
        let fp = fingerprint(&cert_der);
        Self {
            cert_der,
            key_der,
            fingerprint: fp,
            device_id: device_id(&fp),
        }
    }

    pub fn generate() -> Result<Self, NetError> {
        let ck = rcgen::generate_simple_self_signed(vec!["flowacross.local".to_owned()])?;
        Ok(Self::from_der(
            ck.cert.der().to_vec(),
            ck.signing_key.serialize_der(),
        ))
    }

    /// 从目录读取身份；不存在则生成并保存。
    pub fn load_or_create(dir: &Path) -> Result<Self, NetError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        if let (Ok(cert), Ok(key)) = (fs::read(&cert_path), fs::read(&key_path)) {
            return Ok(Self::from_der(cert, key));
        }
        fs::create_dir_all(dir)?;
        let id = Self::generate()?;
        fs::write(&cert_path, &id.cert_der)?;
        write_private(&key_path, &id.key_der)?;
        Ok(id)
    }
}

/// 私钥文件只允许当前用户读写。
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        fs::write(path, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_reloads_same_identity() {
        let dir = std::env::temp_dir().join(format!("flowacross-id-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.device_id, b.device_id);
        assert_eq!(a.device_id.0.len(), 64);
        fs::remove_dir_all(&dir).unwrap();
    }
}
