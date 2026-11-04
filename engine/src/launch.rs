// launch_app: the original accepts exactly two shapes — an installed app id
// (AUMID) from list_apps, or an absolute executable path. Error texts are
// byte-exact with the original helper. Approval gates the actual start
// (dispatch calls execute() only after approval).

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
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

/// Start the planned app (post-approval).
pub fn execute(plan: &Plan) -> Result<(), String> {
    match plan {
        Plan::Aumid(a) => activate_aumid(a),
        Plan::ExePath(p) => start_exe(p),
    }
}

fn activate_aumid(aumid: &str) -> Result<(), String> {
    unsafe {
        let mgr: IApplicationActivationManager =
            CoCreateInstance(&CLSID_AA_MANAGER, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("activation manager: {e}"))?;
        let id = HSTRING::from(aumid);
        mgr.ActivateApplication(&id, None, windows::Win32::UI::Shell::AO_NONE)
            .map_err(|e| format!("{REQUIRES_ID} ({e})"))?;
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
