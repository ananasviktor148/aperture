//! aperture-relay: public rendezvous for WebRTC connections.
//!
//! Friends open `https://<relay>/r/<room>?t=<token>` and get the viewer page.
//! The page and the agent exchange SDP/ICE through this relay over WebSocket;
//! the video itself flows peer-to-peer once the connection is up.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

const INDEX_HTML: &str = include_str!("../../viewer/index.html");
const APP_JS: &str = include_str!("../../viewer/app.js");
const STYLE_CSS: &str = include_str!("../../viewer/style.css");

const DEFAULT_STUN: &str = "stun:stun.l.google.com:19302";

#[derive(Parser)]
#[command(version, about = "aperture-relay: public WebRTC rendezvous")]
struct Args {
    /// Address to bind.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// Port to listen on. Serve behind Caddy/nginx for public TLS.
    #[arg(long, default_value_t = 9000)]
    port: u16,
    /// TURN server, comma-separated: "turn:host:3478?transport=udp,username,credential". Repeatable.
    #[arg(long = "turn")]
    turns: Vec<String>,
}

#[derive(Clone)]
struct RelayState {
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    ice_servers: Arc<Value>,
}

#[derive(Default)]
struct Room {
    host: Option<mpsc::UnboundedSender<String>>,
    guests: HashMap<String, mpsc::UnboundedSender<String>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("relay=info,axum::rejection=trace")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let mut ice_servers = vec![json!({ "urls": [DEFAULT_STUN] })];
    for spec in &args.turns {
        let mut parts = spec.splitn(3, ',');
        let url = parts.next().unwrap_or("").trim();
        let user = parts.next().map(str::trim).unwrap_or("");
        let pass = parts.next().map(str::trim).unwrap_or("");
        if url.is_empty() {
            anyhow::bail!("--turn must look like: turn:host:3478?transport=udp,username,credential");
        }
        ice_servers.push(json!({
            "urls": [url],
            "username": user,
            "credential": pass,
        }));
    }

    let state = RelayState {
        rooms: Arc::new(Mutex::new(HashMap::new())),
        ice_servers: Arc::new(json!(ice_servers)),
    };

    let router = Router::new()
        .route("/r/{room}", get(serve_viewer))
        .route("/app.js", get(|| async { static_file(APP_JS, "text/javascript") }))
        .route("/style.css", get(|| async { static_file(STYLE_CSS, "text/css") }))
        .route("/health", get(|| async { "ok" }))
        .route("/ws/{room}", get(signaling))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    tracing::info!(%addr, "relay listening");
    if !args.turns.is_empty() {
        tracing::info!(turn_servers = args.turns.len(), "TURN enabled");
    }
    axum_server::bind(addr).serve(router.into_make_service()).await?;
    Ok(())
}

async fn serve_viewer(
    State(state): State<RelayState>,
    Path(room): Path<String>,
) -> Response {
    if !valid_room(&room) {
        return (StatusCode::NOT_FOUND, "unknown room").into_response();
    }
    let ice = state.ice_servers.clone();
    let inject = format!(
        "<script>window.APERTURE_ICE={ice};window.APERTURE_ROOM=\"{}\";</script>",
        room.replace('"', "")
    );
    let html = INDEX_HTML.replacen("<!--APERTURE_ICE-->", &inject, 1);
    let mut response = Html(html).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn static_file(body: &'static str, content_type: &'static str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response()
}

fn valid_room(room: &str) -> bool {
    room.len() >= 4
        && room.len() <= 64
        && room
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Deserialize)]
struct SignalQuery {
    role: String,
}

async fn signaling(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
    Path(room): Path<String>,
    Query(query): Query<SignalQuery>,
) -> Response {
    if !valid_room(&room) {
        return (StatusCode::NOT_FOUND, "unknown room").into_response();
    }
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| connection(socket, state, room, query.role))
}

async fn connection(socket: WebSocket, state: RelayState, room: String, role: String) {
    let (to_client, mut from_relay) = mpsc::unbounded_channel::<String>();
    let (guest_id, is_host) = {
        let mut rooms = state.rooms.lock();
        let room_state = rooms.entry(room.clone()).or_default();
        match role.as_str() {
            "host" => {
                if room_state.host.is_some() {
                    drop(rooms);
                    tracing::warn!(room, "second host rejected");
                    return;
                }
                room_state.host = Some(to_client.clone());
                (String::new(), true)
            }
            _ => {
                let id = random_id();
                room_state.guests.insert(id.clone(), to_client.clone());
                (id, false)
            }
        }
    };

    let (mut sink, mut stream) = socket.split();
    tracing::info!(room, role = if is_host { "host" } else { "guest" }, id = %guest_id, "connected");

    // Tell the newcomer who they are (and whether a host is present); let the
    // host know a guest arrived; let guests waiting know a host appeared.
    {
        let rooms = state.rooms.lock();
        if let Some(room_state) = rooms.get(&room) {
            if is_host {
                for guest in room_state.guests.values() {
                    let _ = guest.send(json!({ "type": "host-joined" }).to_string());
                }
            } else {
                let has_host = room_state.host.is_some();
                let _ = to_client.send(
                    json!({ "type": "joined", "id": guest_id, "host": has_host }).to_string(),
                );
                if let Some(host) = &room_state.host {
                    let _ = host.send(
                        json!({ "type": "guest-joined", "id": guest_id }).to_string(),
                    );
                }
            }
        }
    }

    let send_task = tokio::spawn(async move {
        while let Some(text) = from_relay.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(frame) = stream.next().await {
        let Ok(Message::Text(text)) = frame else {
            break;
        };
        let Ok(mut msg) = serde_json::from_str::<Value>(&text) else { continue };

        let rooms = state.rooms.lock();
        let Some(room_state) = rooms.get(&room) else { continue };
        if is_host {
            let target = msg.get("to").and_then(Value::as_str).map(str::to_owned);
            msg["from"] = Value::String("host".into());
            let out = msg.to_string();
            match target {
                Some(id) => {
                    if let Some(guest) = room_state.guests.get(&id) {
                        let _ = guest.send(out);
                    }
                }
                None => {
                    for guest in room_state.guests.values() {
                        let _ = guest.send(out.clone());
                    }
                }
            }
        } else {
            msg["from"] = Value::String(guest_id.clone());
            let out = msg.to_string();
            if let Some(host) = &room_state.host {
                let _ = host.send(out);
            }
        }
    }
    drop(stream);
    send_task.abort();

    // Deregistration and peer notification.
    let mut rooms = state.rooms.lock();
    let empty = if let Some(room_state) = rooms.get_mut(&room) {
        if is_host {
            room_state.host = None;
            for guest in room_state.guests.values() {
                let _ = guest.send(json!({ "type": "host-left" }).to_string());
            }
        } else {
            room_state.guests.remove(&guest_id);
            if let Some(host) = &room_state.host {
                let _ = host.send(json!({ "type": "guest-left", "id": guest_id }).to_string());
            }
        }
        room_state.host.is_none() && room_state.guests.is_empty()
    } else {
        false
    };
    if empty {
        rooms.remove(&room);
    }
    tracing::info!(room, role = if is_host { "host" } else { "guest" }, id = %guest_id, "disconnected");
}

fn random_id() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| ALPHABET[(rng.next_u32() as usize) % ALPHABET.len()] as char)
        .collect()
}
