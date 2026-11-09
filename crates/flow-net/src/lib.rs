//! 流界网络层。
//!
//! - [`identity`]：设备长期身份（自签名证书 + 私钥），指纹即设备 ID。
//! - [`tls`]：QUIC 的 TLS 配置。双向证书认证，证书不走 CA 校验，而是在应用层按指纹判断是否可信。
//! - [`link`]：在 QUIC 连接上收发 [`flow_core::protocol::Message`]。
//! - [`moves`]：用 QUIC 数据报收发鼠标位置（停顿后直接跳到最新位置）。
//! - [`discovery`]：mDNS 广播本机、发现局域网内的其他流界设备。

pub mod discovery;
pub mod identity;
pub mod link;
pub mod moves;
pub mod tls;

use thiserror::Error;

/// mDNS 服务类型。
pub const SERVICE_TYPE: &str = "_flowacross._udp.local.";
/// 默认监听端口；被占用时（例如同一台电脑开了第二个实例）改用随机端口。
pub const DEFAULT_PORT: u16 = 24830;
/// QUIC ALPN。
pub const ALPN: &[u8] = b"flowacross/1";

#[derive(Debug, Error)]
pub enum NetError {
    #[error("读写身份文件失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("生成设备证书失败：{0}")]
    Cert(#[from] rcgen::Error),
    #[error("TLS 配置失败：{0}")]
    Tls(#[from] rustls::Error),
    #[error("QUIC 连接失败：{0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("连接已断开：{0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("发送失败：{0}")]
    Write(#[from] quinn::WriteError),
    #[error("接收失败：{0}")]
    Read(#[from] quinn::ReadError),
    #[error("协议错误：{0}")]
    Protocol(#[from] flow_core::protocol::ProtocolError),
    #[error("对方没有提供设备证书")]
    NoPeerCertificate,
    #[error("mDNS 失败：{0}")]
    Mdns(#[from] mdns_sd::Error),
    #[error("{0}")]
    Other(String),
}

pub use identity::Identity;
pub use link::{Link, LinkReceiver, LinkSender};
pub use moves::{MoveReceiver, MoveSender};
pub use quinn;
pub use tls::{bind_endpoint, channel_binding, peer_fingerprint, random_bytes};
