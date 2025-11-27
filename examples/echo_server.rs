use axum::{
    Router,
    extract::Json,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use rustrtc::PeerConnection;
use rustrtc::{RtcConfiguration, SdpType, SessionDescription};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(index))
        .route("/offer", post(offer))
        .nest_service("/static", ServeDir::new("examples/static"));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

#[derive(Deserialize)]
struct OfferRequest {
    sdp: String,
    #[allow(unused)]
    r#type: String,
}

#[derive(Serialize)]
struct OfferResponse {
    sdp: String,
    #[serde(rename = "type")]
    type_: String,
}

async fn offer(Json(payload): Json<OfferRequest>) -> impl IntoResponse {
    println!("Received offer");

    let config = RtcConfiguration::default();
    let pc = PeerConnection::new(config);

    // Create DataChannel (negotiated id=0)
    let dc = pc.create_data_channel("echo").await.unwrap();

    // Setup echo
    let pc_clone = pc.clone();
    let dc_clone = dc.clone();

    tokio::spawn(async move {
        while let Some(event) = dc_clone.recv().await {
            match event {
                rustrtc::transports::sctp::DataChannelEvent::Message(data) => {
                    println!("Received message: {:?}", String::from_utf8_lossy(&data));
                    let pc = pc_clone.clone();
                    tokio::spawn(async move {
                        // Echo back
                        if let Err(e) = pc.send_data(0, &data).await {
                            eprintln!("Failed to send data: {}", e);
                        } else {
                            println!("Sent echo");
                        }
                    });
                }
                rustrtc::transports::sctp::DataChannelEvent::Open => {
                    println!("Data channel opened");
                }
                rustrtc::transports::sctp::DataChannelEvent::Close => {
                    println!("Data channel closed");
                    break;
                }
            }
        }
    });

    // Handle SDP
    let offer_sdp = SessionDescription::parse(SdpType::Offer, &payload.sdp).unwrap();
    pc.set_remote_description(offer_sdp).await.unwrap();

    // Setup video echo
    let transceivers = pc.get_transceivers().await;
    for t in transceivers {
        if t.kind() == rustrtc::MediaKind::Video {
            t.set_direction(rustrtc::TransceiverDirection::SendRecv)
                .await;
            let receiver = t.receiver.lock().await.clone();
            if let Some(rx) = receiver {
                let track = rx.track();
                let sender =
                    std::sync::Arc::new(rustrtc::peer_connection::RtpSender::new(track, 55555));
                *t.sender.lock().await = Some(sender);
                println!("Added video echo");
            }
        }
    }

    // Create answer and wait for gathering
    let _ = pc.create_answer().await.unwrap();

    // Wait for gathering to complete
    loop {
        if pc.ice_transport().gather_state().await
            == rustrtc::transports::ice::IceGathererState::Complete
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let answer = pc.create_answer().await.unwrap();
    pc.set_local_description(answer.clone()).await.unwrap();

    Json(OfferResponse {
        sdp: answer.to_sdp_string(),
        type_: "answer".to_string(),
    })
}
