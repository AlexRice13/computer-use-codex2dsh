// 物理 Escape 打断：底层键盘钩子盯着 ESC。发现之后，就给当前 turn 的作用范围
// 写打断标记（CODEX_HOME\cache\computer-use\interrupts\<session>\<turn>），
// 然后 exit 130，跟原版 helper 一模一样。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{LRESULT, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
    KBDLLHOOKSTRUCT, MSG, PM_REMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, HHOOK,
    LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED,
};

static ESC_SEEN: AtomicBool = AtomicBool::new(false);
static TURN_KEY: Mutex<Option<String>> = Mutex::new(None);

/// 本插件每次 SendInput 都把这个 "SKY_ESC" 哨兵值写进 dwExtraInfo（见 win::input）。
/// 有些驱动不上报 LLKHF_INJECTED，看门狗就靠这个记号认出是自己注入的事件；
/// 顺便也等于向第三方钩子声明“这些是我们发的”。
pub const SYNTH_INPUT_MARKER: usize = 0x534B_595F_4553_43;

/// 只有用户真的按下*物理* Escape 时才为 true。
///
/// SendInput 注入的按键事件（我们自己的 press_key/type_text，还有别的自动化）
/// 一定带 LLKHF_INJECTED；从更低完整性级别注入的带 LLKHF_LOWER_IL_INJECTED。
/// 早先认了这些事件，结果 agent 自己的 `press_key Escape`（关对话框很常见）
/// 会被报成“the user stopped Computer Use with the physical Escape key”。
/// dwExtraInfo 里的哨兵是第二道戳，注入标志丢了也能认出自己的事件。
fn is_physical_escape(vk: u32, flags: u32, extra_info: usize) -> bool {
    if vk != VK_ESCAPE.0 as u32 {
        return false;
    }
    if flags & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) != 0 {
        return false;
    }
    extra_info != SYNTH_INPUT_MARKER
}

/// 用请求里的 meta 更新当前 turn 的 key（"<session>\0<turn>"）。
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

/// 跟 helper_transport.js 里拼标记路径的写法一字节不差：
/// join(codexHome, "cache", "computer-use", "interrupts", san(session), san(turn))。
fn marker_file(home: &str, session: &str, turn: &str) -> std::path::PathBuf {
    std::path::Path::new(home)
        .join("cache")
        .join("computer-use")
        .join("interrupts")
        .join(sanitize(session))
        .join(sanitize(turn))
}

/// 给 `key`（"<session>\0<turn>"）写一个空的打断标记，key 格式不对就不写。
/// 写了就把标记路径返回。
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

/// 当前 turn 有作用域的话，给它写一个空的打断标记。写了就把标记路径返回。
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
            if is_physical_escape(kb.vkCode, kb.flags.0, kb.dwExtraInfo) {
                ESC_SEEN.store(true, Ordering::SeqCst);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// 起一条专门的线程，在里面装 ESC 钩子并跑消息泵。
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
            // 先 Peek 再短睡，钩子照样跟得上，也不会堵住。
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: u32 = 0x1B;
    const ENTER: u32 = 0x0D;

    #[test]
    fn physical_escape_interrupts() {
        assert!(is_physical_escape(ESC, 0, 0));
        // 别的标志位（扩展键、上一次按下）不该影响判断。
        assert!(is_physical_escape(ESC, 0x01 | 0x20, 0));
    }

    #[test]
    fn injected_escape_never_interrupts() {
        // LLKHF_INJECTED：SendInput 注入的（press_key Escape、type_text、自动化）。
        assert!(!is_physical_escape(ESC, LLKHF_INJECTED.0, 0));
        // LLKHF_LOWER_IL_INJECTED：从更低完整性级别注入的。
        assert!(!is_physical_escape(ESC, LLKHF_LOWER_IL_INJECTED.0, 0));
        assert!(!is_physical_escape(ESC, LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0, 0));
    }

    #[test]
    fn own_sentinel_escape_never_interrupts() {
        // 标志位缺失时，自己注入的事件靠这道兜底。
        assert!(!is_physical_escape(ESC, 0, SYNTH_INPUT_MARKER));
        // 哨兵值对不上，就不是我们发的。
        assert!(is_physical_escape(ESC, 0, SYNTH_INPUT_MARKER ^ 0xFF));
    }

    #[test]
    fn other_keys_never_interrupt() {
        assert!(!is_physical_escape(ENTER, 0, 0));
        assert!(!is_physical_escape(ENTER, LLKHF_INJECTED.0, 0));
    }

    #[test]
    fn sanitize_matches_js_regex() {
        // sanitize(t) = t.replace(/[^A-Za-z0-9._-]/g, "_")
        assert_eq!(sanitize("abc"), "abc");
        assert_eq!(sanitize("aB3._-x"), "aB3._-x");
        assert_eq!(sanitize("a/b\\c:d*e"), "a_b_c_d_e");
        assert_eq!(sanitize("sess-id_01.uuid"), "sess-id_01.uuid");
        // 非 ASCII 字符一个换一个下划线（turn id 都是 id 形态，碰不到 JS 那种
        // 代理对拆开的问题）。
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

    /// 找个肯定写得进去的目录：沙箱不给写 TEMP，测试二进制旁边（target/）是
    /// 自己的地盘。
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
