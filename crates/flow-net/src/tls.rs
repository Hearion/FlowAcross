//! QUIC 的 TLS 1.3 配置。
//!
//! 设备证书是自签名的，没有 CA 可以校验。这里在 TLS 层只验证"对方确实持有证书对应的私钥"
//! （握手签名），证书本身是否可信由应用层按指纹判断：已配对的指纹才建立会话，
//! 未配对的只允许走配对流程，并由用户核对双方显示的配对码。

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use flow_core::pairing::{Fingerprint, fingerprint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use socket2::{Domain, Protocol, Socket, Type};

use crate::{ALPN, Identity, NetError};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// 接受任意证书，但校验握手签名。
#[derive(Debug)]
struct PeerCertVerifier {
    algs: WebPkiSupportedAlgorithms,
}

impl PeerCertVerifier {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            algs: provider().signature_verification_algorithms,
        })
    }
}

impl ServerCertVerifier for PeerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

impl ClientCertVerifier for PeerCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

fn cert_chain(id: &Identity) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    (
        vec![CertificateDer::from(id.cert_der.clone())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key_der.clone())),
    )
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    // 断网后尽快发现：2 秒保活，8 秒无响应视为断开。
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(
        Duration::from_secs(8).try_into().expect("8s 在范围内"),
    ));
    // 数据报只用来发鼠标位置，过时的没有意义：发送缓冲放小，网络卡住时 quinn 会丢掉最旧的。
    t.datagram_send_buffer_size(16 * 1024);
    Arc::new(t)
}

pub fn server_config(id: &Identity) -> Result<quinn::ServerConfig, NetError> {
    let (chain, key) = cert_chain(id);
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(PeerCertVerifier::new())
        .with_single_cert(chain, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|e| NetError::Other(format!("QUIC 服务端配置失败：{e}")))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(transport());
    Ok(cfg)
}

pub fn client_config(id: &Identity) -> Result<quinn::ClientConfig, NetError> {
    let (chain, key) = cert_chain(id);
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(PeerCertVerifier::new())
        .with_client_auth_cert(chain, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| NetError::Other(format!("QUIC 客户端配置失败：{e}")))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(transport());
    Ok(cfg)
}

fn bind_socket(port: u16) -> std::io::Result<UdpSocket> {
    // Windows 的 IPv6 socket 默认只接受 IPv6，必须显式开启双栈。
    let dual_stack = || -> std::io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_only_v6(false)?;
        socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
        Ok(socket.into())
    };
    dual_stack().or_else(|e| {
        // 已被占用时交给调用方换端口，避免 Windows 在同一端口另绑一个 IPv4 socket。
        if e.kind() == std::io::ErrorKind::AddrInUse {
            return Err(e);
        }
        tracing::debug!(error = %e, port, "无法绑定双栈 UDP，回退到 IPv4");
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
    })
}

/// 创建 QUIC 端点：同时作为服务端（接受连接）和客户端（发起连接）。
/// 优先支持 IPv4/IPv6 双栈；系统禁用 IPv6 时回退到 IPv4。
/// 必须在 tokio 运行时内调用。
pub fn bind_endpoint(id: &Identity, port: u16) -> Result<quinn::Endpoint, NetError> {
    let server = server_config(id)?;
    let socket = match bind_socket(port) {
        Ok(socket) => socket,
        Err(_) if port != 0 => bind_socket(0)?,
        Err(e) => return Err(e.into()),
    };
    let mut ep = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server),
        socket,
        Arc::new(quinn::TokioRuntime),
    )?;
    ep.set_default_client_config(client_config(id)?);
    Ok(ep)
}

