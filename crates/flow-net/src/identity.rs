//! 设备长期身份：首次启动生成自签名证书和私钥，保存在应用数据目录。

use std::fs;
use std::io::ErrorKind;
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
    ///
    /// 只有文件确实不存在时才生成新身份。文件在但读不出来（权限问题、被安全软件占用）时返回错误：
    /// 重新生成会换掉设备 ID，所有配对随之失效，而且没有任何提示。
    pub fn load_or_create(dir: &Path) -> Result<Self, NetError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        let read = |path: &Path| match fs::read(path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(std::io::Error::new(
                e.kind(),
                format!(
                    "无法读取设备身份文件 {}（{e}）。为了不丢失已配对的设备，没有重新生成身份；\
                     请检查这个文件的权限、是否被安全软件拦截，然后重新打开流界。",
                    path.display()
                ),
            )),
        };
        if let (Some(cert), Some(key)) = (read(&cert_path)?, read(&key_path)?) {
            tighten_private(&key_path);
            return Ok(Self::from_der(cert, key));
        }
        fs::create_dir_all(dir)?;
        let id = Self::generate()?;
        fs::write(&cert_path, &id.cert_der)?;
        write_private(&key_path, &id.key_der)?;
        Ok(id)
    }
}

/// 已有的私钥文件权限比 0600 宽时收紧（`write_private` 的权限只在新建文件时生效）。
fn tighten_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path)
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// 私钥文件只允许当前用户读写（Windows 上依靠用户数据目录本身的访问控制）。
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

    #[cfg(unix)]
    #[test]
    fn unreadable_identity_is_an_error_not_a_new_identity() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("flowacross-id-perm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let a = Identity::load_or_create(&dir).unwrap();
        let key = dir.join(KEY_FILE);
        // 私钥权限过宽时，下次读取会收紧。
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        Identity::load_or_create(&dir).unwrap();
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // 证书读不出来（不是不存在）：报错，并且不动原来的文件。
        let cert = dir.join(CERT_FILE);
        fs::set_permissions(&cert, fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = fs::read(&cert).is_err(); // root 运行时权限不起作用，跳过断言
        let result = Identity::load_or_create(&dir);
        fs::set_permissions(&cert, fs::Permissions::from_mode(0o644)).unwrap();
        if unreadable {
            assert!(result.is_err());
        }
        assert_eq!(
            Identity::load_or_create(&dir).unwrap().device_id,
            a.device_id
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
