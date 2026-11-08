//! QUIC 的 TLS 1.3 配置。
//!
//! 设备证书是自签名的，没有 CA 可以校验。这里在 TLS 层只验证"对方确实持有证书对应的私钥"
//! （握手签名），证书本身是否可信由应用层按指纹判断：已配对的指纹才建立会话，
//! 未配对的只允许走配对流程，并由用户核对双方显示的配对码。

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

/// 创建 QUIC 端点：同时作为服务端（接受连接）和客户端（发起连接）。
/// 必须在 tokio 运行时内调用。
pub fn bind_endpoint(id: &Identity, port: u16) -> Result<quinn::Endpoint, NetError> {
    let server = server_config(id)?;
    let bind = |p: u16| {
        quinn::Endpoint::server(
            server.clone(),
            std::net::SocketAddr::from(([0, 0, 0, 0], p)),
        )
    };
    let mut ep = match bind(port) {
        Ok(ep) => ep,
        Err(_) if port != 0 => bind(0)?,
        Err(e) => return Err(e.into()),
    };
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
