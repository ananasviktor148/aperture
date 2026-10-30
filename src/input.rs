//! Input injection through `SendInput`.
//!
//! Mouse coordinates are normalized per-monitor and translated into virtual
//! desktop absolute coordinates, so multi-monitor setups behave correctly.

use serde::Deserialize;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSEINPUT, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};

use crate::display::{virtual_screen, Rect};

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

pub fn move_abs(rect: Rect, nx: f64, ny: f64) {
    let vs = virtual_screen();
    if vs.width <= 1 || vs.height <= 1 {
        return;
    }
    let sx = rect.x as f64 + nx.clamp(0.0, 1.0) * (rect.width.max(2) - 1) as f64;
    let sy = rect.y as f64 + ny.clamp(0.0, 1.0) * (rect.height.max(2) - 1) as f64;
    let ax = ((sx - vs.x as f64) * 65535.0 / (vs.width - 1) as f64).round() as i32;
    let ay = ((sy - vs.y as f64) * 65535.0 / (vs.height - 1) as f64).round() as i32;
    send(&[mouse_input(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        ax,
        ay,
        0,
    )]);
}

pub fn move_rel(dx: f64, dy: f64) {
    let dx = dx.round() as i32;
    let dy = dy.round() as i32;
    if dx == 0 && dy == 0 {
        return;
    }
    send(&[mouse_input(MOUSEEVENTF_MOVE, dx, dy, 0)]);
}

pub fn button(button: MouseButton, down: bool) {
    let (flag, data) = match (button, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, 1),
        (MouseButton::Back, false) => (MOUSEEVENTF_XUP, 1),
        (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
        (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, 2),
    };
    send(&[mouse_input(flag, 0, 0, data)]);
}

/// `dy`/`dx` are in wheel ticks (120 per notch on Windows).
pub fn wheel(dx: i32, dy: i32) {
    let mut events = Vec::with_capacity(2);
    if dy != 0 {
        events.push(mouse_input(MOUSEEVENTF_WHEEL, 0, 0, dy * 120));
    }
    if dx != 0 {
        events.push(mouse_input(MOUSEEVENTF_HWHEEL, 0, 0, dx * 120));
    }
    send(&events);
}

fn is_extended(vk: u16) -> bool {
    matches!(
        vk,
        0x2D | 0x2E | 0x21 | 0x22 | 0x23 | 0x24 | 0x25 | 0x26 | 0x27 | 0x28 | 0x5B | 0x5C
            | 0x5D | 0x6F | 0x90 | 0x2C | 0xA3 | 0xA5
    )
}

pub fn vk_from_code(code: &str) -> Option<u16> {
    let vk = match code {
        "Escape" => 0x1B,
        "Tab" => 0x09,
        "CapsLock" => 0x14,
        "Space" => 0x20,
        "Enter" => 0x0D,
        "Backspace" => 0x08,
        "Delete" => 0x2E,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "PrintScreen" => 0x2C,
        "ScrollLock" => 0x91,
        "Pause" => 0x13,
        "Minus" => 0xBD,
        "Equal" => 0xBB,
        "BracketLeft" => 0xDB,
        "BracketRight" => 0xDD,
        "Backslash" => 0xDC,
        "Semicolon" => 0xBA,
        "Quote" => 0xDE,
        "Backquote" => 0xC0,
        "Comma" => 0xBC,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "IntlBackslash" => 0xE2,
        "ShiftLeft" => 0xA0,
        "ShiftRight" => 0xA1,
        "ControlLeft" => 0xA2,
        "ControlRight" => 0xA3,
        "AltLeft" => 0xA4,
        "AltRight" => 0xA5,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "ContextMenu" => 0x5D,
        "NumLock" => 0x90,
        "NumpadAdd" => 0x6B,
        "NumpadSubtract" => 0x6D,
        "NumpadMultiply" => 0x6A,
        "NumpadDivide" => 0x6F,
        "NumpadDecimal" => 0x6E,
        "NumpadEnter" => 0x0D,
        _ => {
            if let Some(rest) = code.strip_prefix("Key") {
                let ch = rest.as_bytes();
                if ch.len() == 1 && ch[0].is_ascii_uppercase() {
                    return Some(ch[0] as u16);
                }
            }
            if let Some(rest) = code.strip_prefix("Digit") {
                if rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit() {
                    return Some(0x30 + (rest.as_bytes()[0] - b'0') as u16);
                }
            }
            if let Some(rest) = code.strip_prefix("Numpad") {
                if rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit() {
                    return Some(0x60 + (rest.as_bytes()[0] - b'0') as u16);
                }
            }
            if let Some(rest) = code.strip_prefix('F') {
                if let Ok(n) = rest.parse::<u16>() {
                    if (1..=24).contains(&n) {
                        return Some(0x6F + n);
                    }
                }
            }
            return None;
        }
    };
    Some(vk)
}

pub fn key(code: &str, down: bool) {
    let Some(vk) = vk_from_code(code) else {
        return;
    };
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags = KEYEVENTF_SCANCODE;
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: if scan == 0 { VIRTUAL_KEY(vk) } else { VIRTUAL_KEY(0) },
                wScan: scan,
                dwFlags: KEYBD_EVENT_FLAGS(flags.0),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send(&[input]);
}

/// Types text as Unicode key events (layout-independent, ideal for phone keyboards).
pub fn text(value: &str) {
    let mut events = Vec::with_capacity(value.encode_utf16().count() * 2);
    for unit in value.encode_utf16() {
        for down in [true, false] {
            let mut flags = KEYEVENTF_UNICODE;
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            events.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYBD_EVENT_FLAGS(flags.0),
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }
    send(&events);
}
