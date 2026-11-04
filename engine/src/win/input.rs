// Synthetic input via SendInput: typing, key combinations, mouse moves,
// clicks, wheel and drag. Coordinates arrive already in screen space
// (the tool layer adds the snapshot origin before calling in).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SetForegroundWindow, ShowWindow, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE,
};

/// Bring a window forward before delivering input (the harness contract
/// requires an explicit activate; helpers on top still call this defensively).
pub fn activate(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn normalized(x: i32, y: i32) -> (i32, i32) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2);
        let nx = (((x - vx) as f64 / (vw as f64 - 1.0)) * 65535.0).round() as i32;
        let ny = (((y - vy) as f64 / (vh as f64 - 1.0)) * 65535.0).round() as i32;
        (nx.clamp(0, 65535), ny.clamp(0, 65535))
    }
}

fn mouse(x: i32, y: i32, flags: MOUSE_EVENT_FLAGS, data: u32) {
    let (nx, ny) = normalized(x, y);
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: nx,
                dy: ny,
                mouseData: data,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: usize::default(),
            },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn key(vk: u16, flags: KEYBD_EVENT_FLAGS, scan: u16) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: usize::default(),
            },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// Type literal text: one VK_PACKET event per UTF-16 code unit.
pub fn type_text(text: &str) -> usize {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut sent = 0usize;
    for u in units {
        let down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: u,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: usize::default(),
                },
            },
        };
        let up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: u,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: usize::default(),
                },
            },
        };
        unsafe {
            if SendInput(&[down, up], std::mem::size_of::<INPUT>() as i32) == 2 {
                sent += 1;
            }
        }
    }
    sent
}

const MODIFIERS: &[(&str, u16)] = &[
    ("ctrl", 0x11),
    ("control", 0x11),
    ("alt", 0x12),
    ("option", 0x12),
    ("shift", 0x10),
    ("cmd", 0x5B),
    ("command", 0x5B),
    ("meta", 0x5B),
    ("super", 0x5B),
    ("win", 0x5B),
    ("windows", 0x5B),
];

const NAMED_KEYS: &[(&str, u16)] = &[
    ("enter", 0x0D),
    ("return", 0x0D),
    ("escape", 0x1B),
    ("esc", 0x1B),
    ("tab", 0x09),
    ("backspace", 0x08),
    ("delete", 0x2E),
    ("del", 0x2E),
    ("insert", 0x2D),
    ("up", 0x26),
    ("down", 0x28),
    ("left", 0x25),
    ("right", 0x27),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("prior", 0x21),
    ("pagedown", 0x22),
    ("next", 0x22),
    ("space", 0x20),
    ("capslock", 0x14),
    ("printscreen", 0x2C),
    ("snapshot", 0x2C),
    ("menu", 0x12),
];

fn vk_for_token(tok: &str) -> Option<u16> {
    if tok.is_empty() {
        return None;
    }
    if let Some((_, vk)) = NAMED_KEYS.iter().find(|(n, _)| *n == tok) {
        return Some(*vk);
    }
    if let Some((_, vk)) = MODIFIERS.iter().find(|(n, _)| *n == tok) {
        return Some(*vk);
    }
    if let Some(rest) = tok.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some((0x70 + n - 1) as u16);
            }
            return None;
        }
    }
    let chars: Vec<char> = tok.chars().collect();
    if chars.len() == 1 {
        let c = chars[0];
        // The virtual-key codes for A-Z and 0-9 are their ASCII codes, so
        // alphanumeric keys need no keyboard-layout lookup at all.
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase() as u16);
        }
        if c.is_ascii_digit() {
            return Some(c as u16);
        }
        // Symbols route through VkKeyScanW with the thread layout.
        let code = {
            let mut buf = [0u16; 2];
            c.encode_utf16(&mut buf);
            buf[0]
        };
        unsafe {
            let scan = windows::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW(code);
            if scan >= 0 {
                return Some((scan as u32 & 0xFF) as u16);
            }
        }
        return None;
    }
    None
}

fn is_modifier_token(tok: &str) -> bool {
    let t = tok.to_lowercase();
    MODIFIERS.iter().any(|(n, _)| *n == t)
}

