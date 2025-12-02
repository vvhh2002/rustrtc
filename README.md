# rustrtc

A high-performance implementation of WebRTC. 

## Features

- **🚀High performance:** ~64% faster than `pion` (go version).
- **🍡WebRTC Compliant**: Full compliance with webrtc/chrome.
- **📺Media Support**: RTP/SRTP handling for audio and video.
- **👌ICE/STUN**: Interactive Connectivity Establishment and STUN protocol support.

## Benchmark game (rustrtc vs webrtc-rs & pion)

**CPU:**  `AMD Ryzen 7 5700X 8-Core Processor`   
**OS** `5.15.0-118-generic #128-Ubuntu`  
**Compiler** `rustc 1.91.0 (f8297e351 2025-10-28)`,  `go version go1.23.0 linux/amd64`   

```shell
nice@miuda.ai rustrtc % cargo run -r --example benchmark

Comparison (Baseline: webrtc)
Metric               | webrtc     | rustrtc    | pion      
--------------------------------------------------------------------------------
Duration (s)         | 10.22      | 10.00      | 10.61     
Setup Latency (ms)   | 0.57       | 0.13       | 0.50      
Throughput (MB/s)    | 257.92     | 493.28     | 299.89    
Msg Rate (msg/s)     | 264113.80  | 505122.00  | 307082.19 
CPU Usage (%)        | 1555.80    | 1331.10    | 1157.17   
Memory (MB)          | 30.00      | 10.00      | 47.00     
--------------------------------------------------------------------------------

Performance Charts
==================

Throughput (MB/s) (Higher is better)
webrtc     | ████████████████████                     257.92
rustrtc    | ████████████████████████████████████████ 493.28
pion       | ████████████████████████                 299.89

Message Rate (msg/s) (Higher is better)
webrtc     | ████████████████████                     264113.80
rustrtc    | ████████████████████████████████████████ 505122.00
pion       | ████████████████████████                 307082.19

Setup Latency (ms) (Lower is better)
webrtc     | ████████████████████████████████████████ 0.57
rustrtc    | █████████                                0.13
pion       | ███████████████████████████████████      0.50

CPU Usage (%) (Lower is better)
webrtc     | ████████████████████████████████████████ 1555.80
rustrtc    | ██████████████████████████████████       1331.10
pion       | █████████████████████████████            1157.17

Memory (MB) (Lower is better)
webrtc     | █████████████████████████                30.00
rustrtc    | ████████                                 10.00
pion       | ████████████████████████████████████████ 47.00
```

**Key Findings:**
- **Throughput**: `rustrtc` is ~91% faster than `webrtc-rs` and ~64% faster than `pion`.
- **Memory**: `rustrtc` uses ~67% less memory than `webrtc-rs` and ~79% less than `pion`.
- **Setup Latency**: Significantly faster connection setup (0.13ms vs 0.57ms/0.50ms).


## Usage

Here is a simple example of how to create a `PeerConnection` and handle an offer:

```rust
use rustrtc::{PeerConnection, RtcConfiguration, SessionDescription, SdpType};

#[tokio::main]
async fn main() {
    let config = RtcConfiguration::default();
    let pc = PeerConnection::new(config);

    // Create a Data Channel
    let dc = pc.create_data_channel("data", None).unwrap();

    // Handle received messages
    let dc_clone = dc.clone();
    tokio::spawn(async move {
        while let Some(event) = dc_clone.recv().await {
            if let rustrtc::DataChannelEvent::Message(data) = event {
                println!("Received: {:?}", String::from_utf8_lossy(&data));
            }
        }
    });

    // Create an offer
    let offer = pc.create_offer().await.unwrap();
    pc.set_local_description(offer).unwrap();

    // Wait for ICE gathering to complete
    pc.wait_for_gathering_complete().await;

    // Get the complete SDP with candidates
    let complete_offer = pc.local_description().unwrap();
    println!("Offer SDP: {}", complete_offer.to_sdp_string());
}
```

## Configuration

`rustrtc` allows customizing the WebRTC session via `RtcConfiguration`:

- **ice_servers**: Configure STUN/TURN servers.
- **ice_transport_policy**: Control ICE candidate gathering (e.g., `All`, `Relay`).
- **ssrc_start**: Set the starting SSRC value for local tracks.
- **media_capabilities**: Configure supported codecs (payload types, names) and SCTP ports.

```rust
use rustrtc::{PeerConnection, RtcConfiguration, IceServer, IceTransportPolicy, config::MediaCapabilities};

let mut config = RtcConfiguration::default();

// Configure ICE servers
config.ice_servers.push(IceServer::new(vec!["stun:stun.l.google.com:19302"]));

// Set ICE transport policy (optional)
config.ice_transport_policy = IceTransportPolicy::All;

config.ssrc_start = 10000;

// Customize media capabilities
let mut caps = MediaCapabilities::default();
// ... configure audio/video/application caps ...
config.media_capabilities = Some(caps);

let pc = PeerConnection::new(config);
```

## Examples

You can run the examples provided in the repository.

### Echo Server

The echo server example demonstrates how to accept a WebRTC connection, receive data on a data channel, and echo it back. It also supports video playback if an IVF file is provided.

1. Run the server:
    ```bash
    cargo run --example echo_server
    ```

2. Open your browser and navigate to `http://127.0.0.1:3000`.

### DataChannel Chat

A multi-user chat room using WebRTC DataChannels.

1. Run the server:
    ```bash
    cargo run --example datachannel_chat
    ```

2. Open your browser and navigate to `http://127.0.0.1:3000`. Open multiple tabs to chat between them.

### Audio Saver

Records audio from the browser's microphone and saves it to a file (`output.ulaw`) on the server.

1. Run the server:
    ```bash
    cargo run --example audio_saver
    ```

2. Open your browser and navigate to `http://127.0.0.1:3000`. Click "Start" to begin recording.

### RTP Play (FFmpeg)

Streams a video file (`examples/static/output.ivf`) via RTP to a UDP port, which can be played back using `ffplay`.

1. Run the server:
    ```bash
    cargo run --example rtp_play
    ```

2. In a separate terminal, run `ffplay` (requires ffmpeg installed):
    ```bash
    ffplay -protocol_whitelist file,udp,rtp -i examples/rtp_play.sdp
    ```

## License

This project is licensed under the MIT License.