/// 已建立连接的对端证书指纹。
pub fn peer_fingerprint(conn: &quinn::Connection) -> Result<Fingerprint, NetError> {
    let certs = conn
        .peer_identity()
        .and_then(|any| any.downcast::<Vec<CertificateDer<'static>>>().ok())
        .ok_or(NetError::NoPeerCertificate)?;
    let leaf = certs.first().ok_or(NetError::NoPeerCertificate)?;
    Ok(fingerprint(leaf.as_ref()))
}

/// 密码学安全的随机字节（配对随机数用）。
pub fn random_bytes<const N: usize>() -> Result<[u8; N], NetError> {
    let mut out = [0u8; N];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut out)
        .map_err(|_| NetError::Other("系统随机数生成失败".into()))?;
    Ok(out)
}

/// TLS 通道绑定值（RFC 5705 exporter）：同一条连接的两端相同，换一条连接（比如中间人分别和两边
/// 建立的连接）就不同。混进配对码，让配对码只在这一条连接上成立。
pub fn channel_binding(conn: &quinn::Connection) -> Result<[u8; 32], NetError> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, b"EXPORTER-flowacross-pairing-v2", b"")
        .map_err(|_| NetError::Other("无法从加密连接导出配对密钥".into()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    #[test]
    fn random_bytes_differ() {
        let a: [u8; 32] = random_bytes().unwrap();
        let b: [u8; 32] = random_bytes().unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn dual_stack_connects_to_ipv4_and_ipv6_only_peers_in_both_directions() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let ep_a = bind_endpoint(&a, 0).unwrap();
        assert!(ep_a.local_addr().unwrap().is_ipv6(), "测试机需启用 IPv6");
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            // 对端故意只监听一种协议，确保双栈在 Windows 上双向兼容。
            let mut ep_b =
                quinn::Endpoint::server(server_config(&b).unwrap(), SocketAddr::new(ip, 0))
                    .unwrap();
            ep_b.set_default_client_config(client_config(&b).unwrap());
            for (client, server) in [(&ep_a, &ep_b), (&ep_b, &ep_a)] {
                let addr = SocketAddr::new(ip, server.local_addr().unwrap().port());
                let connecting = client.connect(addr, "flowacross.local").unwrap();
                let (outgoing, incoming) = tokio::time::timeout(Duration::from_secs(3), async {
                    tokio::join!(connecting, async { server.accept().await.unwrap().await })
                })
                .await
                .expect("QUIC 回环握手超时");
                let (outgoing, incoming) = (outgoing.unwrap(), incoming.unwrap());
                assert_ne!(
                    peer_fingerprint(&outgoing).unwrap(),
                    peer_fingerprint(&incoming).unwrap()
                );
                outgoing.close(0u32.into(), b"test complete");
            }
        }
    }

    #[tokio::test]
    async fn occupied_port_falls_back_to_another_port() {
        let id = Identity::generate().unwrap();
        let a = bind_endpoint(&id, 0).unwrap();
        let port = a.local_addr().unwrap().port();
        let b = bind_endpoint(&id, port).unwrap();
        assert_ne!(b.local_addr().unwrap().port(), port);
    }

    /// 同一条连接两端导出的通道绑定值相同，不同连接不同。
    #[tokio::test]
    async fn channel_binding_matches_on_both_ends_only() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let ep_a = bind_endpoint(&a, 0).unwrap();
        let ep_b = bind_endpoint(&b, 0).unwrap();
        let addr_b =
            std::net::SocketAddr::from(([127, 0, 0, 1], ep_b.local_addr().unwrap().port()));
        let server = tokio::spawn(async move {
            let c1 = ep_b.accept().await.unwrap().await.unwrap();
            let c2 = ep_b.accept().await.unwrap().await.unwrap();
            (channel_binding(&c1).unwrap(), channel_binding(&c2).unwrap())
        });
        let c1 = ep_a
            .connect(addr_b, "flowacross.local")
            .unwrap()
            .await
            .unwrap();
        let c2 = ep_a
            .connect(addr_b, "flowacross.local")
            .unwrap()
            .await
            .unwrap();
        let (s1, s2) = server.await.unwrap();
        assert_eq!(channel_binding(&c1).unwrap(), s1);
        assert_eq!(channel_binding(&c2).unwrap(), s2);
        assert_ne!(s1, s2);
    }
}
