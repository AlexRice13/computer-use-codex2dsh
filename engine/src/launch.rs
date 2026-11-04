// launch_app: start an app from a catalog id, AUMID, exe path, or bare
// executable name. Error texts are byte-exact with the original helper.

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{IApplicationActivationManager, ShellExecuteW};

const REQUIRES_ID: &str =
    "launch_app requires an installed app id from list_apps or a launchable executable identifier";

// CLSID_ApplicationActivationManager {2E941141-7F97-4756-BA1D-9DECDE894A3D}
const CLSID_AA_MANAGER: GUID = GUID::from_u128(0x2E941141_7F97_4756_BA1D_9DECDE894A3D);

fn looks_like_aumid(s: &str) -> bool {
    (s.contains('_') && s.contains('!')) || s.starts_with("windows.")
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
    // ShellExecute "open" on the literal path.
    unsafe {
        let p = HSTRING::from(path);
        let h = ShellExecuteW(None, PCWSTR::null(), PCWSTR(p.as_ptr()), PCWSTR::null(), PCWSTR::null(), windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL);
        if h.0 as isize <= 32 {
            return Err(format!("{REQUIRES_ID} (ShellExecute={})", h.0 as isize));
        }
    }
    Ok(())
}

/// Launch `app`; `{}` on success. Mirrors the original's acceptance rules:
/// pids rejected, shell: URIs rejected, AUMIDs activated, exes started,
/// bare names resolved through App Paths/PATH; ambiguous catalog names
/// rejected.
pub fn launch(app: &str) -> Result<(), String> {
    let app = app.trim();
    if app.is_empty() {
        return Err(REQUIRES_ID.into());
    }
    // Numeric identifiers read as pids.
    if app.parse::<i64>().is_ok() {
        return Err("launch_app does not support pid app identifiers".into());
    }
    // shell: verbs are not accepted even though they look like AUMIDs.
    if app.to_lowercase().starts_with("shell:") || app.to_lowercase().starts_with("app-user-model-id:") {
        return Err(REQUIRES_ID.into());
    }
    if looks_like_aumid(app) {
        return activate_aumid(app);
    }
    // Catalog first: bare names and ".exe" names frequently resolve to a
    // packaged AUMID, which must be activated, not ShellExecute'd.
    let hits = catalog_matches(app);
    if hits.len() > 1 {
        return Err(format!("{app} is ambiguous; use the canonical app id from list_apps"));
    }
    if hits.len() == 1 {
        let id = &hits[0];
        if looks_like_aumid(id) {
            return activate_aumid(id);
        }
        if std::path::Path::new(id).exists() {
            return start_exe(id);
        }
    }
    // A path to an existing exe, or a drive-qualified path at all.
    let pathish = app.contains('\\') || app.to_lowercase().ends_with(".exe");
    if pathish {
        if std::path::Path::new(app).exists() {
            return start_exe(app);
        }
        // Try resolving through App Paths registry (mirrors the helper's
        // resolver), then fall back to the shell (PATH handles plain
        // "notepad.exe" style names).
        if let Some(resolved) = resolve_app_path(app) {
            return start_exe(&resolved);
        }
        return shell_open(app);
    }
    // Bare identifier not in the catalog: fall back to the shell.
    shell_open(app)
}

/// ShellExecuteW "open" with the raw identifier.
fn shell_open(app: &str) -> Result<(), String> {
    unsafe {
        let p = HSTRING::from(app);
        let h = ShellExecuteW(
            None,
            PCWSTR::null(),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
        if h.0 as isize <= 32 {
            return Err(REQUIRES_ID.into());
        }
    }
    Ok(())
}

/// Registry App Paths lookup for bare exe names.
fn resolve_app_path(name: &str) -> Option<String> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
        REG_VALUE_TYPE,
    };
    unsafe {
        for hive in [HKEY_LOCAL_MACHINE] {
            for sub in [
                format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{name}"),
                format!("SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{name}"),
            ] {
                let wide: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
                let mut hkey = HKEY::default();
                if RegOpenKeyExW(hive, PCWSTR(wide.as_ptr()), None, KEY_READ, &mut hkey).is_err() {
                    continue;
                }
                let val_wide: Vec<u16> = [0u16].to_vec();
                let mut buf = [0u16; 1024];
                let mut size = (buf.len() * 2) as u32;
                let mut kind = REG_VALUE_TYPE::default();
                let ok = RegQueryValueExW(
                    hkey,
                    PCWSTR(val_wide.as_ptr()),
                    None,
                    Some(&mut kind),
                    Some(buf.as_mut_ptr() as *mut u8),
                    Some(&mut size),
                );
                let _ = RegCloseKey(hkey);
                if ok.is_ok() && kind == REG_SZ {
                    let len = (size as usize / 2).saturating_sub(1);
                    let val = String::from_utf16_lossy(&buf[..len]);
                    let cleaned = val.trim_matches('"').to_string();
                    if std::path::Path::new(&cleaned).exists() {
                        return Some(cleaned);
                    }
                }
            }
        }
        None
    }
}

/// Catalog rows whose id or (lowercased) display name equals `name`.
fn catalog_matches(name: &str) -> Vec<String> {
    let recs = crate::win::enumerate::enumerate_top_level();
    let rows = crate::catalog::build_catalog(&recs);
    let lower = name.to_lowercase();
    let stem = lower.trim_end_matches(".exe").to_string();
    rows.into_iter()
        .filter(|r| {
            r.id.eq_ignore_ascii_case(name)
                || r.display_name
                    .as_deref()
                    .map(|d| d.to_lowercase() == lower)
                    .unwrap_or(false)
                || r.id
                    .rsplit(['\\', '/'])
                    .next()
                    .map(|s| s.to_lowercase() == lower || s.to_lowercase() == format!("{lower}.exe"))
                    .unwrap_or(false)
                // Packaged ids like Microsoft.Windows.Notepad_<pkg>!App match
                // their meaningful dotted segment (length >= 4 to stay safe).
                || stem.len() >= 4
                    && r.id
                        .split(['.', '_', '!', '\\', '/'])
                        .any(|seg| seg.eq_ignore_ascii_case(&stem))
        })
        .map(|r| r.id)
        .collect()
}
