pub mod dtls;
pub mod ice;
pub mod rtp;
pub mod sctp;

use async_trait::async_trait;
use bytes::Bytes;
use std::net::SocketAddr;

#[async_trait]
pub trait PacketReceiver: Send + Sync {
    async fn receive(&self, packet: Bytes, addr: SocketAddr);
}
