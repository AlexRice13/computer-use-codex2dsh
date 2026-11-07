// launch_app：原版只认两种形状 —— list_apps 里的已安装应用 id（AUMID），或者
// 绝对路径的 exe。错误文案跟原版 helper 逐字节一致。真正启动要过审批
// （dispatch 只在审批之后才调 execute()）。
//
// 在 DSH 里多做了两件事，都是为了让"启动一个应用"这件事别卡在身份上：
// - 裸名字（"画图" / "Paint" / "mspaint"）先按 AppsFolder 清单折成 AUMID；
// - WindowsApps 下的打包 exe 路径也折成 AUMID，审批身份和窗口归属才对得上。

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::System::Com::CoCreateInstance;
use windows::Win32::UI::Shell::{IApplicationActivationManager, ShellExecuteW};

const REQUIRES_ID: &str =
    "launch_app requires an installed app id from list_apps or a launchable executable identifier";

// ApplicationActivationManager 的 CLSID：{2E941141-7F97-4756-BA1D-9DECDE894A3D}
const CLSID_AA_MANAGER: GUID = GUID::from_u128(0x2E941141_7F97_4756_BA1D_9DECDE894A3D);

/// 校验完之后，调用方到底要启动什么。
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

/// 校验并分类 `app`，没有副作用。这一步跑在审批之前，所以请求不合法时
/// 根本弹不出审批框（跟原版一致）。
pub fn plan(app: &str) -> Result<Plan, String> {
    let app = app.trim();
    if app.is_empty() {
        return Err("app is required".into());
    }
    // 纯数字的标识符会被当成 pid。
    if app.parse::<i64>().is_ok() {
        return Err("launch_app does not support pid app identifiers".into());
    }
    let lower = app.to_lowercase();
    // shell: 这类动词不收，虽然看着像能启动。
    if lower.starts_with("shell:") || lower.starts_with("app-user-model-id:") {
        return Err(REQUIRES_ID.into());
    }
    // 先看是不是盘符开头（路径里可能带 _ 和 !）。
    if has_drive_prefix(app) || app.starts_with("\\\\") {
        if !std::path::Path::new(app).is_absolute() {
            return Err("app policy requires an absolute executable path".into());
        }
        if !std::path::Path::new(app).exists() {
            return Err(REQUIRES_ID.into());
        }
        // 打包应用的 exe（WindowsApps 下面那些）按 AUMID 启动：直接
        // ShellExecute 也能开，但窗口属于包身份，审批和 wait_for_window
        // 拿 exe 路径对不上，会误报 "did not expose a targetable window"。
        if let Some(aumid) = aumid_for_packaged_exe(app) {
            return Ok(Plan::Aumid(aumid));
        }
        return Ok(Plan::ExePath(app.to_string()));
    }
    if looks_like_aumid(app) {
        return Ok(Plan::Aumid(app.to_string()));
    }
    // 剩下的全拒掉（"notepad" 这种裸名字、相对路径的 *.exe）：原版从不找
    // shell 去解析名字。
    if lower.ends_with(".exe") || app.contains('\\') {
        return Err("app policy requires an absolute executable path".into());
    }
    Err(REQUIRES_ID.into())
}

/// 审批网关用的身份串。
pub fn plan_owner(plan: &Plan) -> String {
    match plan {
        Plan::Aumid(a) => a.clone(),
        Plan::ExePath(p) => format!("process:{p}"),
    }
}

/// 这个串像不像"应用的名字"（而不是 id、pid、路径或 shell 动词）。
/// 只有这种输入才值得去 AppsFolder 清单里查一遍。
pub fn is_bare_name(app: &str) -> bool {
    let a = app.trim();
    if a.is_empty() || a.parse::<i64>().is_ok() {
        return false;
    }
    let lower = a.to_lowercase();
    if lower.starts_with("shell:") || lower.starts_with("app-user-model-id:") {
        return false;
    }
    if has_drive_prefix(a) || a.starts_with("\\\\") || a.contains('\\') || looks_like_aumid(a) {
        return false;
    }
    true
}

