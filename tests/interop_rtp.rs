use anyhow::Result;
use rustrtc::rtp::{RtpHeader, RtpPacket};
use rustrtc::transports::rtp::UdpRtpEndpoint;
use tokio::net::UdpSocket;

#[tokio::test]
async fn test_rtp_interop_loopback() -> Result<()> {
    // 1. Setup two RTP endpoints on loopback
    let socket_a = UdpSocket::bind("127.0.0.1:0").await?;
    let socket_b = UdpSocket::bind("127.0.0.1:0").await?;
    let addr_a = socket_a.local_addr()?;
    let addr_b = socket_b.local_addr()?;

    // Connect them to each other (RTP only, no RTCP for this simple test)
    let endpoint_a = UdpRtpEndpoint::connect(socket_a, None, addr_b, None).await?;
    let endpoint_b = UdpRtpEndpoint::connect(socket_b, None, addr_a, None).await?;

    // 2. Send RTP packet from A to B
    let header = RtpHeader::new(96, 1001, 12345, 0x11223344);
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let packet = RtpPacket::new(header, payload.clone());

    endpoint_a.send_rtp(&packet).await?;

    // 3. Receive at B
    let received = match endpoint_b.recv().await? {
        rustrtc::transports::rtp::ReceivedPacket::Rtp(p) => p,
        _ => panic!("expected RTP"),
    };

    // 4. Verify
    assert_eq!(received.header.payload_type, 96);
    assert_eq!(received.header.sequence_number, 1001);
    assert_eq!(received.header.timestamp, 12345);
    assert_eq!(received.header.ssrc, 0x11223344);
    assert_eq!(received.payload, payload);

    println!("RTP Interop Test Passed: A -> B");
    Ok(())
}
