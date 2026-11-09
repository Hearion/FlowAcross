//! 设备指纹与配对码。
//!
//! 配对码由双方证书指纹、两边各出的随机数和 TLS 通道绑定值算出，带承诺（类似蓝牙数字比较）：
//!
//! 1. 发起方 I 选随机数 `nI`，先只发承诺 `H(fpI, nI)`（[`commitment`]）；
//! 2. 响应方 R 收到承诺后选随机数 `nR` 并发出；
//! 3. I 公开 `nI`，R 核对它与承诺一致（[`verify_commitment`]）；
//! 4. 两边各自算 [`sas_code`]，显示给用户核对。
//!
//! 为什么要承诺：只用两边指纹算码时，中间人可以反复换自己的证书（改序列号就换了指纹），
//! 离线枚举约一百万次就能让两边显示同一个码。有了承诺，每一边的码都取决于一个中间人
//! 出牌时还不知道的随机数，中间人每次尝试只有百万分之一的机会。通道绑定值让码只在这一条
//! TLS 连接上成立：中间人分别和两边建立的是两条连接，绑定值不同。
//!
//! 用户核对两边一致并分别确认后，互相保存对方指纹为可信设备。随机数由调用方生成后传入。

use std::fmt;

use sha2::{Digest, Sha256};

use crate::layout::DeviceId;

pub type Fingerprint = [u8; 32];
/// 配对随机数。
pub type Nonce = [u8; 32];

const COMMIT_DOMAIN: &[u8] = b"flowacross-pairing-commit-v2";
const SAS_DOMAIN: &[u8] = b"flowacross-pairing-v2";

/// 设备证书（DER）的 SHA-256 指纹。证书在首次启动时生成并长期保存，指纹即设备身份。
pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    Sha256::digest(cert_der).into()
}

/// 从设备 ID（十六进制指纹）解析回指纹。
pub fn parse_device_id(id: &DeviceId) -> Option<Fingerprint> {
    if !id.is_well_formed() {
        return None;
    }
    let s = id.0.as_bytes();
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

/// 发起方对随机数的承诺：在看到响应方的随机数之前就锁定自己的随机数。
pub fn commitment(initiator: &Fingerprint, nonce: &Nonce) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(COMMIT_DOMAIN);
    h.update(initiator);
    h.update(nonce);
    h.finalize().into()
}

/// 响应方核对发起方公开的随机数与之前收到的承诺一致。不一致说明有人在中间改了消息。
pub fn verify_commitment(initiator: &Fingerprint, nonce: &Nonce, commit: &[u8; 32]) -> bool {
    // 比较的是公开数据，不需要常数时间。
    &commitment(initiator, nonce) == commit
}

/// 配对码。参数按角色（发起方 / 响应方）传入，两边算出同一个值。
/// `binding` 是这条 TLS 连接的通道绑定值，两端相同。
pub fn sas_code(
    initiator: &Fingerprint,
    responder: &Fingerprint,
    initiator_nonce: &Nonce,
    responder_nonce: &Nonce,
    binding: &[u8; 32],
) -> PairingCode {
    let mut h = Sha256::new();
    h.update(SAS_DOMAIN);
    h.update(initiator);
    h.update(responder);
    h.update(initiator_nonce);
    h.update(responder_nonce);
    h.update(binding);
    let digest = h.finalize();
    let n = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
    PairingCode((n % 1_000_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps() -> (Fingerprint, Fingerprint) {
        (fingerprint(b"device-a"), fingerprint(b"device-b"))
    }

    #[test]
    fn both_sides_compute_the_same_six_digit_code() {
        let (a, b) = fps();
        let (na, nb, bind) = ([1u8; 32], [2u8; 32], [9u8; 32]);
        // 发起方 A、响应方 B 各自按角色计算。
        let at_a = sas_code(&a, &b, &na, &nb, &bind);
        let at_b = sas_code(&a, &b, &na, &nb, &bind);
        assert_eq!(at_a, at_b);
        assert!(at_a.value() < 1_000_000);
        let s = at_a.to_string();
        assert_eq!(s.len(), 7);
        assert_eq!(&s[3..4], " ");
    }

    #[test]
    fn code_depends_on_every_input() {
        let (a, b) = fps();
        let mitm = fingerprint(b"attacker");
        let (na, nb, bind) = ([1u8; 32], [2u8; 32], [9u8; 32]);
        let base = sas_code(&a, &b, &na, &nb, &bind);
        assert_ne!(base, sas_code(&a, &mitm, &na, &nb, &bind));
        assert_ne!(base, sas_code(&mitm, &b, &na, &nb, &bind));
        assert_ne!(base, sas_code(&a, &b, &[3; 32], &nb, &bind));
        assert_ne!(base, sas_code(&a, &b, &na, &[3; 32], &bind));
        assert_ne!(base, sas_code(&a, &b, &na, &nb, &[8; 32]));
        // 角色互换得到不同的码：两边必须对谁是发起方达成一致。
        assert_ne!(base, sas_code(&b, &a, &nb, &na, &bind));
    }

    #[test]
    fn commitment_binds_nonce_and_initiator() {
        let (a, b) = fps();
        let n = [7u8; 32];
        let c = commitment(&a, &n);
        assert!(verify_commitment(&a, &n, &c));
        // 公开时换了随机数，或换了发起方身份，都核对不过。
        assert!(!verify_commitment(&a, &[8; 32], &c));
        assert!(!verify_commitment(&b, &n, &c));
    }

    #[test]
    fn device_id_is_hex_of_fingerprint() {
        let fp = fingerprint(b"x");
        let id = device_id(&fp);
        assert_eq!(id.0.len(), 64);
        assert!(id.0.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(parse_device_id(&id), Some(fp));
        assert_eq!(parse_device_id(&DeviceId("zz".into())), None);
        // from_str_radix 接受 "+f"，必须先校验格式，否则同一指纹有多种写法。
        assert_eq!(parse_device_id(&DeviceId("+f".repeat(32))), None);
    }
}
