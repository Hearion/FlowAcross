//! 设备指纹与配对码。
//!
//! 双方建立 TLS 连接后，各自用"自己的证书指纹 + 对方的证书指纹"算出同一个 6 位配对码并显示。
//! 用户核对两边一致并分别确认后，互相保存对方指纹为可信设备。
//! 中间人要冒充任一方就必须换掉证书，指纹一变，两边显示的码就对不上。

use std::fmt;

use sha2::{Digest, Sha256};

use crate::layout::DeviceId;

pub type Fingerprint = [u8; 32];

const PAIRING_DOMAIN: &[u8] = b"flowacross-pairing-v1";

/// 设备证书（DER）的 SHA-256 指纹。证书在首次启动时生成并长期保存，指纹即设备身份。
pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    Sha256::digest(cert_der).into()
}

/// 从设备 ID（十六进制指纹）解析回指纹。
pub fn parse_device_id(id: &DeviceId) -> Option<Fingerprint> {
    let s = id.0.as_bytes();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(out)
}

/// 设备 ID 即指纹的小写十六进制。
pub fn device_id(fp: &Fingerprint) -> DeviceId {
    DeviceId(fp.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingCode(u32);

impl PairingCode {
    pub fn value(self) -> u32 {
        self.0
    }
}

impl fmt::Display for PairingCode {
    /// 显示为 "123 456"，方便口头核对。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:03} {:03}", self.0 / 1000, self.0 % 1000)
    }
}

/// 两端算出的配对码与参数顺序无关。
pub fn pairing_code(a: &Fingerprint, b: &Fingerprint) -> PairingCode {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut h = Sha256::new();
    h.update(PAIRING_DOMAIN);
    h.update(lo);
    h.update(hi);
    let digest = h.finalize();
    let n = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
    PairingCode((n % 1_000_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_symmetric_and_six_digits() {
        let a = fingerprint(b"device-a");
        let b = fingerprint(b"device-b");
        let c1 = pairing_code(&a, &b);
        assert_eq!(c1, pairing_code(&b, &a));
        assert!(c1.value() < 1_000_000);
        let s = c1.to_string();
        assert_eq!(s.len(), 7);
        assert_eq!(&s[3..4], " ");
    }

    #[test]
    fn different_peer_gives_different_code() {
        let a = fingerprint(b"device-a");
        let b = fingerprint(b"device-b");
        let mitm = fingerprint(b"attacker");
        assert_ne!(pairing_code(&a, &b), pairing_code(&a, &mitm));
    }

    #[test]
    fn device_id_is_hex_of_fingerprint() {
        let fp = fingerprint(b"x");
        let id = device_id(&fp);
        assert_eq!(id.0.len(), 64);
        assert!(id.0.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(parse_device_id(&id), Some(fp));
        assert_eq!(parse_device_id(&DeviceId("zz".into())), None);
    }
}
