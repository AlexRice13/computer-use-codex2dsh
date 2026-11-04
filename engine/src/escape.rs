// Physical-Escape interruption: a low-level keyboard hook watches for ESC.
// On detection we write the interrupt marker for the current turn scope
// (CODEX_HOME\cache\computer-use\interrupts\<session>\<turn>) and exit 130,
// exactly as the original helper does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{LRESULT, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
    KBDLLHOOKSTRUCT, MSG, PM_REMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, HHOOK,
};

static ESC_SEEN: AtomicBool = AtomicBool::new(false);
static TURN_KEY: Mutex<Option<String>> = Mutex::new(None);

/// Update the current turn key ("<session>\0<turn>") from request meta.
pub fn set_turn(key: &str) {
    *TURN_KEY.lock().unwrap() = Some(key.to_string());
}

fn sanitize(t: &str) -> String {
    t.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

unsafe extern "system" fn hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;
        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if kb.vkCode as u32 == VK_ESCAPE.0 as u32 {
                ESC_SEEN.store(true, Ordering::SeqCst);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn write_marker_and_exit() -> ! {
    let key = TURN_KEY.lock().unwrap().clone();
    if let Some(key) = key {
        let (session, turn) = key.split_once('\0').unwrap_or((key.as_str(), ""));
        if !session.is_empty() && !turn.is_empty() {
            if let Ok(home) = std::env::var("CODEX_HOME") {
                let dir = std::path::Path::new(&home)
                    .join("cache")
                    .join("computer-use")
                    .join("interrupts")
                    .join(sanitize(session));
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join(sanitize(turn)), "");
            }
        }
    }
    std::process::exit(130);
}

/// Install the ESC watchdog hook on a dedicated pumping thread.
pub fn start() {
    std::thread::spawn(|| unsafe {
        let _hook: HHOOK = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if ESC_SEEN.load(Ordering::SeqCst) {
                write_marker_and_exit();
            }
            // Peek + short sleep keeps the hook responsive without blocking.
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}
