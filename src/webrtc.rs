//! WebRTC hosting: friends connect straight to the agent over DataChannels.
//!
//! The agent dials out to the public relay as the room "host". Guest browsers
//! exchange SDP/ICE through the relay; once a DataChannel is up the aperture
//! protocol (JSON control + binary H.264) runs over it exactly like the
//! WebSocket transport, with the same freshness-over-completeness drop logic.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::agent::{Agent, ClientGuard};
use crate::protocol::{ServerMsg, encode_frame};
use crate::server::{handle_client_message, hello};
use crate::tls::token_matches;

/// Above this many bytes buffered in SCTP, drop frames instead of queueing.
const MAX_BUFFERED: u64 = 1 << 20;
const STUN_SERVER: &str = "stun:stun.l.google.com:19302";
/// webrtc-rs caps one DataChannel message at 64 KiB; large IDR frames must be
/// split into chunks. Header layout (shared with the viewer):
/// byte0 magic (1=frame, 2=continuation), byte1 bit0 keyframe, bit1 more-parts.
const MAX_DC_CHUNK: usize = 60 * 1024;
/// Hosted relays drop idle sockets; Cloudflare answers this exact text frame
/// itself, so the signalling link stays open without waking anything.
const KEEPALIVE: &str = "aperture-keepalive";

async fn send_binary(dc: &RTCDataChannel, binary: &[u8]) -> anyhow::Result<()> {
    if binary.len() <= MAX_DC_CHUNK {
        dc.send(&Bytes::copy_from_slice(binary)).await?;
        return Ok(());
    }
    let payload = &binary[8..];
    let head = MAX_DC_CHUNK - 8;
    let mut first = Vec::with_capacity(MAX_DC_CHUNK);
    first.extend_from_slice(&binary[..8]);
    first[1] |= 2;
    first.extend_from_slice(&payload[..head]);
    dc.send(&Bytes::from(first)).await?;

    let mut rest = &payload[head..];
    while !rest.is_empty() {
        let take = rest.len().min(head);
        let more = rest.len() > take;
        let mut chunk = Vec::with_capacity(8 + take);
        chunk.extend_from_slice(&[2, if more { 2 } else { 0 }, 0, 0, 0, 0, 0, 0]);
        chunk.extend_from_slice(&rest[..take]);
        dc.send(&Bytes::from(chunk)).await?;
        rest = &rest[take..];
    }
    Ok(())
}

