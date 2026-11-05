// launch_app: the original accepts exactly two shapes — an installed app id
// (AUMID) from list_apps, or an absolute executable path. Error texts are
// byte-exact with the original helper. Approval gates the actual start
// (dispatch calls execute() only after approval).

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::System::Com::CoCreateInstance;
use windows::Win32::UI::Shell::{IApplicationActivationManager, ShellExecuteW};

const REQUIRES_ID: &str =
    "launch_app requires an installed app id from list_apps or a launchable executable identifier";

// CLSID_ApplicationActivationManager {2E941141-7F97-4756-BA1D-9DECDE894A3D}
const CLSID_AA_MANAGER: GUID = GUID::from_u128(0x2E941141_7F97_4756_BA1D_9DECDE894A3D);

/// What the caller asked to launch, after validation.
#[derive(Debug, Clone)]
pub enum Plan {
    Aumid(String),
    ExePath(String),
}

fn looks_like_aumid(s: &str) -> bool {
    (s.contains('_') && s.contains('!')) || s.starts_with("windows.")
}

fn has_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// Validate and classify `app` (no side effects). Runs before approval, so
/// malformed requests never pop an approval dialog (matches the original).
pub fn plan(app: &str) -> Result<Plan, String> {
    let app = app.trim();
    if app.is_empty() {
        return Err("app is required".into());
    }
    // Numeric identifiers read as pids.
    if app.parse::<i64>().is_ok() {
        return Err("launch_app does not support pid app identifiers".into());
    }
    let lower = app.to_lowercase();
    // shell: verbs are not accepted even though they look launchable.
    if lower.starts_with("shell:") || lower.starts_with("app-user-model-id:") {
        return Err(REQUIRES_ID.into());
    }
    // Drive-qualified first (paths may contain _ and !).
    if has_drive_prefix(app) || app.starts_with("\\\\") {
        if !std::path::Path::new(app).is_absolute() {
            return Err("app policy requires an absolute executable path".into());
        }
        if !std::path::Path::new(app).exists() {
            return Err(REQUIRES_ID.into());
        }
        return Ok(Plan::ExePath(app.to_string()));
    }
    if looks_like_aumid(app) {
        return Ok(Plan::Aumid(app.to_string()));
    }
    // Anything else (bare names like "notepad", relative "*.exe") is rejected:
    // the original never resolves names through the shell.
    if lower.ends_with(".exe") || app.contains('\\') {
        return Err("app policy requires an absolute executable path".into());
    }
    Err(REQUIRES_ID.into())
}

/// Identity string used for the approval gate.
pub fn plan_owner(plan: &Plan) -> String {
    match plan {
        Plan::Aumid(a) => a.clone(),
        Plan::ExePath(p) => format!("process:{p}"),
    }
}

/// Start the planned app (post-approval). The original verifies the launch
/// actually produced a targetable window: it waits after activation and
/// fails with "launched app did not expose a targetable window: {owner}"
/// when none appears (measured against the original helper).
///
/// Both launch channels run on a dedicated STA thread: the shell-side COM
/// classes (ApplicationActivationManager) are STA-only and CoCreateInstance
/// from our MTA serve thread fails with CO_E_CLASSSTRING (0x80040154),
/// while the original (a C++ helper) activates them from an STA context.
pub fn execute(plan: &Plan, budget_ms: u64) -> Result<(), String> {
    let owner = plan_owner(plan);
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let plan2 = plan.clone();
    std::thread::spawn(move || {
        let r = unsafe {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let r = match &plan2 {
                Plan::Aumid(a) => activate_aumid(a),
                Plan::ExePath(p) => start_exe(p),
            };
            // Let shell COM complete its reentrant callbacks.
            use windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
            };
            let t0 = std::time::Instant::now();
            while t0.elapsed() < std::time::Duration::from_millis(400) {
                let mut msg = MSG::default();
                if PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            use windows::Win32::System::Com::CoUninitialize;
            CoUninitialize();
            r
        };
        let _ = tx.send(r);
    });
    rx.recv().map_err(|e| format!("launch thread: {e}"))??;
    // Only the exe-path channel verifies a window: ActivateApplication
    // returning S_OK is accepted verbatim (measured: the original answers
    // OK for a packaged app whose window never appears), while ShellExecute
    // — which can silently no-op — is verified against the window table
    // (measured: System32\Notepad.exe -> "launched app did not expose a
    // targetable window: process:C:\Windows\System32\Notepad.exe").
    if let Plan::ExePath(_) = plan {
        // Wait most of the request budget (measured: the original waits
        // for the window under a cold packaged-app start). Reserve enough
        // headroom so rpc never reports budget exhaustion.
        let wait_ms = budget_ms.saturating_sub(3000).clamp(4000, 12000);
        return wait_for_window(&owner, std::time::Duration::from_millis(wait_ms));
    }
    Ok(())
}

/// Poll the raw window table until a visible window owned by `owner`
/// exists (a pre-existing window also satisfies it — relaunching a running
/// app surfaces its window and must succeed). Uses the raw window table:
/// packaged apps (Calculator) present an untitled window first, which the
/// title filter in enumerate_top_level would drop.
fn wait_for_window(owner: &str, timeout: std::time::Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // No visibility filter: packaged apps (Calculator) keep an
        // invisible/splash top-level window that the original still counts
        // as "a targetable window" (measured: OK while MainWindowHandle=0).
        let found = crate::win::enumerate::debug_windows()
            .into_iter()
            .any(|(_, _, _, _, _, _, _, app)| {
                app.as_ref()
                    .map(|a| a.eq_ignore_ascii_case(owner))
                    .unwrap_or(false)
            });
        if found {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("launched app did not expose a targetable window: {owner}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn activate_aumid(aumid: &str) -> Result<(), String> {
    unsafe {
        // Preferred channel: the shell activation server. INPROC_SERVER
        // alone fails with CO_E_CLASSSTRING in stripped environments, so
        // allow the out-of-process fallback too.
        let via_com = CoCreateInstance::<_, IApplicationActivationManager>(
            &CLSID_AA_MANAGER,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER
                | windows::Win32::System::Com::CLSCTX_LOCAL_SERVER,
        )
        .map_err(|e| format!("activation manager: {e}"))
        .and_then(|mgr| {
            let id = HSTRING::from(aumid);
            mgr.ActivateApplication(&id, None, windows::Win32::UI::Shell::AO_NONE)
                .map(|_process_key| ())
                .map_err(|e| format!("{REQUIRES_ID} ({e})"))
        });
        if via_com.is_ok() {
            return via_com;
        }
        // Fallback: ask the shell to activate the AppsFolder verb. Behaves
        // identically (activates/foregrounds the packaged app) and works
        // where direct COM activation is blocked for the process token.
        let verb_target = HSTRING::from("explorer.exe");
        let p = HSTRING::from(format!("shell:AppsFolder\\{aumid}"));
        let h = ShellExecuteW(
            None,
            PCWSTR::null(),
            PCWSTR(verb_target.as_ptr()),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
        if h.0 as isize <= 32 {
            return via_com; // report the original COM error
        }
        Ok(())
    }
}

fn start_exe(path: &str) -> Result<(), String> {
    unsafe {
        let p = HSTRING::from(path);
        let h = ShellExecuteW(
            None,
            PCWSTR::null(),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
        if h.0 as isize <= 32 {
            return Err(format!("{REQUIRES_ID} (ShellExecute={})", h.0 as isize));
        }
    }
    Ok(())
}
