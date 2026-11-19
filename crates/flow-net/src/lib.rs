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

/// 本机现在能用来连局域网的地址：不含回环、自动分配的 169.254.x.x 和 IPv6 链路本地地址
/// （后两种在没连上任何网络时也会有）。连接失败时用来判断"本机有没有网"和"是不是同一个网段"。
/// 查不到时返回空列表。
pub fn local_networks() -> Vec<flow_core::diagnose::LocalNet> {
    use flow_core::diagnose::LocalNet;
    use if_addrs::IfAddr;
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| lan_interface(i.is_loopback(), i.is_p2p(), &i.oper_status, cfg!(windows)))
        .filter_map(|i| match i.addr {
            IfAddr::V4(a) if !a.ip.is_link_local() => Some(LocalNet {
                addr: a.ip.into(),
                prefix: a.prefixlen,
            }),
            // fe80::/10 是链路本地地址。
            IfAddr::V6(a) if a.ip.segments()[0] & 0xffc0 != 0xfe80 => Some(LocalNet {
                addr: a.ip.into(),
                prefix: a.prefixlen,
            }),
            _ => None,
        })
        .collect()
}

/// 这块网卡算不算"连着局域网"。回环和点对点的（VPN、代理软件的虚拟网卡 utun / tun）不算：
/// 它们的地址和局域网没关系，算进去的话关掉 Wi‑Fi 后还会被当成"有网"，诊断里显示的本机地址
/// 也会是 VPN 的。
///
/// 接通状态：Windows 上明确报"没接通"的排除，其余当作可用；其他系统上 if-addrs 只会给
/// "在运行"或"不知道"，从不给"没接通"，所以只收"在运行"的（没插网线的网卡、没启动的
/// docker0 是"不知道"）。
fn lan_interface(
    loopback: bool,
    point_to_point: bool,
    status: &if_addrs::IfOperStatus,
    windows: bool,
) -> bool {
    use if_addrs::IfOperStatus;
    if loopback || point_to_point {
        return false;
    }
    if windows {
        *status != IfOperStatus::Down
    } else {
        *status == IfOperStatus::Up
    }
}

pub use identity::Identity;
pub use link::{Link, LinkReceiver, LinkSender};
pub use moves::{MoveReceiver, MoveSender};
pub use quinn;
pub use tls::{bind_endpoint, channel_binding, peer_fingerprint, random_bytes};

#[cfg(test)]
mod tests {
    use super::lan_interface;
    use if_addrs::IfOperStatus::{Down, Unknown, Up};

    #[test]
    fn only_running_non_tunnel_interfaces_count_as_lan() {
        // macOS / Linux：在运行的普通网卡才算。
        assert!(lan_interface(false, false, &Up, false));
        assert!(!lan_interface(false, false, &Unknown, false));
        // VPN 的点对点网卡、回环不算。
        assert!(!lan_interface(false, true, &Up, false));
        assert!(!lan_interface(true, false, &Up, false));
        // Windows：状态可靠，只排除明确没接通的。
        assert!(lan_interface(false, false, &Unknown, true));
        assert!(!lan_interface(false, false, &Down, true));
        assert!(!lan_interface(false, true, &Up, true));
    }

    /// 查询本身不出错，结果里没有回环和自动分配的地址（有没有网、有几块网卡随机器而定）。
    #[test]
    fn local_networks_never_lists_loopback_or_link_local() {
        for net in super::local_networks() {
            assert!(!net.addr.is_loopback(), "{net:?}");
            if let std::net::IpAddr::V4(v4) = net.addr {
                assert!(!v4.is_link_local(), "{net:?}");
            }
        }
    }
}