/// Connect to the relay and serve guests until shutdown. Reconnects forever.
pub fn run(agent: Arc<Agent>, relay_ws_url: String, room: String, token: Arc<str>, turns: &[String]) {
    let ice = Arc::new(ice_servers(turns));
    tokio::spawn(async move {
        loop {
            match host_session(&agent, &relay_ws_url, &room, &token, &ice).await {
                Ok(()) => warn!(room, "relay connection closed; reconnecting"),
                Err(error) => warn!(room, %error, "relay connection failed; reconnecting"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// `"turn:host:3478?transport=udp,username,credential"` → an ICE server.
pub fn parse_turn(spec: &str) -> anyhow::Result<RTCIceServer> {
    let mut parts = spec.splitn(3, ',');
    let url = parts.next().unwrap_or("").trim();
    let user = parts.next().unwrap_or("").trim();
    let pass = parts.next().unwrap_or("").trim();
    anyhow::ensure!(
        !url.is_empty(),
        "--turn must look like: turn:host:3478?transport=udp,username,credential"
    );
    Ok(RTCIceServer {
        urls: vec![url.to_owned()],
        username: user.to_owned(),
        credential: pass.to_owned(),
        ..Default::default()
    })
}

fn ice_servers(turns: &[String]) -> Vec<RTCIceServer> {
    let mut servers = vec![RTCIceServer {
        urls: vec![STUN_SERVER.to_owned()],
        ..Default::default()
    }];
    // A friend behind carrier-grade NAT can only be reached through a relay,
    // and that needs credentials on this side too.
    servers.extend(turns.iter().filter_map(|spec| parse_turn(spec).ok()));
    servers
}

async fn host_session(
    agent: &Arc<Agent>,
    relay_ws_url: &str,
    room: &str,
    token: &Arc<str>,
    ice: &Arc<Vec<RTCIceServer>>,
) -> anyhow::Result<()> {
    let url = format!("{relay_ws_url}?role=host");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
    info!(room, "connected to relay as host");
    let (mut sink, mut stream) = ws.split();
    let (outbox, mut outbox_rx) = mpsc::unbounded_channel::<String>();
    let peers = Arc::new(Mutex::new(HashMap::<String, Arc<RTCPeerConnection>>::new()));
    let api = APIBuilder::new().build();

    let mut heartbeat = tokio::time::interval(Duration::from_secs(25));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.tick().await;

    loop {
        tokio::select! {
            Some(text) = outbox_rx.recv() => {
                sink.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await?;
            }
            _ = heartbeat.tick() => {
                sink.send(tokio_tungstenite::tungstenite::Message::Text(KEEPALIVE.into())).await?;
            }
            frame = stream.next() => {
                let Some(frame) = frame else { break };
                let Ok(tokio_tungstenite::tungstenite::Message::Text(text)) = frame else { continue };
                let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
                let from = msg.get("from").and_then(Value::as_str).unwrap_or("").to_string();
                match kind {
                    "offer" => {
                        let sdp = msg.get("sdp").and_then(Value::as_str).unwrap_or("").to_string();
                        let answer = handle_offer(
                            agent.clone(), token.clone(), &api, &peers, ice,
                            outbox.clone(), from.clone(), sdp,
                        ).await;
                        match answer {
                            Ok(sdp) => {
                                let _ = outbox.send(
                                    serde_json::json!({ "type": "answer", "sdp": sdp, "to": from }).to_string(),
                                );
                            }
                            Err(error) => warn!(guest = %from, %error, "offer failed"),
                        }
                    }
                    "ice" => {
                        let pc = peers.lock().get(&from).cloned();
                        if let Some(pc) = pc {
                            if let Ok(init) = serde_json::from_value::<RTCIceCandidateInit>(
                                msg.get("candidate").cloned().unwrap_or(Value::Null),
                            ) {
                                let _ = pc.add_ice_candidate(init).await;
                            }
                        }
                    }
                    "guest-left" => {
                        let pc = peers.lock().remove(&from);
                        if let Some(pc) = pc {
                            tokio::spawn(async move { let _ = pc.close().await; });
                        }
                    }
                    "guest-joined" => {}
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn handle_offer(
    agent: Arc<Agent>,
    token: Arc<str>,
    api: &webrtc::api::API,
    peers: &Arc<Mutex<HashMap<String, Arc<RTCPeerConnection>>>>,
    ice: &[RTCIceServer],
    outbox: mpsc::UnboundedSender<String>,
    guest: String,
    sdp: String,
) -> anyhow::Result<String> {
    let existing = peers.lock().get(&guest).cloned();
    if let Some(existing) = existing {
        let _ = existing.close().await;
        peers.lock().remove(&guest);
    }

    let config = RTCConfiguration {
        ice_servers: ice.to_vec(),
        ..Default::default()
    };
    let pc = Arc::new(api.new_peer_connection(config).await?);
    peers.lock().insert(guest.clone(), pc.clone());

    {
        let peers = peers.clone();
        let guest = guest.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let peers = peers.clone();
            let guest = guest.clone();
            Box::pin(async move {
                if matches!(
                    state,
                    RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed
                        | RTCPeerConnectionState::Disconnected
                ) {
                    let pc = peers.lock().remove(&guest);
                    if let Some(pc) = pc {
                        let _ = pc.close().await;
                    }
                }
            })
        }));
    }

    {
        let guest = guest.clone();
        pc.on_ice_candidate(Box::new(move |candidate| {
            let outbox = outbox.clone();
            let guest = guest.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Ok(init) = candidate.to_json() {
                        let _ = outbox.send(
                            serde_json::json!({ "type": "ice", "to": guest, "candidate": init })
                                .to_string(),
                        );
                    }
                }
            })
        }));
    }

    {
        let agent = agent.clone();
        let token = token.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let agent = agent.clone();
            let token = token.clone();
            Box::pin(async move { watch_data_channel(agent, token, dc); })
        }));
    }

    pc.set_remote_description(RTCSessionDescription::offer(sdp)?).await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;
    let local = pc.local_description().await;
    anyhow::ensure!(local.is_some(), "no local description after answer");
    Ok(local.unwrap().sdp)
}

fn watch_data_channel(agent: Arc<Agent>, token: Arc<str>, dc: Arc<RTCDataChannel>) {
    let authed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent_for_msg = agent.clone();
    let authed_for_msg = authed.clone();
    let dc_for_msg = dc.clone();

    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let agent = agent_for_msg.clone();
        let token = token.clone();
        let dc = dc_for_msg.clone();
        let authed = authed_for_msg.clone();
        Box::pin(async move {
            if !msg.is_string {
                return;
            }
            let text = String::from_utf8_lossy(&msg.data);
            if !authed.load(Ordering::Relaxed) {
                let Ok(auth) = serde_json::from_str::<Value>(&text) else { return };
                if auth.get("type").and_then(Value::as_str) != Some("auth") {
                    return;
                }
                let ok = token_matches(&token, auth.get("token").and_then(Value::as_str));
                if !ok {
                    warn!("guest auth failed; closing data channel");
                    dc.close().await.ok();
                    return;
                }
                authed.store(true, Ordering::Relaxed);
                info!("guest authenticated over WebRTC");
                tokio::spawn(run_session(agent, dc));
                return;
            }
            if let Some(reply) = handle_client_message(&agent, &text) {
                dc.send_text(&reply).await.ok();
            }
        })
    }));
}