/// 裸名字 → AUMID。模型和用户都爱说"画图""Paint""mspaint"，原版 helper
/// 对这类输入直接报 REQUIRES_ID；这里先拿 AppsFolder 清单对一遍，对上就
/// 按 AUMID 走，审批看到的身份也就跟 list_apps 里那条一致了。
pub fn resolve_name(app: &str) -> Option<String> {
    let raw = app.trim();
    if raw.is_empty() {
        return None;
    }
    let mut token = raw.to_lowercase();
    if token.ends_with(".exe") {
        token.truncate(token.len() - 4);
    }
    let compact = squash(&token);

    // 清单里的显示名是本地化过的（中文系统上是"画图"），老名字对不上，
    // 但这几个是大家真会敲的，直接给包名前缀。
    const ALIASES: &[(&str, &str)] = &[
        ("mspaint", "microsoft.paint_"),
        ("paint", "microsoft.paint_"),
        ("画图", "microsoft.paint_"),
        ("notepad", "microsoft.windowsnotepad_"),
        ("记事本", "microsoft.windowsnotepad_"),
    ];
    let alias_prefix = ALIASES
        .iter()
        .find(|(name, _)| squash(name) == compact)
        .map(|(_, prefix)| *prefix);

    for (display, id) in crate::startapps::collect() {
        // 跟 list_apps 一个口径：浏览器/终端/shell 组件不解析。
        if crate::catalog::is_filtered(&id, Some(&display)) || !id.contains('!') {
            continue;
        }
        let idl = id.to_lowercase();
        if let Some(prefix) = alias_prefix {
            if idl.starts_with(prefix) {
                return Some(id);
            }
        }
        let dl = display.to_lowercase();
        if dl == token || squash(&dl) == compact {
            return Some(id);
        }
        // 包名尾巴：Microsoft.Paint_8wekyb3d8bbwe!App → "Paint"。
        let family = idl.split('!').next().unwrap_or("");
        let tail = family
            .split('_')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("");
        if !tail.is_empty() && (tail == token || (token.len() >= 4 && tail.ends_with(&token))) {
            return Some(id);
        }
    }
    None
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `...\WindowsApps\Microsoft.Paint_11.2605.81.0_x64__8wekyb3d8bbwe\...`
/// → `Microsoft.Paint_8wekyb3d8bbwe!App`（取清单里同族那条）。
fn aumid_for_packaged_exe(path: &str) -> Option<String> {
    let mut seen_windows_apps = false;
    let mut dir = None;
    for part in path.split('\\') {
        if seen_windows_apps {
            dir = Some(part);
            break;
        }
        if part.eq_ignore_ascii_case("WindowsApps") {
            seen_windows_apps = true;
        }
    }
    let dir = dir?;
    let family = package_family(dir)?;
    crate::startapps::collect()
        .into_iter()
        .map(|(_, id)| id)
        .find(|id| {
            let idl = id.to_lowercase();
            idl.starts_with(&family) && idl.contains('!')
        })
}

/// 包目录名 `Microsoft.Paint_11.2605.81.0_x64__8wekyb3d8bbwe`
/// → 包族名 `Microsoft.Paint_8wekyb3d8bbwe`。
fn package_family(dir: &str) -> Option<String> {
    let (name, _) = dir.split_once('_')?;
    let publisher = dir.rsplit("__").next()?;
    if name.is_empty() || publisher.is_empty() || publisher == dir {
        return None;
    }
    Some(format!("{name}_{publisher}").to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_names_are_told_apart_from_ids_and_paths() {
        assert!(is_bare_name("画图"));
        assert!(is_bare_name("Paint"));
        assert!(is_bare_name("mspaint"));
        assert!(is_bare_name("notepad.exe"));
        assert!(!is_bare_name("Microsoft.Paint_8wekyb3d8bbwe!App"));
        assert!(!is_bare_name("C:\\Windows\\System32\\mspaint.exe"));
        assert!(!is_bare_name("shell:AppsFolder\\x"));
        assert!(!is_bare_name("123"));
        assert!(!is_bare_name("  "));
    }

    #[test]
    fn packaged_exe_dir_folds_to_family() {
        assert_eq!(
            package_family("Microsoft.Paint_11.2605.81.0_x64__8wekyb3d8bbwe").as_deref(),
            Some("microsoft.paint_8wekyb3d8bbwe")
        );
        assert_eq!(package_family("not-a-package"), None);
    }
}

/// 真正启动（审批之后才走这里）。原版会确认这次启动确实冒出了可定位的窗口：
/// 激活后等一会儿，等不到就报 "launched app did not expose a targetable window:
/// {owner}"（对着原版 helper 实测过）。
///
/// 两条启动通道都跑在单独一条 STA 线程上：shell 那边这些 COM 类
/// （ApplicationActivationManager）只认 STA，从我们 MTA 的服务线程
/// CoCreateInstance 会挂 CO_E_CLASSSTRING (0x80040154)；原版是个 C++ helper，
/// 本来就在 STA 上下文里激活。
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
            // 让 shell COM 把重入的回调跑完。
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
    // 只有走 exe 路径这条通道才验窗口：ActivateApplication 返回 S_OK 就直接
    // 认成功（实测：打包应用的窗口一直没出现，原版照样回 OK）；ShellExecute
    // 可能悄悄什么都不干，所以要对着窗口表核一遍（实测：System32\Notepad.exe
    // -> "launched app did not expose a targetable window:
    // process:C:\Windows\System32\Notepad.exe"）。
    if let Plan::ExePath(_) = plan {
        // 把请求预算的大部分时间用在等窗口上（实测：打包应用冷启动时原版就是
        // 这么等的）。留够余量，别让 rpc 报预算耗尽。
        let wait_ms = budget_ms.saturating_sub(3000).clamp(4000, 12000);
        return wait_for_window(&owner, std::time::Duration::from_millis(wait_ms));
    }
    Ok(())
}

/// 轮询原始窗口表，直到出现 `owner` 名下的可见窗口（本来就开着的窗口也算数
/// —— 重新启动一个已经在跑的应用会把它窗口顶出来，这种情况必须成功）。这里
/// 用原始窗口表：打包应用（计算器）先冒出来的是个无标题窗口，
/// enumerate_top_level 的标题过滤器会把它扔掉。
fn wait_for_window(owner: &str, timeout: std::time::Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // 不按可见性过滤：打包应用（计算器）会留一个不可见/闪屏的顶层窗口，
        // 原版照样算成“可定位窗口”（实测：MainWindowHandle=0 时也回 OK）。
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
        // 首选走 shell 的激活服务。精简过的环境里光用 INPROC_SERVER 会挂
        // CO_E_CLASSSTRING，所以进程外回退也一起允许。
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
        // 回退：让 shell 去激活 AppsFolder 的 verb。效果一样（把打包应用
        // 激活并顶到前台），进程令牌导致直接 COM 激活被挡时还能用。
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
            return via_com; // 把 COM 原本的报错抛出去
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
