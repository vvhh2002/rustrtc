pub mod handshake;
#[cfg(test)]
mod interop_tests;
pub mod record;
#[cfg(test)]
mod tests;

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes128Gcm, Nonce,
};
use anyhow::Result;
use bytes::{Buf, Bytes, BytesMut};
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Verifier; // Add this
use p256::ecdsa::{signature::Signer, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use p256::{ecdh::EphemeralSecret, elliptic_curve::sec1::ToEncodedPoint, PublicKey};
use rand_core::OsRng;
use rcgen::generate_simple_self_signed;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use self::handshake::{
    CertificateMessage, ClientHello, ClientKeyExchange, Finished, HandshakeMessage, HandshakeType,
    HelloVerifyRequest, Random, ServerHello, ServerHelloDone, ServerKeyExchange,
};
use self::record::{ContentType, DtlsRecord, ProtocolVersion};
use crate::transports::ice::conn::IceConn;

pub fn generate_certificate() -> Result<Certificate> {
    let cert = generate_simple_self_signed(vec!["localhost".to_string()])?;
    Ok(Certificate {
        certificate: vec![cert.cert.der().to_vec()],
        private_key: cert.signing_key.serialize_pem(),
    })
}

pub fn fingerprint(cert: &Certificate) -> String {
    let mut hasher = Sha256::new();
    hasher.update(&cert.certificate[0]);
    let result = hasher.finalize();
    result
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<String>>()
        .join(":")
}

#[derive(Clone, Debug)]
pub struct Certificate {
    pub certificate: Vec<Vec<u8>>,
    pub private_key: String, // PEM encoded key
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SessionKeys {
    pub client_write_key: Vec<u8>,
    pub server_write_key: Vec<u8>,
    pub client_write_iv: Vec<u8>,
    pub server_write_iv: Vec<u8>,
    pub master_secret: Vec<u8>,
    pub client_random: Vec<u8>,
    pub server_random: Vec<u8>,
}

pub struct DtlsTransport {
    conn: Arc<IceConn>,
    state: Arc<Mutex<DtlsState>>,
    incoming_data_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    outgoing_data_tx: mpsc::Sender<Vec<u8>>,
    // Internal channel to feed the handshake loop from PacketReceiver
    handshake_rx_feeder: mpsc::Sender<Vec<u8>>,
}

#[derive(Debug)]
#[allow(dead_code)]
enum DtlsState {
    New,
    Handshaking,
    Connected(SessionKeys),
    Failed,
}

impl DtlsTransport {
    pub async fn new(
        conn: Arc<IceConn>,
        certificate: Certificate,
        is_client: bool,
    ) -> Result<Arc<Self>> {
        let (incoming_data_tx, incoming_data_rx) = mpsc::channel(100);
        let (outgoing_data_tx, outgoing_data_rx) = mpsc::channel(100);
        let (handshake_rx_feeder, handshake_rx) = mpsc::channel(100);

        let transport = Arc::new(Self {
            conn: conn.clone(),
            state: Arc::new(Mutex::new(DtlsState::New)),
            incoming_data_rx: Arc::new(Mutex::new(incoming_data_rx)),
            outgoing_data_tx,
            handshake_rx_feeder,
        });

        // Register with IceConn
        conn.set_dtls_receiver(transport.clone()).await;

        let transport_clone = transport.clone();
        tokio::spawn(async move {
            if let Err(e) = transport_clone
                .handshake(
                    certificate,
                    is_client,
                    incoming_data_tx,
                    outgoing_data_rx,
                    handshake_rx,
                )
                .await
            {
                eprintln!("DTLS handshake failed: {}", e);
                *transport_clone.state.lock().await = DtlsState::Failed;
            }
            // Connected state is set inside handshake now
        });

        Ok(transport)
    }

    pub async fn send(&self, data: &[u8]) -> Result<()> {
        self.outgoing_data_tx
            .send(data.to_vec())
            .await
            .map_err(|_| anyhow::anyhow!("Send failed"))
    }

    pub async fn recv(&self) -> Result<Vec<u8>> {
        let mut rx = self.incoming_data_rx.lock().await;
        rx.recv().await.ok_or(anyhow::anyhow!("Channel closed"))
    }

    pub async fn export_keying_material(&self, label: &str, len: usize) -> Result<Vec<u8>> {
        let state = self.state.lock().await;
        if let DtlsState::Connected(keys) = &*state {
            let seed = [keys.client_random.as_slice(), keys.server_random.as_slice()].concat();
            prf_sha256(&keys.master_secret, label.as_bytes(), &seed, len)
        } else {
            Err(anyhow::anyhow!("DTLS not connected"))
        }
    }

    async fn handshake(
        &self,
        certificate: Certificate,
        is_client: bool,
        incoming_data_tx: mpsc::Sender<Vec<u8>>,
        mut outgoing_data_rx: mpsc::Receiver<Vec<u8>>,
        mut handshake_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Result<()> {
        *self.state.lock().await = DtlsState::Handshaking;

        let mut sequence_number = 0;
        let mut epoch = 0;
        let mut message_seq = 0;

        // Retransmission state
        let mut last_flight_buffer: Option<Vec<u8>> = None;
        let mut retransmit_interval = tokio::time::interval(std::time::Duration::from_secs(1));
        retransmit_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Generate ephemeral key for ECDHE
        let local_secret = EphemeralSecret::random(&mut OsRng);
        let local_public = local_secret.public_key();
        let local_public_key_bytes = local_public.to_encoded_point(false).as_bytes().to_vec();

        // We need to keep local_secret until we have the peer's public key
        let mut local_secret_opt = Some(local_secret);
        let mut peer_public_key: Option<Vec<u8>> = None;

        let mut client_random: Option<Vec<u8>> = None;
        let mut server_random: Option<Vec<u8>> = None;
        let mut session_keys: Option<SessionKeys> = None;
        let mut handshake_messages = Vec::new();

        if is_client {
            // Send ClientHello
            println!("Sending ClientHello");
            let random = Random::new();
            client_random = Some(random.to_bytes());

            let mut extensions = Vec::new();

            // Supported Elliptic Curves (secp256r1)
            extensions.extend_from_slice(&[0x00, 0x0a]); // Type
            extensions.extend_from_slice(&[0x00, 0x04]); // Length
            extensions.extend_from_slice(&[0x00, 0x02]); // List Length
            extensions.extend_from_slice(&[0x00, 0x17]); // secp256r1

            // Supported Point Formats (uncompressed)
            extensions.extend_from_slice(&[0x00, 0x0b]); // Type
            extensions.extend_from_slice(&[0x00, 0x02]); // Length
            extensions.extend_from_slice(&[0x01]); // List Length
            extensions.extend_from_slice(&[0x00]); // uncompressed

            let client_hello = ClientHello {
                version: ProtocolVersion::DTLS_1_2,
                random,
                session_id: vec![],
                cookie: vec![],
                cipher_suites: vec![0xC02B], // TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
                compression_methods: vec![0], // Null
                extensions,
            };

            let mut body = BytesMut::new();
            client_hello.encode(&mut body);

            let handshake_msg = HandshakeMessage {
                msg_type: HandshakeType::ClientHello,
                message_seq,
                fragment_offset: 0,
                fragment_length: body.len() as u32,
                body: body.freeze(),
            };

            let mut buf = BytesMut::new();
            handshake_msg.encode(&mut buf);
            handshake_messages.extend_from_slice(&buf);

            let buf = self
                .send_handshake_message(handshake_msg, epoch, &mut sequence_number, None, is_client)
                .await?;
            last_flight_buffer = Some(buf);
            message_seq += 1;
        }

        loop {
            tokio::select! {
                _ = retransmit_interval.tick() => {
                    if let Some(buf) = &last_flight_buffer {
                        if is_client && message_seq == 1 {
                            println!("Retransmitting ClientHello");
                            if let Err(e) = self.conn.send(buf).await {
                                eprintln!("Retransmission failed: {}", e);
                            }
                        }
                    }
                }
                Some(packet) = handshake_rx.recv() => {
                            let mut data = Bytes::from(packet);

                            while !data.is_empty() {
                                match DtlsRecord::decode(&mut data) {
                                    Ok(None) => break,
                                    Ok(Some(record)) => {
                                        let mut payload = record.payload;

                                        if record.epoch > 0 {
                                            if let Some(keys) = &session_keys {
                                                let (key, iv) = if is_client {
                                                    (&keys.server_write_key, &keys.server_write_iv)
                                                } else {
                                                    (&keys.client_write_key, &keys.client_write_iv)
                                                };

                                                // Sequence number for AAD is epoch (16) + seq (48)
                                                let full_seq = ((record.epoch as u64) << 48) | record.sequence_number;

                                                match decrypt_record(
                                                    record.content_type,
                                                    record.version,
                                                    full_seq,
                                                    &payload,
                                                    key,
                                                    iv
                                                ) {
                                                    Ok(p) => payload = Bytes::from(p),
                                                    Err(e) => {
                                                        eprintln!("Decryption failed: {}", e);
                                                        break;
                                                    }
                                                }
                                            } else {
                                                eprintln!("Received encrypted record but no keys available");
                                                break;
                                            }
                                        }

                                        match record.content_type {
                                            ContentType::ChangeCipherSpec => {
                                                println!("Received ChangeCipherSpec");
                                                // TODO: Switch to encrypted mode
                                            },
                                            ContentType::ApplicationData => {
                                                if let Err(e) = incoming_data_tx.send(payload.to_vec()).await {
                                                    eprintln!("Failed to send incoming data to channel: {}", e);
                                                }
                                            },
                                            ContentType::Handshake => {
                                                let mut body = payload;
                                                while !body.is_empty() {
                                                    let msg_buf = body.clone();
                                                    match HandshakeMessage::decode(&mut body) {
                                                        Ok(None) => break,
                                                        Ok(Some(msg)) => {
                                                            let consumed = msg_buf.len() - body.len();
                                                            let raw_msg = msg_buf.slice(0..consumed);

                                                            println!("Received handshake message: {:?} seq={} frag_off={} frag_len={}",
                                                                msg.msg_type, msg.message_seq, msg.fragment_offset, msg.fragment_length);

                                                            if msg.msg_type != HandshakeType::Finished
                                                                && msg.msg_type != HandshakeType::HelloRequest
                                                                && msg.msg_type != HandshakeType::HelloVerifyRequest
                                                            {
                                                                handshake_messages.extend_from_slice(&raw_msg);
                                                            }

                                                            match msg.msg_type {
                                                                HandshakeType::ClientHello => {
                                                                    if !is_client {
                                                                        println!("Received ClientHello");
                                                                        let mut body = msg.body.clone();
                                                                        match ClientHello::decode(&mut body) {
                                                                            Ok(client_hello) => {
                                                                            println!("ClientHello Version: {:?} ({}, {})", client_hello.version, client_hello.version.major, client_hello.version.minor);
                                                                            client_random = Some(
                                                                                client_hello.random.to_bytes(),
                                                                            );

                                                                            // Send ServerHello
                                                                            println!("Sending ServerHello");

                                                                            // Parse ClientHello extensions to debug
                                                                            let mut srtp_profiles = Vec::new();
                                                                            let mut ext_buf = Bytes::from(client_hello.extensions.clone());
                                                                            while ext_buf.len() >= 4 {
                                                                                let ext_type = ext_buf.get_u16();
                                                                                let ext_len = ext_buf.get_u16() as usize;
                                                                                if ext_buf.len() < ext_len {
                                                                                    break;
                                                                                }
                                                                                let _ext_data = ext_buf.split_to(ext_len);
                                                                                println!("Client Extension: Type={}", ext_type);
                                                                                if ext_type == 13 { // signature_algorithms
                                                                                    println!("  Signature Algorithms: {:?}", _ext_data);
                                                                                } else if ext_type == 10 { // supported_groups
                                                                                    println!("  Supported Groups: {:?}", _ext_data);
                                                                                } else if ext_type == 14 { // use_srtp
                                                                                    println!("  SRTP Extension: {:?}", _ext_data);
                                                                                    if _ext_data.len() >= 2 {
                                                                                        let len = u16::from_be_bytes([_ext_data[0], _ext_data[1]]) as usize;
                                                                                        let mut idx = 2;
                                                                                        while idx < 2 + len && idx + 1 < _ext_data.len() {
                                                                                            let profile = u16::from_be_bytes([_ext_data[idx], _ext_data[idx+1]]);
                                                                                            srtp_profiles.push(profile);
                                                                                            idx += 2;
                                                                                        }
                                                                                    }
                                                                                }
                                                                            }

                                                                            let random = Random::new();
                                                                            server_random = Some(random.to_bytes());

                                            let mut extensions = Vec::new();
                                            // Supported Point Formats (uncompressed)
                                            extensions.extend_from_slice(&[0x00, 0x0b]); // Type
                                            extensions.extend_from_slice(&[0x00, 0x02]); // Length
                                            extensions.extend_from_slice(&[0x01]); // List Length
                                            extensions.extend_from_slice(&[0x00]); // uncompressed

                                            // Renegotiation Info (empty)
                                            extensions.extend_from_slice(&[0xff, 0x01]); // Type
                                            extensions.extend_from_slice(&[0x00, 0x01]); // Length
                                            extensions.extend_from_slice(&[0x00]);       // Body (len 0)

                                            // Extended Master Secret (removed to avoid implementing RFC 7627 logic for now)
                                            // extensions.extend_from_slice(&[0x00, 0x17]); // Type
                                            // extensions.extend_from_slice(&[0x00, 0x00]); // Length

                                            // Use SRTP
                                            if !srtp_profiles.is_empty() {
                                                let selected_profile = srtp_profiles[0];
                                                extensions.extend_from_slice(&[0x00, 0x0e]); // Type 14
                                                extensions.extend_from_slice(&[0x00, 0x05]); // Length
                                                extensions.extend_from_slice(&[0x00, 0x02]); // List Length
                                                extensions.extend_from_slice(&selected_profile.to_be_bytes());
                                                extensions.extend_from_slice(&[0x00]); // MKI Length
                                            }

                                            let server_hello = ServerHello {
                                                version: ProtocolVersion::DTLS_1_2,
                                                random,
                                                session_id: client_hello.session_id,
                                                cipher_suite: 0xC02B, // TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
                                                compression_method: 0,
                                                extensions,
                                            };                                                                            let mut body = BytesMut::new();
                                                                            server_hello.encode(&mut body);

                                                                            let handshake_msg = HandshakeMessage {
                                                                                msg_type:
                                                                                    HandshakeType::ServerHello,
                                                                                message_seq,
                                                                                fragment_offset: 0,
                                                                                fragment_length: body.len() as u32,
                                                                                body: body.freeze(),
                                                                            };

                                                                            let mut buf = BytesMut::new();
                                                                            handshake_msg.encode(&mut buf);
                                                                            handshake_messages
                                                                                .extend_from_slice(&buf);

                                                                            self.send_handshake_message(
                                                                                handshake_msg,
                                                                                epoch,
                                                                                &mut sequence_number,
                                                                                None,
                                                                                is_client,
                                                                            )
                                                                            .await?;
                                                                            message_seq += 1;

                                                                            // Send Certificate
                                                                            let cert_msg = CertificateMessage {
                                                                                certificates: certificate
                                                                                    .certificate
                                                                                    .clone(),
                                                                            };

                                                                            let mut body = BytesMut::new();
                                                                            cert_msg.encode(&mut body);

                                                                            let handshake_msg = HandshakeMessage {
                                                                                msg_type:
                                                                                    HandshakeType::Certificate,
                                                                                message_seq,
                                                                                fragment_offset: 0,
                                                                                fragment_length: body.len() as u32,
                                                                                body: body.freeze(),
                                                                            };

                                                                            let mut buf = BytesMut::new();
                                                                            handshake_msg.encode(&mut buf);
                                                                            handshake_messages
                                                                                .extend_from_slice(&buf);

                                                                            self.send_handshake_message(
                                                                                handshake_msg,
                                                                                epoch,
                                                                                &mut sequence_number,
                                                                                None,
                                                                                is_client,
                                                                            )
                                                                            .await?;
                                                                            message_seq += 1;

                                                                            // Send ServerKeyExchange
                                                                            let mut params = Vec::new();
                                                                            if let (Some(cr), Some(sr)) = (&client_random, &server_random) {
                                                                                params.extend_from_slice(cr);
                                                                                params.extend_from_slice(sr);
                                                                            }
                                                                            params.push(3); // curve_type: named_curve
                                                                            params.extend_from_slice(&23u16.to_be_bytes()); // named_curve: secp256r1
                                                                            params.push(local_public_key_bytes.len() as u8);
                                                                            params.extend_from_slice(&local_public_key_bytes);

                                                                            let signing_key = SigningKey::from_pkcs8_pem(&certificate.private_key)
                                                                                .map_err(|e| anyhow::anyhow!("Failed to parse private key: {}", e))?;
                                                                            let signature: p256::ecdsa::Signature = signing_key.sign(&params);
                                                                            let signature_bytes = signature.to_der().as_bytes().to_vec();

                                                                            println!("Signed params len: {}", params.len());
                                                                            println!("Signature len: {}", signature_bytes.len());
                                                                            println!("Signature: {:?}", signature_bytes);

                                                                            // Self-verification
                                                                            let verifying_key = signing_key.verifying_key();
                                                                            if let Err(e) = verifying_key.verify(&params, &signature) {
                                                                                println!("SELF-VERIFICATION FAILED: {}", e);
                                                                            } else {
                                                                                println!("SELF-VERIFICATION SUCCEEDED");
                                                                            }

                                                                            let server_key_exchange =
                                                                                ServerKeyExchange {
                                                                                    curve_type: 3,   // named_curve
                                                                                    named_curve: 23, // secp256r1
                                                                                    public_key:
                                                                                        local_public_key_bytes
                                                                                            .clone(),
                                                                                    signature: signature_bytes,
                                                                                };

                                                                            let mut body = BytesMut::new();
                                                                            server_key_exchange.encode(&mut body);

                                                                            let handshake_msg = HandshakeMessage {
                                                                                msg_type:
                                                                                    HandshakeType::ServerKeyExchange,
                                                                                message_seq,
                                                                                fragment_offset: 0,
                                                                                fragment_length: body.len() as u32,
                                                                                body: body.freeze(),
                                                                            };

                                                                            let mut buf = BytesMut::new();
                                                                            handshake_msg.encode(&mut buf);
                                                                            handshake_messages
                                                                                .extend_from_slice(&buf);

                                                                            self.send_handshake_message(
                                                                                handshake_msg,
                                                                                epoch,
                                                                                &mut sequence_number,
                                                                                None,
                                                                                is_client,
                                                                            )
                                                                            .await?;
                                                                            message_seq += 1;

                                                                            // Send ServerHelloDone
                                                                            let done_msg = ServerHelloDone {};
                                                                            let mut body = BytesMut::new();
                                                                            done_msg.encode(&mut body);

                                                                            let handshake_msg = HandshakeMessage {
                                                                                msg_type:
                                                                                    HandshakeType::ServerHelloDone,
                                                                                message_seq,
                                                                                fragment_offset: 0,
                                                                                fragment_length: body.len() as u32,
                                                                                body: body.freeze(),
                                                                            };

                                                                            let mut buf = BytesMut::new();
                                                                            handshake_msg.encode(&mut buf);
                                                                            handshake_messages
                                                                                .extend_from_slice(&buf);

                                                                            self.send_handshake_message(
                                                                                handshake_msg,
                                                                                epoch,
                                                                                &mut sequence_number,
                                                                                None,
                                                                                is_client,
                                                                            )
                                                                            .await?;
                                                                            message_seq += 1;
                                                                            }
                                                                            Err(e) => {
                                                                                println!("Failed to decode ClientHello: {}", e);
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                HandshakeType::ClientKeyExchange => {
                                                                    if !is_client {
                                                                        println!("Received ClientKeyExchange");
                                                                        let mut body = msg.body.clone();
                                                                        if let Ok(client_key_exchange) =
                                                                            ClientKeyExchange::decode(&mut body)
                                                                        {
                                                                            peer_public_key = Some(
                                                                                client_key_exchange.public_key,
                                                                            );

                                                                            // Compute shared secret
                                                                            if let Some(peer_key) = &peer_public_key
                                                                            {
                                                                                if let Some(secret) =
                                                                                    local_secret_opt.take()
                                                                                {
                                                                                    if let Ok(pk) =
                                                                                        PublicKey::from_sec1_bytes(
                                                                                            peer_key,
                                                                                        )
                                                                                    {
                                                                                        let shared_secret = secret
                                                                                            .diffie_hellman(&pk);
                                                                                        println!("Shared secret computed (Server)");

                                                                                        if let (
                                                                                            Some(cr),
                                                                                            Some(sr),
                                                                                        ) = (
                                                                                            &client_random,
                                                                                            &server_random,
                                                                                        ) {
                                                                                            let pre_master_secret = shared_secret.raw_secret_bytes();
                                                                                            let mut seed =
                                                                                                Vec::new();
                                                                                            seed.extend_from_slice(
                                                                                                cr,
                                                                                            );
                                                                                            seed.extend_from_slice(
                                                                                                sr,
                                                                                            );

                                                                                            if let Ok(
                                                                                                master_secret,
                                                                                            ) = prf_sha256(
                                                                                                pre_master_secret,
                                                                                                b"master secret",
                                                                                                &seed,
                                                                                                48,
                                                                                            ) {
                                                                                                println!("Master secret derived: {:?}", master_secret);
                                                                                                if let Ok(keys) = expand_keys(&master_secret, cr, sr) {
                                                                                                    println!("Session keys derived (Server)");
                                                                                                    session_keys = Some(keys);
                                                                                                }
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                HandshakeType::Finished => {
                                                                    println!("Received Finished");
                                                                    let mut body = msg.body.clone();
                                                                    if let Ok(finished) =
                                                                        Finished::decode(&mut body)
                                                                    {
                                                                        if !is_client {
                                                                            // Verify Client's Finished
                                                                            if let Some(keys) = &session_keys {
                                                                                let expected_verify_data =
                                                                                    calculate_verify_data(
                                                                                        &keys.master_secret,
                                                                                        b"client finished",
                                                                                        &handshake_messages,
                                                                                    )?;

                                                                                if finished.verify_data
                                                                                    != expected_verify_data
                                                                                {
                                                                                    eprintln!("Finished verification failed. Expected {:?}, got {:?}", expected_verify_data, finished.verify_data);
                                                                                    *self.state.lock().await =
                                                                                        DtlsState::Failed;
                                                                                    return Err(anyhow::anyhow!("Finished verification failed"));
                                                                                } else {
                                                                                    println!("Client Finished verified");
                                                                                }
                                                                            }

                                                                            // Add Client's Finished to transcript
                                                                            handshake_messages.extend_from_slice(&raw_msg);

                                                                            // Send ChangeCipherSpec
                                                                            let record = DtlsRecord {
                                                                                content_type:
                                                                                    ContentType::ChangeCipherSpec,
                                                                                version: ProtocolVersion::DTLS_1_2,
                                                                                epoch,
                                                                                sequence_number,
                                                                                payload: Bytes::from_static(&[1]),
                                                                            };
                                                                            // sequence_number increment is not needed as we reset it below

                                                                            let mut buf = BytesMut::new();
                                                                            record.encode(&mut buf);
                                                                            self.conn.send(&buf).await?;

                                                                            epoch += 1;
                                                                            sequence_number = 0;

                                                                            // Send Finished
                                                                            let verify_data =
                                                                                if let Some(keys) = &session_keys {
                                                                                    calculate_verify_data(
                                                                                        &keys.master_secret,
                                                                                        b"server finished",
                                                                                        &handshake_messages,
                                                                                    )?
                                                                                } else {
                                                                                    vec![0u8; 12]
                                                                                };

                                                                            let finished = Finished { verify_data };

                                                                            let mut body = BytesMut::new();
                                                                            finished.encode(&mut body);

                                                                            let handshake_msg = HandshakeMessage {
                                                                                msg_type: HandshakeType::Finished,
                                                                                message_seq,
                                                                                fragment_offset: 0,
                                                                                fragment_length: body.len() as u32,
                                                                                body: body.freeze(),
                                                                            };

                                                                            let mut buf = BytesMut::new();
                                                                            handshake_msg.encode(&mut buf);
                                                                            handshake_messages
                                                                                .extend_from_slice(&buf);

                                                                            self.send_handshake_message(
                                                                                handshake_msg,
                                                                                epoch,
                                                                                &mut sequence_number,
                                                                                session_keys.as_ref(),
                                                                                is_client,
                                                                            )
                                                                            .await?;
                                                                            // message_seq += 1; // End of handshake

                                                                            if let Some(keys) = &session_keys
                                                                            {
                                                                                *self.state.lock().await =
                                                                                    DtlsState::Connected(keys.clone());
                                                                            } else {
                                                                                *self.state.lock().await =
                                                                                    DtlsState::Failed;
                                                                                return Err(anyhow::anyhow!(
                                                                                    "Session keys not derived"
                                                                                ));
                                                                            }
                                                                            // return Ok(());
                                                                        } else {
                                                                            // Client logic: Verify Finished
                                                                            if let Some(keys) = &session_keys {
                                                                                let expected_verify_data =
                                                                                    calculate_verify_data(
                                                                                        &keys.master_secret,
                                                                                        b"server finished",
                                                                                        &handshake_messages,
                                                                                    )?;

                                                                                if finished.verify_data
                                                                                    != expected_verify_data
                                                                                {
                                                                                    eprintln!("Finished verification failed. Expected {:?}, got {:?}", expected_verify_data, finished.verify_data);
                                                                                    *self.state.lock().await =
                                                                                        DtlsState::Failed;
                                                                                    return Err(anyhow::anyhow!("Finished verification failed"));
                                                                                } else {
                                                                                    println!("Finished verified");
                                                                                    if let Some(keys) =
                                                                                        &session_keys
                                                                                    {
                                                                                        *self.state.lock().await =
                                                                                            DtlsState::Connected(
                                                                                                keys.clone(),
                                                                                            );
                                                                                    }
                                                                                    // return Ok(());
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                HandshakeType::HelloVerifyRequest => {
                                                                    println!("Received HelloVerifyRequest");
                                                                    let mut body = msg.body.clone();
                                                                    if let Ok(verify_req) =
                                                                        HelloVerifyRequest::decode(&mut body)
                                                                    {
                                                                        println!(
                                                                            "Resending ClientHello with cookie"
                                                                        );

                                                                        // Reset handshake messages
                                                                        handshake_messages.clear();

                                                                        // Reuse previous random
                                                                        let random =
                                                                            if let Some(bytes) = &client_random {
                                                                                let gmt = u32::from_be_bytes([
                                                                                    bytes[0], bytes[1], bytes[2],
                                                                                    bytes[3],
                                                                                ]);
                                                                                let mut rb = [0u8; 28];
                                                                                rb.copy_from_slice(&bytes[4..32]);
                                                                                Random {
                                                                                    gmt_unix_time: gmt,
                                                                                    random_bytes: rb,
                                                                                }
                                                                            } else {
                                                                                Random::new()
                                                                            };
                                                                        // client_random is already set, no need to update

                                                                        let mut extensions = Vec::new();
                                                                        // Supported Elliptic Curves (secp256r1)
                                                                        extensions.extend_from_slice(&[
                                                                            0x00, 0x0a, 0x00, 0x04, 0x00, 0x02,
                                                                            0x00, 0x17,
                                                                        ]);
                                                                        // Supported Point Formats (uncompressed)
                                                                        extensions.extend_from_slice(&[
                                                                            0x00, 0x0b, 0x00, 0x02, 0x01, 0x00,
                                                                        ]);

                                                                        let client_hello = ClientHello {
                                                                            version: ProtocolVersion::DTLS_1_2,
                                                                            random,
                                                                            session_id: vec![],
                                                                            cookie: verify_req.cookie,
                                                                            cipher_suites: vec![0xC02B],
                                                                            compression_methods: vec![0],
                                                                            extensions,
                                                                        };

                                                                        let mut body = BytesMut::new();
                                                                        client_hello.encode(&mut body);

                                                                        let handshake_msg = HandshakeMessage {
                                                                            msg_type: HandshakeType::ClientHello,
                                                                            message_seq,
                                                                            fragment_offset: 0,
                                                                            fragment_length: body.len() as u32,
                                                                            body: body.freeze(),
                                                                        };

                                                                        let mut buf = BytesMut::new();
                                                                        handshake_msg.encode(&mut buf);
                                                                        handshake_messages.extend_from_slice(&buf);

                                                                        self.send_handshake_message(
                                                                            handshake_msg,
                                                                            epoch,
                                                                            &mut sequence_number,
                                                                            None,
                                                                            is_client,
                                                                        )
                                                                        .await?;
                                                                        message_seq += 1;
                                                                    }
                                                                }
                                                                HandshakeType::ServerHello => {
                                                                    if is_client {
                                                                        println!("Received ServerHello");
                                                                        let mut body = msg.body.clone();
                                                                        if let Ok(server_hello) =
                                                                            ServerHello::decode(&mut body)
                                                                        {
                                                                            server_random = Some(
                                                                                server_hello.random.to_bytes(),
                                                                            );
                                                                            println!(
                                                                                "Server extensions len: {}",
                                                                                server_hello.extensions.len()
                                                                            );
                                                                            if !server_hello.extensions.is_empty() {
                                                                                println!(
                                                                                    "Server extensions: {:?}",
                                                                                    server_hello.extensions
                                                                                );
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                HandshakeType::Certificate => {
                                                                    println!("Received Certificate");
                                                                }
                                                                HandshakeType::ServerKeyExchange => {
                                                                    if is_client {
                                                                        println!("Received ServerKeyExchange");
                                                                        let mut body = msg.body.clone();
                                                                        if let Ok(server_key_exchange) =
                                                                            ServerKeyExchange::decode(&mut body)
                                                                        {
                                                                            peer_public_key = Some(
                                                                                server_key_exchange.public_key,
                                                                            );
                                                                        }
                                                                    }
                                                                }
                                                                HandshakeType::ServerHelloDone => {
                                                                    println!("Received ServerHelloDone");

                                                                    // Send ClientKeyExchange
                                                                    let client_key_exchange = ClientKeyExchange {
                                                                        identity_hint: vec![],
                                                                        public_key: local_public_key_bytes.clone(),
                                                                    };

                                                                    let mut body = BytesMut::new();
                                                                    client_key_exchange.encode(&mut body);

                                                                    let handshake_msg = HandshakeMessage {
                                                                        msg_type: HandshakeType::ClientKeyExchange,
                                                                        message_seq,
                                                                        fragment_offset: 0,
                                                                        fragment_length: body.len() as u32,
                                                                        body: body.freeze(),
                                                                    };

                                                                    let mut buf = BytesMut::new();
                                                                    handshake_msg.encode(&mut buf);
                                                                    handshake_messages.extend_from_slice(&buf);

                                                                    self.send_handshake_message(
                                                                        handshake_msg,
                                                                        epoch,
                                                                        &mut sequence_number,
                                                                        None,
                                                                        is_client,
                                                                    )
                                                                    .await?;
                                                                    message_seq += 1;

                                                                    // Compute shared secret
                                                                    if let Some(peer_key) = &peer_public_key {
                                                                        if let Some(secret) =
                                                                            local_secret_opt.take()
                                                                        {
                                                                            if let Ok(pk) =
                                                                                PublicKey::from_sec1_bytes(peer_key)
                                                                            {
                                                                                let shared_secret =
                                                                                    secret.diffie_hellman(&pk);
                                                                                println!("Shared secret computed (Client)");

                                                                                if let (Some(cr), Some(sr)) =
                                                                                    (&client_random, &server_random)
                                                                                {
                                                                                    let pre_master_secret =
                                                                                        shared_secret
                                                                                            .raw_secret_bytes();
                                                                                    let mut seed = Vec::new();
                                                                                    seed.extend_from_slice(cr);
                                                                                    seed.extend_from_slice(sr);

                                                                                    if let Ok(master_secret) =
                                                                                        prf_sha256(
                                                                                            pre_master_secret,
                                                                                            b"master secret",
                                                                                            &seed,
                                                                                            48,
                                                                                        )
                                                                                    {
                                                                                        println!("Master secret derived: {:?}", master_secret);
                                                                                        if let Ok(keys) =
                                                                                            expand_keys(
                                                                                                &master_secret,
                                                                                                cr,
                                                                                                sr,
                                                                                            )
                                                                                        {
                                                                                            println!("Session keys derived (Client)");
                                                                                            session_keys =
                                                                                                Some(keys);

                                                                                            // Send ChangeCipherSpec
                                                                                            let record = DtlsRecord {
                                                                                                content_type: ContentType::ChangeCipherSpec,
                                                                                                version: ProtocolVersion::DTLS_1_2,
                                                                                                epoch,
                                                                                                sequence_number,
                                                                                                payload: Bytes::from_static(&[1]),
                                                                                            };

                                                                                            let mut buf =
                                                                                                BytesMut::new();
                                                                                            record.encode(&mut buf);
                                                                                            self.conn
                                                                                                .send(&buf)
                                                                                                .await?;

                                                                                            epoch += 1;
                                                                                            sequence_number = 0;

                                                                                            // Send Finished
                                                                                            let verify_data = calculate_verify_data(&master_secret, b"client finished", &handshake_messages)?;

                                                                                            let finished =
                                                                                                Finished {
                                                                                                    verify_data,
                                                                                                };

                                                                                            let mut body =
                                                                                                BytesMut::new();
                                                                                            finished
                                                                                                .encode(&mut body);

                                                                                            let handshake_msg = HandshakeMessage {
                                                                                                msg_type: HandshakeType::Finished,
                                                                                                message_seq,
                                                                                                fragment_offset: 0,
                                                                                                fragment_length: body.len() as u32,
                                                                                                body: body.freeze(),
                                                                                            };

                                                                                            let mut buf =
                                                                                                BytesMut::new();
                                                                                            handshake_msg
                                                                                                .encode(&mut buf);
                                                                                            handshake_messages
                                                                                                .extend_from_slice(
                                                                                                    &buf,
                                                                                                );

                                                                                            self.send_handshake_message(handshake_msg, epoch, &mut sequence_number, session_keys.as_ref(), is_client).await?;
                                                                                            message_seq += 1;
                                                                                        }
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                _ => {}
                                                            }
                                                        }
                                                        Err(e) => {
                                                            eprintln!("Failed to decode handshake message: {}", e);
                                                            body = Bytes::new();
                                                        }
                                                    }
                                                }
                                    }
                                    ContentType::Alert => {
                                        println!("Received Alert: {:?}", payload);
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                eprintln!("Failed to decode DTLS record: {}", e);
                                data = Bytes::new();
                            }
                        }
                    }
                }
                Some(data) = outgoing_data_rx.recv() => {
                    if let Some(keys) = &session_keys {
                        let (key, iv) = if is_client {
                            (&keys.client_write_key, &keys.client_write_iv)
                        } else {
                            (&keys.server_write_key, &keys.server_write_iv)
                        };

                        let full_seq = ((epoch as u64) << 48) | sequence_number;

                        match encrypt_record(
                            ContentType::ApplicationData,
                            ProtocolVersion::DTLS_1_2,
                            full_seq,
                            &data,
                            key,
                            iv
                        ) {
                            Ok(encrypted) => {
                                let record = DtlsRecord {
                                    content_type: ContentType::ApplicationData,
                                    version: ProtocolVersion::DTLS_1_2,
                                    epoch,
                                    sequence_number,
                                    payload: Bytes::from(encrypted),
                                };
                                sequence_number += 1;

                                let mut buf = BytesMut::new();
                                record.encode(&mut buf);
                                if let Err(e) = self.conn.send(&buf).await {
                                     eprintln!("Failed to send application data: {}", e);
                                }
                            },
                            Err(e) => eprintln!("Failed to encrypt application data: {}", e),
                        }
                    } else {
                        eprintln!("Cannot send application data: handshake not completed");
                    }
                }
            }
        }
    }

    async fn send_handshake_message(
        &self,
        msg: HandshakeMessage,
        epoch: u16,
        sequence_number: &mut u64,
        session_keys: Option<&SessionKeys>,
        is_client: bool,
    ) -> Result<Vec<u8>> {
        let mut buf = BytesMut::new();
        msg.encode(&mut buf);
        let payload = buf.freeze();

        println!(
            "Sending HandshakeMessage: {:?} len={}",
            msg.msg_type,
            payload.len()
        );

        let mut record_payload = payload;

        if epoch > 0 {
            if let Some(keys) = session_keys {
                let (key, iv) = if is_client {
                    (&keys.client_write_key, &keys.client_write_iv)
                } else {
                    (&keys.server_write_key, &keys.server_write_iv)
                };

                let full_seq = ((epoch as u64) << 48) | *sequence_number;

                record_payload = Bytes::from(encrypt_record(
                    ContentType::Handshake,
                    ProtocolVersion::DTLS_1_2,
                    full_seq,
                    &record_payload,
                    key,
                    iv,
                )?);
            }
        }

        let record = DtlsRecord {
            content_type: ContentType::Handshake,
            version: ProtocolVersion::DTLS_1_2,
            epoch,
            sequence_number: *sequence_number,
            payload: record_payload,
        };
        *sequence_number += 1;

        let mut buf = BytesMut::new();
        record.encode(&mut buf);
        self.conn.send(&buf).await?;

        Ok(buf.to_vec())
    }
}

impl Clone for DtlsTransport {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            state: self.state.clone(),
            incoming_data_rx: self.incoming_data_rx.clone(),
            outgoing_data_tx: self.outgoing_data_tx.clone(),
            handshake_rx_feeder: self.handshake_rx_feeder.clone(),
        }
    }
}

use crate::transports::PacketReceiver;
use std::net::SocketAddr;

#[async_trait::async_trait]
impl PacketReceiver for DtlsTransport {
    async fn receive(&self, packet: Bytes, _addr: SocketAddr) {
        // Push to the internal handshake loop
        // We ignore errors here (e.g. if the loop is closed)
        let _ = self.handshake_rx_feeder.send(packet.to_vec()).await;
    }
}

fn prf_sha256(secret: &[u8], label: &[u8], seed: &[u8], output_length: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut real_seed = Vec::new();
    real_seed.extend_from_slice(label);
    real_seed.extend_from_slice(seed);

    let mut a = real_seed.clone();

    while output.len() < output_length {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
            .map_err(|_| anyhow::anyhow!("Invalid key length"))?;
        mac.update(&a);
        a = mac.finalize().into_bytes().to_vec();

        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
            .map_err(|_| anyhow::anyhow!("Invalid key length"))?;
        mac.update(&a);
        mac.update(&real_seed);
        let block = mac.finalize().into_bytes();

        let len = std::cmp::min(block.len(), output_length - output.len());
        output.extend_from_slice(&block[..len]);
    }

    Ok(output)
}

fn expand_keys(
    master_secret: &[u8],
    client_random: &[u8],
    server_random: &[u8],
) -> Result<SessionKeys> {
    let mut keys = SessionKeys {
        client_write_key: vec![0u8; 16],
        server_write_key: vec![0u8; 16],
        client_write_iv: vec![0u8; 4],
        server_write_iv: vec![0u8; 4],
        master_secret: master_secret.to_vec(),
        client_random: client_random.to_vec(),
        server_random: server_random.to_vec(),
    };

    let key_block = prf_sha256(
        master_secret,
        b"key expansion",
        [server_random, client_random].concat().as_slice(),
        40,
    )?;

    keys.client_write_key.copy_from_slice(&key_block[0..16]);
    keys.server_write_key.copy_from_slice(&key_block[16..32]);
    keys.client_write_iv.copy_from_slice(&key_block[32..36]);
    keys.server_write_iv.copy_from_slice(&key_block[36..40]);

    Ok(keys)
}

fn calculate_verify_data(
    master_secret: &[u8],
    label: &[u8],
    handshake_messages: &[u8],
) -> Result<Vec<u8>> {
    let mut hasher = Sha256::new();
    hasher.update(handshake_messages);
    let hash = hasher.finalize();
    println!(
        "VerifyData: label={:?} hash={:?}",
        String::from_utf8_lossy(label),
        hash
    );

    let verify_data = prf_sha256(master_secret, label, &hash, 12)?;
    Ok(verify_data)
}

fn make_aad(
    seq: u64,
    content_type: ContentType,
    version: ProtocolVersion,
    length: usize,
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(13);
    aad.extend_from_slice(&seq.to_be_bytes());
    aad.push(content_type as u8);
    aad.push(version.major);
    aad.push(version.minor);
    aad.extend_from_slice(&(length as u16).to_be_bytes());
    aad
}

fn decrypt_record(
    content_type: ContentType,
    version: ProtocolVersion,
    seq: u64,
    payload: &Bytes,
    key: &[u8],
    iv: &[u8],
) -> Result<Vec<u8>> {
    if payload.len() < 8 {
        return Err(anyhow::anyhow!("Record too short for explicit nonce"));
    }

    let explicit_nonce = &payload[0..8];
    let ciphertext = &payload[8..];

    if ciphertext.len() < 16 {
        return Err(anyhow::anyhow!("Ciphertext too short for tag"));
    }

    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[0..4].copy_from_slice(iv);
    nonce_bytes[4..12].copy_from_slice(explicit_nonce);

    let nonce = Nonce::from_slice(&nonce_bytes);
    let cipher =
        Aes128Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("Invalid key length"))?;

    let plaintext_len = ciphertext.len() - 16;
    let aad = make_aad(seq, content_type, version, plaintext_len);

    println!(
        "Decrypting: seq={} epoch={} seq_num={} type={:?} ver={:?} len={}",
        seq,
        seq >> 48,
        seq & 0xFFFFFFFFFFFF,
        content_type,
        version,
        plaintext_len
    );
    println!("Nonce: {:?}", nonce_bytes);
    println!("AAD: {:?}", aad);
    println!("Key: {:?}", key);
    println!("IV: {:?}", iv);

    let decrypted_payload = cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;

    Ok(decrypted_payload)
}

fn encrypt_record(
    content_type: ContentType,
    version: ProtocolVersion,
    seq: u64,
    payload: &[u8],
    key: &[u8],
    iv: &[u8],
) -> Result<Vec<u8>> {
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[0..4].copy_from_slice(iv);
    nonce_bytes[4..12].copy_from_slice(&seq.to_be_bytes());

    let nonce = Nonce::from_slice(&nonce_bytes);
    let cipher =
        Aes128Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("Invalid key length"))?;

    let aad = make_aad(seq, content_type, version, payload.len());

    let encrypted_payload = cipher
        .encrypt(
            nonce,
            Payload {
                msg: payload,
                aad: &aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    let mut result = Vec::new();
    result.extend_from_slice(&nonce_bytes[4..12]);
    result.extend_from_slice(&encrypted_payload);

    Ok(result)
}
