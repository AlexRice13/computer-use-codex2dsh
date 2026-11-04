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

/// Byte-exact port of helper_transport.js's marker path builder:
/// join(codexHome, "cache", "computer-use", "interrupts", san(session), san(turn)).
fn marker_file(home: &str, session: &str, turn: &str) -> std::path::PathBuf {
    std::path::Path::new(home)
        .join("cache")
        .join("computer-use")
        .join("interrupts")
        .join(sanitize(session))
        .join(sanitize(turn))
}

/// Write the (empty) interrupt marker for `key` ("<session>\0<turn>"), if
/// well-formed. Returns the marker path when one was written.
fn marker_from_key(home: &str, key: &str) -> Option<std::path::PathBuf> {
    let (session, turn) = key.split_once('\0')?;
    if session.is_empty() || turn.is_empty() {
        return None;
    }
    let path = marker_file(home, session, turn);
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, "").ok()?;
    Some(path)
}

/// Write the (empty) interrupt marker for the current turn scope, if any.
/// Returns the marker path when one was written.
fn write_marker() -> Option<std::path::PathBuf> {
    let key = TURN_KEY.lock().unwrap().clone()?;
    let home = std::env::var("CODEX_HOME").ok()?;
    marker_from_key(&home, &key)
}

fn write_marker_and_exit() -> ! {
    let _ = write_marker();
    std::process::exit(130);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_matches_js_regex() {
        // sanitize(t) = t.replace(/[^A-Za-z0-9._-]/g, "_")
        assert_eq!(sanitize("abc"), "abc");
        assert_eq!(sanitize("aB3._-x"), "aB3._-x");
        assert_eq!(sanitize("a/b\\c:d*e"), "a_b_c_d_e");
        assert_eq!(sanitize("sess-id_01.uuid"), "sess-id_01.uuid");
        // Non-ASCII collapses to one underscore per char (turn ids are
        // id-like, so surrogate-pair divergence from JS cannot occur here).
        assert_eq!(sanitize("会话1"), "__1");
    }

    #[test]
    fn marker_file_layout() {
        let p = marker_file("/h", "sess/1", "tu rn");
        assert_eq!(
            p,
            std::path::Path::new("/h")
                .join("cache")
                .join("computer-use")
                .join("interrupts")
                .join("sess_1")
                .join("tu_rn")
        );
    }

    /// A scratch dir we can always write to: the sandbox denies TEMP, but
    /// next to the test binary (target/) is ours.
    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("dsh-sky-{tag}-{}", std::process::id()))
    }

    #[test]
    fn marker_from_key_writes_empty_file() {
        let home = scratch("esc");
        let _ = std::fs::remove_dir_all(&home);
        let key = format!("sess-{}/x\0turn-42!", std::process::id());
        let path = marker_from_key(home.to_str().unwrap(), &key).expect("marker written");
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert!(path
            .to_string_lossy()
            .contains(&format!("interrupts{}sess-{}_x{}turn-42_", std::path::MAIN_SEPARATOR, std::process::id(), std::path::MAIN_SEPARATOR)));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn marker_from_key_rejects_malformed() {
        let home = scratch("esc-bad");
        assert!(marker_from_key(home.to_str().unwrap(), "").is_none());
        assert!(marker_from_key(home.to_str().unwrap(), "no-null").is_none());
        assert!(marker_from_key(home.to_str().unwrap(), "\0turn").is_none());
        assert!(marker_from_key(home.to_str().unwrap(), "sess\0").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }
}