async fn run_session(agent: Arc<Agent>, dc: Arc<RTCDataChannel>) {
    agent.client_connected();
    let _guard = ClientGuard(agent.clone());
    dc.send_text(&hello(&agent)).await.ok();

    let mut packets = agent.shared.packets.subscribe();
    let mut dims = {
        let state = agent.shared.state.read();
        (state.width, state.height)
    };
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;

    let mut window = Instant::now();
    let mut window_frames = 0u32;
    let mut window_bytes = 0u64;
    let mut capture_seen = agent.shared.capture_stats.frames.load(Ordering::Relaxed);
    let mut window_capture = 0u64;
    let mut drops = 0u32;
    let mut sent_seen = 0u64;

    loop {
        tokio::select! {
            biased;
            packet = packets.recv() => {
                match packet {
                    Ok(packet) => {
                        if (packet.width, packet.height) != dims {
                            dims = (packet.width, packet.height);
                            dc.send_text(&hello(&agent)).await.ok();
                        }
                        let binary = encode_frame(
                            packet.keyframe,
                            (packet.capture_us / 1000) as u32,
                            packet.encode_us.min(u16::MAX as u32) as u16,
                            &packet.data,
                        );
                        // Same rule as the socket transport: never let video
                        // pile up in a send buffer. Once a fragmented frame has
                        // started it must finish — dropping mid-frame would
                        // desync the reassembler.
                        if dc.buffered_amount().await > MAX_BUFFERED as usize {
                            drops += 1;
                            if drops >= 6 {
                                agent.shared.control.force_keyframe.store(true, Ordering::Relaxed);
                                drops = 0;
                            }
                            continue;
                        }
                        if let Err(error) = send_binary(&dc, &binary).await {
                            warn!(%error, len = packet.data.len(), keyframe = packet.keyframe, "dc send failed");
                            break;
                        }
                        window_frames += 1;
                        window_bytes += packet.data.len() as u64;
                        sent_seen += 1;
                        if sent_seen == 1 {
                            info!(len = packet.data.len(), keyframe = packet.keyframe, "first rtc frame sent");
                        } else if sent_seen % 600 == 0 {
                            tracing::debug!(sent_seen, len = packet.data.len(), "rtc frame sent");
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        agent.shared.control.force_keyframe.store(true, Ordering::Relaxed);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            _ = ticker.tick() => {
                let seconds = window.elapsed().as_secs_f32().max(0.001);
                let capture_now = agent.shared.capture_stats.frames.load(Ordering::Relaxed);
                window_capture += capture_now.saturating_sub(capture_seen);
                capture_seen = capture_now;
                let stats = ServerMsg::Stats {
                    fps: window_frames as f32 / seconds,
                    capture_fps: window_capture as f32 / seconds,
                    kbps: (window_bytes as f64 * 8.0 / 1000.0 / seconds as f64) as u32,
                    encode_us: agent.shared.control.last_encode_us.load(Ordering::Relaxed),
                    clients: agent.shared.clients.load(Ordering::Relaxed),
                };
                window = Instant::now();
                window_frames = 0;
                window_bytes = 0;
                window_capture = 0;
                let text = serde_json::to_string(&stats).unwrap_or_default();
                if dc.send_text(&text).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Stable room id derived from the token, so the share link survives restarts.
pub fn room_for_token(token: &str) -> String {
    token.chars().take(12).collect()
}

/// `https://relay.example` -> `wss://relay.example/ws/<room>`.
pub fn signaling_url(relay: &str, room: &str) -> String {
    let relay = relay.trim_end_matches('/');
    let ws = relay
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| relay.strip_prefix("http://").map(|rest| format!("ws://{rest}")))
        .unwrap_or_else(|| relay.to_string());
    format!("{ws}/ws/{room}")
}
