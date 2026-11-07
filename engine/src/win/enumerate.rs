use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowLongPtrW, GetWindowThreadProcessId, IsWindowVisible,
    GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

use super::com::ComGuard;
use super::identity;

/// 解析好的一个顶层窗口，字段形状照着原版 helper 报的来。
#[derive(Clone)]
pub struct WindowRecord {
    pub app: String,
    pub hwnd: isize,
    pub title: String,
    #[allow(dead_code)] // 留着做诊断，以后也可能按 pid 查
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

/// 列出可见、没被 cloaked、身份又解析得出来的顶层窗口
/// （规范 §C.1：pid 0、cloaked、不可见、解析不出的全跳过）。
pub fn enumerate_top_level() -> Vec<WindowRecord> {
    // 查 AUMID 要用 shell 的属性存储，这个线程上得先有 COM。
    let _com = ComGuard::new();

    // 第一遍：原样全收，什么条件都不加。这样解析身份时还看得见 UWP 的内容
    // 窗口——它们可能被 cloaked，也可能因为别的条件被筛掉。
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

    // 第二遍：先过滤，再解析身份。
    let mut out = Vec::new();
    for w in &raw {
        if w.pid == 0 || !w.visible || w.cloaked || w.toolwindow {
            continue;
        }
        if DESKTOP_CLASSES.contains(&w.class.as_str()) {
            continue;
        }
        // UWP 的内容窗口是挂在它的 frame 上露出来的，不单独列一条
        // （原版 helper 从来不会把它们单列）。
        if UWP_CONTENT_CLASSES.contains(&w.class.as_str()) {
            continue;
        }
        let mut app = identity::app_id_for_window(w.hwnd, w.pid);
        if app.is_none() {
            // ApplicationFrameHost 的框：AUMID 属于框后面那个
            // Windows.UI.Core 内容窗口所在的打包应用进程。
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

/// 只在调试时用（--wins）：EnumWindows 拿到的每一条都原样打出来，带上过滤
/// 用的字段和身份解析结果，专门查枚举结果对不上的问题。
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
