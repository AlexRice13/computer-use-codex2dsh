// 合成输入都走 SendInput：打字、按键组合、鼠标移动、点击、滚轮、拖拽。
// 传进来的坐标已经是屏幕坐标（工具层调进来之前会先加上 snapshot 原点）。

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

/// 发输入之前先把窗口提到前台（harness 契约要求显式 activate；上层 helper
/// 还是会自己再调一次，防一手）。
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
                dwExtraInfo: crate::escape::SYNTH_INPUT_MARKER,
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
                dwExtraInfo: crate::escape::SYNTH_INPUT_MARKER,
            },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// 原样打字：每个 UTF-16 码元发一个 VK_PACKET 事件。
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
                    dwExtraInfo: crate::escape::SYNTH_INPUT_MARKER,
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
                    dwExtraInfo: crate::escape::SYNTH_INPUT_MARKER,
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
        // A-Z 和 0-9 的虚拟键码就是它们的 ASCII 码，所以字母数字这些键
        // 根本不用查键盘布局。
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase() as u16);
        }
        if c.is_ascii_digit() {
            return Some(c as u16);
        }
        // 符号走 VkKeyScanW，按当前线程的键盘布局查。
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

/// 按一个键或者一串组合键："ctrl+shift+s"、"Enter"、"F5"、"space"。
pub fn press_key(combo: &str) -> Result<(), String> {
    let toks: Vec<String> = combo
        .split('+')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if toks.is_empty() {
        return Err("invalid key combination".into());
    }
    // 最后一个 token 才是真正按的键，前面的都当修饰键。
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
            // 中间夹了个非修饰键：还是算在这串组合里。
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
    // 先把光标移过去，滚轮事件才会落在该落的那个面上。
    mouse(x, y, MOUSEEVENTF_MOVE, 0);
    if scroll_y != 0 {
        mouse(x, y, MOUSEEVENTF_WHEEL, (scroll_y.saturating_mul(120)) as u32);
    }
    if scroll_x != 0 {
        mouse(x, y, MOUSEEVENTF_HWHEEL, (scroll_x.saturating_mul(120)) as u32);
    }
}

/// 按下左键、移动、再松开。
///
/// 按距离插值这一步不能省：长拖拽（画图里画个形状、拉个选区）要目标窗口收到
/// 足够多的 `WM_MOUSEMOVE` 采样，而且在 `MOUSEEVENTF_LEFTUP` 之前稳一下才肯
/// 提交。以前固定走 12 步、中间不加延时，长拖拽就一直是橡皮筋预览
/// （目标根本没等到一次完整手势的抬键）。
pub fn drag(from: (i32, i32), to: (i32, i32)) {
    mouse(from.0, from.1, MOUSEEVENTF_MOVE, 0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    mouse(from.0, from.1, MOUSEEVENTF_LEFTDOWN, 0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    let dx = (to.0 - from.0) as f64;
    let dy = (to.1 - from.1) as f64;
    let distance = (dx * dx + dy * dy).sqrt();
    let steps = ((distance / 6.0).ceil() as i64).clamp(12, 96);
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        // 缓入缓出，免得首尾那几个像素被跳过。
        let e = t * t * (3.0 - 2.0 * t);
        let x = (from.0 as f64 + dx * e).round() as i32;
        let y = (from.1 as f64 + dy * e).round() as i32;
        mouse(x, y, MOUSEEVENTF_MOVE, 0);
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
    // 精确落到目标点上，再停一下，让目标先把最后一次移动处理完再抬键。
    mouse(to.0, to.1, MOUSEEVENTF_MOVE, 0);
    std::thread::sleep(std::time::Duration::from_millis(40));
    mouse(to.0, to.1, MOUSEEVENTF_LEFTUP, 0);
    std::thread::sleep(std::time::Duration::from_millis(20));
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
        // 解析在任何 SendInput 之前跑完，所以遇到不认识的 token 直接报错，
        // 一个事件都不会落到桌面上。
        assert!(press_key("ctrl+not-a-key").is_err());
        assert!(press_key("").is_err());
    }
}
