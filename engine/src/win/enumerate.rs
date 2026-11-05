use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowLongPtrW, GetWindowThreadProcessId, IsWindowVisible,
    GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

use super::com::ComGuard;
use super::identity;

/// A resolved top-level window, shaped after what the helper reports.
#[derive(Clone)]
pub struct WindowRecord {
    pub app: String,
    pub hwnd: isize,
    pub title: String,
    #[allow(dead_code)] // kept for diagnostics / future per-pid queries
    pub pid: u32,
    #[allow(dead_code)]
    pub bounds: Option<RECT>,
}

struct RawWin {
    hwnd: windows::Win32::Foundation::HWND,
    pid: u32,
    class: String,
    title: String,
    visible: bool,
    cloaked: bool,
    toolwindow: bool,
}

const DESKTOP_CLASSES: &[&str] = &["Progman", "WorkerW", "Shell_TrayWnd"];
const UWP_CONTENT_CLASSES: &[&str] = &["Windows.UI.Core.CoreWindow", "Windows.UI.Core.AppWindow"];
const FRAME_HOST_EXE: &str = "applicationframehost.exe";

/// Enumerate visible, non-cloaked top-level windows with a resolvable
/// identity (spec §C.1: skip pid 0, cloaked, invisible, unresolvable).
pub fn enumerate_top_level() -> Vec<WindowRecord> {
    // The shell property store (AUMID lookup) requires COM on this thread.
    let _com = ComGuard::new();

    // Pass 1: raw collection (no filters) so identity resolution can see the
    // UWP content windows (which may be cloaked or otherwise unfilters).
    unsafe extern "system" fn cb(hwnd: windows::Win32::Foundation::HWND, lparam: LPARAM) -> windows::core::BOOL {
        let out = &mut *(lparam.0 as *mut Vec<RawWin>);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut cls = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut cls);
        let class = if n > 0 { String::from_utf16_lossy(&cls[..n as usize]) } else { String::new() };
        out.push(RawWin {
            hwnd,
            pid,
            class,
            title: identity::window_title(hwnd),
            visible: IsWindowVisible(hwnd).as_bool(),
            cloaked: identity::is_cloaked(hwnd),
            toolwindow: (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize) != 0,
        });
        true.into()
    }
    let mut raw: Vec<RawWin> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut raw as *mut _ as isize));
    }

    // Pass 2: filter + resolve identity.
    let mut out = Vec::new();
    for w in &raw {
        if w.pid == 0 || !w.visible || w.cloaked || w.toolwindow {
            continue;
        }
        if DESKTOP_CLASSES.contains(&w.class.as_str()) {
            continue;
        }
        // UWP content windows are surfaced through their frame, not listed
        // standalone (the real helper never lists them separately).
        if UWP_CONTENT_CLASSES.contains(&w.class.as_str()) {
            continue;
        }
        let mut app = identity::app_id_for_window(w.hwnd, w.pid);
        if app.is_none() {
            // ApplicationFrameHost frame: the AUMID belongs to the packaged
            // app process behind the owned Windows.UI.Core content window.
            let is_frame_host = identity::process_image_path(w.pid)
                .map(|p| {
                    p.rsplit('\\')
                        .next()
                        .unwrap_or("")
                        .eq_ignore_ascii_case(FRAME_HOST_EXE)
                })
                .unwrap_or(false);
            if is_frame_host {
                let trace = std::env::var("SKY_TRACE").is_ok();
                for c in &raw {
                    if c.pid != w.pid
                        && UWP_CONTENT_CLASSES.contains(&c.class.as_str())
                        && !w.title.trim().is_empty()
                        && c.title == w.title
                    {
                        if trace {
                            eprintln!("[trace] frame-host candidate pid={} class={} title={:?}", c.pid, c.class, c.title);
                        }
                        if let Some(a) = identity::aumid_for_hosted_pid(c.pid) {
                            if trace {
                                eprintln!("[trace] frame-host aumid={a}");
                            }
                            app = Some(a);
                            break;
                        }
                    }
                }
            }
        }
        let Some(app) = app else { continue };
        if w.title.trim().is_empty() {
            continue;
        }
        out.push(WindowRecord {
            app,
            hwnd: w.hwnd.0 as isize,
            title: w.title.clone(),
            pid: w.pid,
            bounds: identity::extended_bounds(w.hwnd),
        });
    }
    out
}

/// Debug-only (--wins): every raw EnumWindows entry with its filter fields
/// and identity resolution, for diagnosing enumeration divergences.
pub fn debug_windows() -> Vec<(isize, u32, String, String, bool, bool, bool, Option<String>)> {
    let _com = ComGuard::new();
    unsafe extern "system" fn cb(hwnd: windows::Win32::Foundation::HWND, lparam: LPARAM) -> windows::core::BOOL {
        let out = &mut *(lparam.0 as *mut Vec<RawWin>);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut cls = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut cls);
        let class = if n > 0 { String::from_utf16_lossy(&cls[..n as usize]) } else { String::new() };
        out.push(RawWin {
            hwnd,
            pid,
            class,
            title: identity::window_title(hwnd),
            visible: IsWindowVisible(hwnd).as_bool(),
            cloaked: identity::is_cloaked(hwnd),
            toolwindow: (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize) != 0,
        });
        true.into()
    }
    let mut raw: Vec<RawWin> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut raw as *mut _ as isize));
    }
    raw.into_iter()
        .map(|w| {
            let app = identity::app_id_for_window(w.hwnd, w.pid);
            (w.hwnd.0 as isize, w.pid, w.class, w.title, w.visible, w.cloaked, w.toolwindow, app)
        })
        .collect()
}
