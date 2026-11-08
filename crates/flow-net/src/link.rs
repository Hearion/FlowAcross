//! 在 QUIC 连接的控制流上收发消息。
//!
//! 发起方打开双向流并先发消息（QUIC 在有数据写入前不会通知对方新流），接受方等待这条流。

use flow_core::layout::DeviceId;
use flow_core::pairing::{Fingerprint, device_id};
use flow_core::protocol::{FrameDecoder, Message, encode_frame};

use crate::NetError;
use crate::tls::peer_fingerprint;

/// 一条已认证的设备连接（尚未判断是否可信）。
pub struct Link {
    pub conn: quinn::Connection,
    pub peer_fingerprint: Fingerprint,
    pub peer_id: DeviceId,
    pub sender: LinkSender,
    pub receiver: LinkReceiver,
}

pub struct LinkSender {
    send: quinn::SendStream,
}

pub struct LinkReceiver {
    recv: quinn::RecvStream,
    decoder: FrameDecoder,
    buf: Vec<u8>,
}

impl Link {
    /// 发起方：打开控制流，并立即发送第一条消息（通常是 Hello）。
    pub async fn open(conn: quinn::Connection, first: &Message) -> Result<Link, NetError> {
        let fp = peer_fingerprint(&conn)?;
        let (send, recv) = conn.open_bi().await?;
        let mut sender = LinkSender { send };
        sender.send(first).await?;
        Ok(Link {
            peer_id: device_id(&fp),
            peer_fingerprint: fp,
            conn,
            sender,
            receiver: LinkReceiver::new(recv),
        })
    }

    /// 接受方：等待发起方打开的控制流。
    pub async fn accept(conn: quinn::Connection) -> Result<Link, NetError> {
        let fp = peer_fingerprint(&conn)?;
        let (send, recv) = conn.accept_bi().await?;
        Ok(Link {
            peer_id: device_id(&fp),
            peer_fingerprint: fp,
            conn,
            sender: LinkSender { send },
            receiver: LinkReceiver::new(recv),
        })
    }
}

impl LinkSender {
    pub async fn send(&mut self, msg: &Message) -> Result<(), NetError> {
        let frame = encode_frame(msg)?;
        self.send.write_all(&frame).await?;
        Ok(())
    }
}

impl LinkReceiver {
    fn new(recv: quinn::RecvStream) -> Self {
        Self {
            recv,
            decoder: FrameDecoder::default(),
            buf: vec![0u8; 16 * 1024],
        }
    }

    /// 读取下一条消息；对方正常关闭流时返回 `None`。
    pub async fn recv(&mut self) -> Result<Option<Message>, NetError> {
        loop {
            if let Some(msg) = self.decoder.next_message()? {
                return Ok(Some(msg));
            }
            match self.recv.read(&mut self.buf).await? {
                Some(n) => self.decoder.push(&self.buf[..n]),
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;
    use crate::tls::bind_endpoint;
    use flow_core::protocol::{Os, PROTOCOL_VERSION};

    fn hello(id: &Identity) -> Message {
        Message::Hello {
            protocol: PROTOCOL_VERSION,
            device: id.device_id.clone(),
            name: "test".into(),
            os: Os::current(),
            app_version: "0".into(),
            trusts_you: false,
        }
    }

    /// 两个端点在本机回环地址上建立双向认证的 QUIC 连接，互相看到对方的真实指纹并交换消息。
    #[tokio::test]
    async fn loopback_mutual_auth_and_messages() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let ep_a = bind_endpoint(&a, 0).unwrap();
        let ep_b = bind_endpoint(&b, 0).unwrap();
        let addr_b =
            std::net::SocketAddr::from(([127, 0, 0, 1], ep_b.local_addr().unwrap().port()));

        let b_id = b.clone();
        let server = tokio::spawn(async move {
            let conn = ep_b.accept().await.unwrap().await.unwrap();
            let mut link = Link::accept(conn).await.unwrap();
            let first = link.receiver.recv().await.unwrap().unwrap();
            link.sender.send(&hello(&b_id)).await.unwrap();
            let second = link.receiver.recv().await.unwrap().unwrap();
            (link.peer_id, first, second)
        });

        let conn = ep_a
            .connect(addr_b, "flowacross.local")
            .unwrap()
            .await
            .unwrap();
        let mut link = Link::open(conn, &hello(&a)).await.unwrap();
        assert_eq!(link.peer_id, b.device_id);
        let reply = link.receiver.recv().await.unwrap().unwrap();
        assert!(matches!(reply, Message::Hello { device, .. } if device == b.device_id));
        link.sender.send(&Message::PairRequest).await.unwrap();

        let (seen_by_b, first, second) = server.await.unwrap();
        assert_eq!(seen_by_b, a.device_id);
        assert!(matches!(first, Message::Hello { .. }));
        assert_eq!(second, Message::PairRequest);
    }
}
