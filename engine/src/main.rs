// sky-engine——按 SKY_WINDOWS_PROTOCOL_SPEC 重写的 Sky computer-use helper
// （原版叫 codex-computer-use.exe），行为跟原版对齐。
//
// 启动约定（照原版观察到的来，一步不差）：
//   spawn(exe, ["--parent-pid", <hostPid>], { cwd: stageDir,
//     env: { CODEX_HOME, CODEX_CLI_PATH, ... } })
//   - stdin/stdout 上走一行一个的 JSON-RPC
//   - 没跟 `codex app-server` 握手完（CODEX_CLI_PATH）就不开始处理请求
//   - 父进程 pid 死了跟着退；物理 Esc 退出码 130（第 6 阶段）

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

/// 宿主起我们时把环境变量清得只剩 CODEX_HOME 和 CODEX_CLI_PATH。这里把
/// Windows API（ShellExecute、子进程、PowerShell）要用的几个基础变量补上，
/// 已经有的不覆盖。
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
                // 问 shell 要；一个环境变量都没有也问得出来。
                use windows::Win32::UI::Shell::SHGetFolderPathW;
                let mut buf = [0u16; windows::Win32::Foundation::MAX_PATH as usize];
                unsafe {
                    if SHGetFolderPathW(None, 28 /* CSIDL_LOCAL_APPDATA，本地 AppData 目录 */, None, 0, &mut buf)
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
    // 用户目录相关的变量：少了 USERPROFILE/APPDATA/LOCALAPPDATA，
    // PowerShell 的 Get-StartApps 和 shell 的 per-user AppModel 视图会
    // 悄悄退化到几乎查不出东西。
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
            if let Some(p) = shell_folder(40 /* CSIDL_PROFILE，用户主目录 */) {
                std::env::set_var("USERPROFILE", p);
            }
        }
        if std::env::var_os("APPDATA").is_none() {
            if let Some(p) = shell_folder(26 /* CSIDL_APPDATA，漫游 AppData 目录 */) {
                std::env::set_var("APPDATA", p);
            }
        }
        if std::env::var_os("LOCALAPPDATA").is_none() {
            if let Some(p) = shell_folder(28 /* CSIDL_LOCAL_APPDATA，本地 AppData 目录 */) {
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
    // 原版 helper 是 per-monitor DPI-aware 进程：UIA 给的矩形、SendInput 收的
    // 坐标，都是物理像素。
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    harden_env();
    // 私有调试模式，不在 RPC 契约里。
    {
        let argv: Vec<String> = std::env::args().collect();
        if argv.get(1).map(String::as_str) == Some("--wins") {
            for (hwnd, pid, class, title, visible, cloaked, tool, app) in win::enumerate::debug_windows() {
                if title.trim().is_empty() && !class.contains("Notepad") && !class.contains("Content") {
                    continue;
                }
                println!(
                    "{hwnd} pid={pid} vis={visible} cloak={cloaked} tool={tool} class={class:?} title={title:?} app={app:?}"
                );
            }
            return ExitCode::SUCCESS;
        }
    }
    // 私有调试模式（不在 RPC 契约里）：单抓一帧。
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
            // 原版那个闭源 helper 会再起一个自己当 system-cursor manager
            // 子进程。v1 直接驱真光标，没有 overlay，所以这个模式什么都不做，
            // 立刻退出。
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
