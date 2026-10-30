//! HTTPS + WebSocket transport and access control.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::task::Poll;
use futures_util::{SinkExt, StreamExt, poll};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::MissedTickBehavior;

use crate::agent::{Agent, ClientGuard};
use crate::input;
use crate::protocol::{encode_frame, ClientMsg, InputEvent, ServerMsg};
use crate::tls::token_matches;

const INDEX_HTML: &str = include_str!("../viewer/index.html");
const APP_JS: &str = include_str!("../viewer/app.js");
const STYLE_CSS: &str = include_str!("../viewer/style.css");

#[derive(Clone)]
pub struct AppState {
    pub agent: Arc<Agent>,
    pub token: Arc<str>,
}

#[derive(Deserialize)]
struct AuthQuery {
    t: Option<String>,
}

pub async fn serve(
    state: AppState,
    bind: IpAddr,
    port: u16,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
) -> anyhow::Result<()> {
    let router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(|| async { static_file(APP_JS, "text/javascript") }))
        .route("/style.css", get(|| async { static_file(STYLE_CSS, "text/css") }))
        .route("/ws", get(ws))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let addr = SocketAddr::new(bind, port);
    match tls {
        Some(config) => {
            axum_server::bind_rustls(addr, config)
                .serve(router.into_make_service())
                .await?;
        }
        None => {
            axum_server::bind(addr)
                .serve(router.into_make_service())
                .await?;
        }
    }
    Ok(())
}

async fn index(State(app): State<AppState>, Query(query): Query<AuthQuery>) -> Response {
    if !token_matches(&app.token, query.t.as_deref()) {
        return unauthorized();
    }
    let mut response = Html(INDEX_HTML).into_response();
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

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Aperture: missing or invalid access token.\nUse the full link printed by the agent (it contains ?t=...).\n",
    )
        .into_response()
}

async fn ws(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    Query(query): Query<AuthQuery>,
) -> Response {
    if !token_matches(&app.token, query.t.as_deref()) {
        return unauthorized();
    }
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| client_loop(socket, app))
}

pub(crate) fn hello(agent: &Agent) -> String {
    let shared = &agent.shared;
    let state = shared.state.read();
    let message = ServerMsg::Hello {
        width: state.width,
        height: state.height,
        desktop_width: state.monitor_rect.width.max(1) as u32,
        desktop_height: state.monitor_rect.height.max(1) as u32,
        fps: state.fps,
        bitrate: state.bitrate,
        encoder: &state.encoder_name,
        monitor: state.monitor,
        monitors: &shared.monitors,
    };
    serde_json::to_string(&message).unwrap_or_else(|_| "{}".into())
}

async fn client_loop(socket: WebSocket, app: AppState) {
    let agent = app.agent.clone();
    agent.client_connected();
    let _guard = ClientGuard(agent.clone());

    let mut packets = agent.shared.packets.subscribe();
    let (mut sink, mut stream) = socket.split();
    if sink.send(Message::Text(hello(&agent).into())).await.is_err() {
        return;
    }

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

    loop {
        tokio::select! {
            biased;
            packet = packets.recv() => {
                match packet {
                    Ok(packet) => {
                        if (packet.width, packet.height) != dims {
                            dims = (packet.width, packet.height);
                            if sink.send(Message::Text(hello(&agent).into())).await.is_err() {
                                break;
                            }
                        }
                        let binary = encode_frame(
                            packet.keyframe,
                            (packet.capture_us / 1000) as u32,
                            packet.encode_us.min(u16::MAX as u32) as u16,
                            &packet.data,
                        );
                        // Never queue behind a slow link: if the socket would
                        // block, drop the frame and resync at the next IDR.
                        let ready = poll!(futures_util::future::poll_fn(|cx| {
                            sink.poll_ready_unpin(cx)
                        }));
                        match ready {
                            Poll::Ready(Ok(())) => {
                                if sink.start_send_unpin(Message::Binary(binary.into())).is_err() {
                                    break;
                                }
                                // start_send only buffers; push the frame to the
                                // socket now (best effort, never blocking).
                                let _ = poll!(futures_util::future::poll_fn(|cx| {
                                    sink.poll_flush_unpin(cx)
                                }));
                                window_frames += 1;
                                window_bytes += packet.data.len() as u64;
                            }
                            _ => {
                                drops += 1;
                                if drops >= 6 {
                                    agent.shared.control.force_keyframe.store(true, Ordering::Relaxed);
                                    drops = 0;
                                }
                            }
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // The client cannot keep up (or the link is slow):
                        // skip ahead and restart from a keyframe.
                        agent.shared.control.force_keyframe.store(true, Ordering::Relaxed);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(reply) = handle_client_message(&agent, text.as_str()) {
                            if sink.send(Message::Text(reply.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
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
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

pub(crate) fn handle_client_message(agent: &Agent, text: &str) -> Option<String> {
    let message: ClientMsg = serde_json::from_str(text).ok()?;
    match message {
        ClientMsg::Ping { ts } => {
            let pong = ServerMsg::Pong {
                ts,
                server_us: agent.shared.capture_stats.epoch.elapsed().as_micros() as u64,
            };
            Some(serde_json::to_string(&pong).unwrap_or_default())
        }
        ClientMsg::Input { event } => {
            apply_input(agent, event);
            None
        }
        ClientMsg::Keyframe => {
            agent.shared.control.force_keyframe.store(true, Ordering::Relaxed);
            None
        }
        ClientMsg::Bitrate { bitrate } => {
            let clamped = bitrate.clamp(500_000, 80_000_000);
            agent.shared.control.bitrate.store(clamped, Ordering::Relaxed);
            None
        }
        ClientMsg::Monitor { index } => {
            if index < agent.shared.monitors.len() {
                agent.switch_monitor(index);
                return Some(hello(agent));
            }
            None
        }
    }
}

fn apply_input(agent: &Agent, event: InputEvent) {
    match event {
        InputEvent::Move { x, y } => {
            let rect = agent.shared.state.read().monitor_rect;
            input::move_abs(rect, x, y);
        }
        InputEvent::MoveRel { dx, dy } => input::move_rel(dx, dy),
        InputEvent::Button { button, down } => input::button(button, down),
        InputEvent::Wheel { dx, dy } => input::wheel(dx, dy),
        InputEvent::Key { code, down } => input::key(&code, down),
        InputEvent::Text { text } => {
            if text.len() <= 8192 {
                input::text(&text);
            }
        }
    }
}
