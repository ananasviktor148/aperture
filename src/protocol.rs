//! Wire protocol shared by the agent and the browser viewer.
//!
//! Control traffic is JSON text; video frames are binary with an 8-byte header.

use serde::{Deserialize, Serialize};

use crate::display::MonitorInfo;
use crate::input::MouseButton;

pub const FRAME_MAGIC: u8 = 1;
pub const HEADER_LEN: usize = 8;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg<'a> {
    Hello {
        width: u32,
        height: u32,
        desktop_width: u32,
        desktop_height: u32,
        fps: u32,
        bitrate: u32,
        encoder: &'a str,
        monitor: usize,
        monitors: &'a [MonitorInfo],
    },
    Stats {
        fps: f32,
        capture_fps: f32,
        kbps: u32,
        encode_us: u32,
        clients: usize,
    },
    Pong {
        ts: u64,
        server_us: u64,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Ping {
        ts: u64,
    },
    Input {
        #[serde(flatten)]
        event: InputEvent,
    },
    Keyframe,
    Bitrate {
        bitrate: u32,
    },
    Monitor {
        index: usize,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InputEvent {
    Move { x: f64, y: f64 },
    MoveRel { dx: f64, dy: f64 },
    Button { button: MouseButton, down: bool },
    Wheel { #[serde(default)] dx: i32, #[serde(default)] dy: i32 },
    Key { code: String, down: bool },
    Text { text: String },
}

/// Builds the binary frame packet: magic, flags, encode time (ms), capture
/// time (ms since agent start), Annex B payload. Milliseconds keep the capture
/// stamp unambiguous far beyond a 32-bit microsecond counter.
pub fn encode_frame(keyframe: bool, capture_ms: u32, encode_ms: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.push(FRAME_MAGIC);
    out.push(if keyframe { 1 } else { 0 });
    out.extend_from_slice(&encode_ms.to_le_bytes());
    out.extend_from_slice(&capture_ms.to_le_bytes());
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_roundtrip() {
        let packet = encode_frame(true, 0xDEAD_BEEF, 7, &[9, 8, 7]);
        assert_eq!(packet[0], FRAME_MAGIC);
        assert_eq!(packet[1], 1);
        assert_eq!(u16::from_le_bytes(packet[2..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(packet[4..8].try_into().unwrap()), 0xDEAD_BEEF);
        assert_eq!(&packet[8..], &[9, 8, 7]);
    }

    #[test]
    fn parses_input_messages() {
        let msg: ClientMsg = serde_json::from_str(r#"{"type":"input","kind":"move","x":0.5,"y":0.25}"#).unwrap();
        match msg {
            ClientMsg::Input { event: InputEvent::Move { x, y } } => {
                assert_eq!((x, y), (0.5, 0.25));
            }
            _ => panic!("wrong variant"),
        }

        let msg: ClientMsg =
            serde_json::from_str(r#"{"type":"input","kind":"button","button":"right","down":true}"#)
                .unwrap();
        assert!(matches!(
            msg,
            ClientMsg::Input { event: InputEvent::Button { button: MouseButton::Right, down: true } }
        ));
    }
}
