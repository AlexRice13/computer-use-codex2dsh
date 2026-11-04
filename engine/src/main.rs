// sky-engine — behavior-compatible open reimplementation of the Sky
// computer-use helper (codex-computer-use.exe), per SKY_WINDOWS_PROTOCOL_SPEC.
//
// Startup contract (observed, replicated exactly):
//   spawn(exe, ["--parent-pid", <hostPid>], { cwd: stageDir,
//     env: { CODEX_HOME, CODEX_CLI_PATH, ... } })
//   - line-delimited JSON-RPC over stdin/stdout
//   - blocks on the `codex app-server` handshake (CODEX_CLI_PATH) before serving
//   - dies with the parent pid; exit 130 on physical Escape (phase 6)

mod args;
mod catalog;
mod escape;
mod handshake;
mod launch;
mod methods;
mod parent;
mod policy;
mod rpc;
mod startapps;
mod state;
mod user_assist;
mod win;

use std::process::ExitCode;

/// The host spawns us with a stripped environment (CODEX_HOME, CODEX_CLI_PATH
/// only). Fill in the basics Windows APIs (ShellExecute, child processes,
/// PowerShell) expect, without overwriting anything already set.
fn harden_env() {
    let sysroot = std::env::var("SystemRoot")
        .or_else(|_| std::env::var("SYSTEMROOT"))
        .unwrap_or_else(|_| "C:\\Windows".into());
    if std::env::var_os("SystemRoot").is_none() && std::env::var_os("SYSTEMROOT").is_none() {
        std::env::set_var("SystemRoot", &sysroot);
    }
    if std::env::var_os("windir").is_none() {
        std::env::set_var("windir", &sysroot);
    }
    if std::env::var_os("PATH").is_none() {
        std::env::set_var(
            "PATH",
            format!(
                "{sysroot}\\system32;{sysroot};{sysroot}\\System32\\Wbem;{sysroot}\\System32\\WindowsPowerShell\\v1.0"
            ),
        );
    }
    if std::env::var_os("TEMP").is_none() || std::env::var_os("TMP").is_none() {
        let tmp = std::env::var("LOCALAPPDATA")
            .map(|l| format!("{l}\\Temp"))
            .unwrap_or_else(|_| {
                // Ask the shell; works with no env at all.
                use windows::Win32::UI::Shell::SHGetFolderPathW;
                let mut buf = [0u16; windows::Win32::Foundation::MAX_PATH as usize];
                unsafe {
                    if SHGetFolderPathW(None, 28 /* CSIDL_LOCAL_APPDATA */, None, 0, &mut buf)
                        .is_ok()
                    {
                        let len = buf.iter().position(|c| *c == 0).unwrap_or(0);
                        format!("{}\\Temp", String::from_utf16_lossy(&buf[..len]))
                    } else {
                        std::env::temp_dir().display().to_string()
                    }
                }
            });
        let _ = std::fs::create_dir_all(&tmp);
        if std::env::var_os("TEMP").is_none() {
            std::env::set_var("TEMP", &tmp);
        }
        if std::env::var_os("TMP").is_none() {
            std::env::set_var("TMP", &tmp);
        }
    }
    // Profile-scoped folders: PowerShell's Get-StartApps and the shell's
    // per-user AppModel views silently degrade to almost nothing without
    // USERPROFILE/APPDATA/LOCALAPPDATA.
    {
        use windows::Win32::UI::Shell::SHGetFolderPathW;
        let shell_folder = |csidl: i32| -> Option<String> {
            let mut buf = [0u16; windows::Win32::Foundation::MAX_PATH as usize];
            unsafe {
                SHGetFolderPathW(None, csidl, None, 0, &mut buf)
                    .ok()
                    .map(|_| String::from_utf16_lossy(&buf[..buf.iter().position(|c| *c == 0).unwrap_or(0)]))
            }
        };
        if std::env::var_os("USERPROFILE").is_none() {
            if let Some(p) = shell_folder(40 /* CSIDL_PROFILE */) {
                std::env::set_var("USERPROFILE", p);
            }
        }
        if std::env::var_os("APPDATA").is_none() {
            if let Some(p) = shell_folder(26 /* CSIDL_APPDATA */) {
                std::env::set_var("APPDATA", p);
            }
        }
        if std::env::var_os("LOCALAPPDATA").is_none() {
            if let Some(p) = shell_folder(28 /* CSIDL_LOCAL_APPDATA */) {
                std::env::set_var("LOCALAPPDATA", p);
            }
        }
        if std::env::var_os("HOME").is_none() {
            if let Some(p) = std::env::var_os("USERPROFILE") {
                std::env::set_var("HOME", p);
            }
        }
    }
}

fn main() -> ExitCode {
    // The original helper is a per-monitor DPI-aware process: UIA rectangles
    // and SendInput coordinates are physical pixels.
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    harden_env();
    // Private debug mode (not part of the RPC contract): capture one frame.
    {
        let argv: Vec<String> = std::env::args().collect();
        if argv.get(1).map(String::as_str) == Some("--capture") {
            let hwnd = argv
                .get(2)
                .and_then(|s| i64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok().or_else(|| s.parse().ok()))
                .expect("usage: --capture <hwnd> <out.jpg>");
            let out = argv.get(3).cloned().unwrap_or_else(|| "capture.jpg".into());
            let _com = win::com::ComGuard::new();
            match win::capture::capture_window_jpeg(windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void)) {
                Ok((bytes, w, h)) => {
                    std::fs::write(&out, &bytes).expect("write failed");
                    println!("wrote {} bytes ({w}x{h}) to {out}", bytes.len());
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("capture failed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    let parsed = match args::parse_args() {
        Ok(args::Invocation::Rpc(opts)) => opts,
        Ok(args::Invocation::CursorManager) => {
            // The proprietary helper re-execs itself as a system-cursor
            // manager child. v1 drives the real cursor and has no overlay,
            // so this mode is a no-op that exits immediately.
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    parent::start_watchdog(parsed.parent_pid);
    escape::start();

    let pol = match handshake::run_app_server_handshake() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    rpc::serve(pol);
    ExitCode::SUCCESS
}