/// Press a key or combination: "ctrl+shift+s", "Enter", "F5", "space".
pub fn press_key(combo: &str) -> Result<(), String> {
    let toks: Vec<String> = combo
        .split('+')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if toks.is_empty() {
        return Err("invalid key combination".into());
    }
    // The last token is the action key; leading tokens are modifiers.
    let mut mods: Vec<u16> = Vec::new();
    let mut action: Option<u16> = None;
    for (i, t) in toks.iter().enumerate() {
        let vk = vk_for_token(t).ok_or_else(|| format!("unknown key: {t}"))?;
        if i + 1 == toks.len() && !is_modifier_token(t) {
            action = Some(vk);
        } else if is_modifier_token(t) {
            if !mods.contains(&vk) {
                mods.push(vk);
            }
        } else {
            // Non-modifier in the middle: still treat as part of the chord.
            action = action.or(Some(vk));
        }
    }
    for m in &mods {
        key(*m, KEYBD_EVENT_FLAGS(0), 0);
    }
    if let Some(a) = action {
        key(a, KEYBD_EVENT_FLAGS(0), 0);
        key(a, KEYEVENTF_KEYUP, 0);
    }
    for m in mods.iter().rev() {
        key(*m, KEYEVENTF_KEYUP, 0);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

pub fn click(x: i32, y: i32, button: Button, count: u32) {
    let (down, up) = match button {
        Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };
    mouse(x, y, MOUSEEVENTF_MOVE, 0);
    for _ in 0..count.max(1) {
        mouse(x, y, down, 0);
        mouse(x, y, up, 0);
    }
}

pub fn wheel(x: i32, y: i32, scroll_x: i32, scroll_y: i32) {
    // Move the cursor first so the wheel events target the right surface.
    mouse(x, y, MOUSEEVENTF_MOVE, 0);
    if scroll_y != 0 {
        mouse(x, y, MOUSEEVENTF_WHEEL, (scroll_y.saturating_mul(120)) as u32);
    }
    if scroll_x != 0 {
        mouse(x, y, MOUSEEVENTF_HWHEEL, (scroll_x.saturating_mul(120)) as u32);
    }
}

pub fn drag(from: (i32, i32), to: (i32, i32)) {
    mouse(from.0, from.1, MOUSEEVENTF_MOVE, 0);
    mouse(from.0, from.1, MOUSEEVENTF_LEFTDOWN, 0);
    // Intermediate moves help targets that track drag via WM_MOUSEMOVE.
    let steps = 12;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let x = (from.0 as f64 + (to.0 - from.0) as f64 * t).round() as i32;
        let y = (from.1 as f64 + (to.1 - from.1) as f64 * t).round() as i32;
        mouse(x, y, MOUSEEVENTF_MOVE, 0);
    }
    mouse(to.0, to.1, MOUSEEVENTF_LEFTUP, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_and_function_keys() {
        assert_eq!(vk_for_token("enter"), Some(0x0D));
        assert_eq!(vk_for_token("escape"), Some(0x1B));
        assert_eq!(vk_for_token("esc"), Some(0x1B));
        assert_eq!(vk_for_token("tab"), Some(0x09));
        assert_eq!(vk_for_token("backspace"), Some(0x08));
        assert_eq!(vk_for_token("delete"), Some(0x2E));
        assert_eq!(vk_for_token("up"), Some(0x26));
        assert_eq!(vk_for_token("down"), Some(0x28));
        assert_eq!(vk_for_token("left"), Some(0x25));
        assert_eq!(vk_for_token("right"), Some(0x27));
        assert_eq!(vk_for_token("space"), Some(0x20));
        assert_eq!(vk_for_token("f1"), Some(0x70));
        assert_eq!(vk_for_token("f12"), Some(0x7B));
        assert_eq!(vk_for_token("f24"), Some(0x87));
        assert_eq!(vk_for_token("f25"), None);
        assert_eq!(vk_for_token("f0"), None);
    }

    #[test]
    fn characters_map_through_vk_key_scan() {
        assert_eq!(vk_for_token("a"), Some(0x41));
        assert_eq!(vk_for_token("Z"), Some(0x5A));
        assert_eq!(vk_for_token("5"), Some(0x35));
        assert_eq!(vk_for_token(""), None);
    }

    #[test]
    fn modifier_tokens() {
        assert!(is_modifier_token("ctrl"));
        assert!(is_modifier_token("SHIFT"));
        assert!(is_modifier_token("alt"));
        assert!(!is_modifier_token("enter"));
        assert!(!is_modifier_token("a"));
    }

    #[test]
    fn combo_rejects_unknown_key_without_injecting() {
        // The parser runs before any SendInput call, so an unknown token is
        // an error and nothing reaches the desktop.
        assert!(press_key("ctrl+not-a-key").is_err());
        assert!(press_key("").is_err());
    }
}
