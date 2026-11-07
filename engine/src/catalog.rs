use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{Interface, PCWSTR};
use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, STGM, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::IShellLinkW;

use crate::user_assist;
use crate::win::com::ComGuard;
use crate::win::enumerate::WindowRecord;
use crate::win::identity::{self, CLSID_SHELL_LINK};

/// 原版 helper 拿这几个已知文件夹 GUID 当 id 前缀（实测到的）。
const PROGRAM_FILES_GUID: &str = "{6D809377-6AF0-444B-8957-A3773F02200E}";
const PROGRAM_FILES_X86_GUID: &str = "{7C5A40EF-A0FB-4BFC-874A-C0F2E0B9FA8E}";
const SYSTEM32_GUID: &str = "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}";

pub struct AppRecord {
    pub id: String,
    pub display_name: Option<String>,
    pub is_running: bool,
    pub windows: Vec<WindowRecord>,
    pub last_used_date: Option<String>,
    pub use_count: Option<u32>,
}

/// 一条扫出来的快捷方式。
struct LnkEntry {
    path_lower: String,
    stem: String,
    id: String,
    exe: Option<String>,
}

/// 拼出 list_apps 那个数组。原版 helper 的条目集合是挑过的：正在跑的应用，
/// 加上 UserAssist 里的使用历史，就这么多（实测：40 条 = 2 个在跑 + 38 条
/// 使用记录）。这里照做：条目从 UserAssist 来，展示用的 id 和显示名去
/// 开始菜单/桌面快捷方式目录里解，目录认不出来的正在跑的应用再补一条。
pub fn build_catalog(records: &[WindowRecord]) -> Vec<AppRecord> {
    let _com = ComGuard::new();

    // 把正在跑的窗口按运行时 app id 分组。
    let mut by_app: HashMap<String, Vec<&WindowRecord>> = HashMap::new();
    for r in records {
        by_app.entry(r.app.clone()).or_default().push(r);
    }

    let lnks = scan_lnk_entries();
    let assist = user_assist::collect();
    // 原版给日期的那一块，是 UserAssist 和 shell AppsFolder 目录的*交集*
    // （实测：只在 UserAssist 里出现的噪音 —— 装好的 exe、cmd.exe、安装
    // 程序 —— 从来不露头）。所以先把目录拉下来。
    let apps = crate::startapps::collect();
    let apps_lower: std::collections::HashSet<String> =
        apps.iter().map(|(_, id)| id.to_lowercase()).collect();
    let trace = std::env::var("SKY_TRACE").is_ok();
    if trace {
        eprintln!(
            "[trace] catalog: lnks={} assist={} apps={}",
            lnks.len(),
            assist.len(),
            apps.len()
        );
    }
    let mut assist_entries: Vec<&user_assist::AssistInfo> = assist.values().collect();
    assist_entries.sort_by(|a, b| a.decoded.to_lowercase().cmp(&b.decoded.to_lowercase()));

    let mut out: Vec<AppRecord> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 第一遍：先处理裸 AUMID 条目（后面 \WindowsApps\ 下那些打包 exe 路径
    // 才好并进来）。
    for info in &assist_entries {
        let key = info.decoded.trim();
        if !identity::looks_like_aumid(key) {
            continue;
        }
        // 微软自家打包的 AUMID（shell 组件之类）从不作为历史条目出现
        // （实测：Game Bar、Notepad、Terminal、Store…都不出现）。
        let kl = key.to_lowercase();
        if kl.starts_with("microsoft.") && kl.contains('_') && kl.contains('!') {
            continue;
        }
        if is_filtered(key, None) || !apps_lower.contains(&key.to_lowercase()) {
            continue;
        }
        if !seen_ids.insert(key.to_lowercase()) {
            continue;
        }
        let wins = take_windows(&mut by_app, key, None);
        let is_running = !wins.is_empty();
        out.push(AppRecord {
            id: key.to_string(),
            display_name: Some(identity::app_display_name(key)),
            is_running,
            windows: wins,
            last_used_date: if is_running { None } else { info.last_used_date.clone() },
            // 实测：先启动、但没算进次数的记录，原版照样输出 useCount: 0
            // （日期也一起带上），0 不省略。
            use_count: if is_running { None } else { Some(info.use_count) },
        });
    }

    // 第二遍：路径类条目（.lnk、.exe、带 GUID 前缀的）。每条 UserAssist key
    // 都去目录里找对应条目：精确 AppID、快捷方式目标或 URI、快捷方式名字
    // 匹配。目录里没有对应物的就是噪音（下载的安装包、cmd.exe 之类），从来
    // 不露头。
    let a_count = out.len();
    let by_appid: HashMap<String, (String, String)> = apps
        .iter()
        .map(|(d, a)| (a.to_lowercase(), (d.clone(), a.clone())))
        .collect();
    let mut by_display: HashMap<String, (String, String)> = HashMap::new();
    for (d, a) in &apps {
        by_display
            .entry(d.to_lowercase())
            .or_insert_with(|| (d.clone(), a.clone()));
    }
    let lookup = |cand: &str| -> Option<(String, String)> {
        by_appid.get(&cand.to_lowercase()).cloned()
    };
    for info in &assist_entries {
        let key = info.decoded.trim();
        let lower = key.to_lowercase();
        if identity::looks_like_aumid(key) {
            continue;
        }

        let mut hit: Option<(String, String)> = None;
        let mut exe_hint: Option<String> = None;
        if lower.ends_with(".lnk") {
            if let Some(l) = lnks.iter().find(|l| l.path_lower == lower) {
                hit = lookup(&l.id);
                exe_hint = l.exe.clone();
            }
            if hit.is_none() {
                let stem = Path::new(key)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                hit = by_display.get(&stem).cloned();
            }
        } else if lower.contains("://") {
            // 协议 URI 启动的（steam://…）直接拿 key 去目录里对。
            hit = lookup(key);
        } else if lower.ends_with(".exe") {
            let expanded = expand_guid_prefix(key);
            // 把打包应用的 exe 路径并进它那条裸 AUMID 条目。
            if expanded.to_lowercase().contains("\\windowsapps\\")
                && seen_ids
                    .iter()
                    .any(|aumid| folds_package_exe(aumid, &expanded))
            {
                continue;
            }
            // 匹配顺序：精确 AppID（原始路径，以及折成已知文件夹前缀的），
            // 目标就是这个 exe 的快捷方式，名字等于 exe 文件名或某级父目录名
            // 的快捷方式（steam 启动器），显示名等于 exe 文件名的目录条目。
            hit = lookup(key)
                .or_else(|| lookup(&expanded))
                .or_else(|| lookup(&catalog_id_for_exe(&expanded)))
                .or_else(|| {
                lnks
                    .iter()
                    .find(|l| {
                        l.exe
                            .as_deref()
                            .map(|e| e.eq_ignore_ascii_case(&expanded))
                            .unwrap_or(false)
                    })
                    .and_then(|l| lookup(&l.id))
            });
            if hit.is_none() {
                let m = lnks.iter().find(|l| {
                    let mut units: Vec<String> = Vec::new();
                    let p = Path::new(&expanded);
                    if let Some(s) = p.file_stem() {
                        units.push(s.to_string_lossy().to_lowercase());
                    }
                    for anc in p.ancestors().take(4) {
                        if let Some(s) = anc.file_stem() {
                            units.push(s.to_string_lossy().to_lowercase());
                        }
                    }
                    units
                        .iter()
                        .any(|u| !u.is_empty() && u.eq_ignore_ascii_case(&l.stem))
                });
                if let Some(l) = m {
                    hit = lookup(&l.id).or_else(|| by_display.get(&l.stem.to_lowercase()).cloned());
                }
            }
            if hit.is_none() {
                let stem = Path::new(&expanded)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                hit = by_display.get(&stem).cloned();
            }
            exe_hint = Some(expanded);
        }
        let probe_kw = lower.contains("uu_")
            || lower.contains("wiztree")
            || lower.contains("steam")
            || lower.contains("tryp")
            || lower.contains("lceda")
            || lower.contains("gpu-z")
            || lower.contains("teardown")
            || lower.contains("clash-verge");
        let Some((display, id)) = hit.clone() else {
            if trace && probe_kw {
                eprintln!("[trace] passB MISS key={key}");
            }
            continue;
        };
        if trace && probe_kw {
            eprintln!("[trace] passB HIT key={key} id={id} date={:?} uc={}", info.last_used_date, info.use_count);
        }
        if is_filtered(&id, Some(&display)) {
            continue;
        }
        let id_lower = id.to_lowercase();
        if !seen_ids.insert(id_lower.clone()) {
            if trace && id_lower.contains("gpu-z") {
                eprintln!("[trace] passB DUP id={id} via key={key}");
            }
            continue;
        }
        let wins = take_windows(&mut by_app, &id, exe_hint.as_deref());
        let is_running = !wins.is_empty();
        out.push(AppRecord {
            id,
            display_name: Some(display),
            is_running,
            windows: wins,
            last_used_date: if is_running { None } else { info.last_used_date.clone() },
            // 实测：先启动、但没算进次数的记录，原版照样输出 useCount: 0
            // （日期也一起带上），0 不省略。
            use_count: if is_running { None } else { Some(info.use_count) },
        });
    }

    // 第三遍：shell 那个 "AppsFolder" 应用目录（跟 Get-StartApps /
    // Windows.UI.Shell.ApplicationInformation.FindAllAsync 同一份数据）。
    // 原版把这些按显示名排序追加在后面，id 对得上的再挂上 UserAssist 的次数，
    // 最后整个列表截到 40 条（对着原版 helper 实测过）。
    let b_count = out.len();
    if trace {
        eprintln!("[trace] catalog: passA={} passB={}", a_count, b_count - a_count);
    }
    for (display, appid) in apps {
        let lower = appid.to_lowercase();
        if appid.trim().is_empty()
            || is_filtered(&appid, Some(&display))
            || !seen_ids.insert(lower.clone())
        {
            continue;
        }
        let wins = take_windows(&mut by_app, &appid, None);
        let is_running = !wins.is_empty();
        // 只有普通记录才挂 UserAssist 次数：微软打包的 AUMID 从来不带历史；
        // UserAssist 里还有日期、但没进日期块的记录算过期（原版把日期和次数
        // 一起扔掉）。
        let is_ms_pkg = lower.starts_with("microsoft.") && lower.contains('_') && lower.contains('!');
        let assist_hit = if is_ms_pkg {
            None
        } else {
            assist.get(&lower).filter(|a| a.last_used_date.is_none())
        };
        out.push(AppRecord {
            id: appid,
            display_name: Some(display),
            is_running,
            windows: wins,
            last_used_date: if is_running {
                None
            } else {
                assist_hit.and_then(|a| a.last_used_date.clone())
            },
            use_count: if is_running {
                None
            } else {
                assist_hit.map(|a| a.use_count)
            },
        });
    }

    // 第四遍：打包 AUMID 的窗口，加上没人认领的进程窗口，都合成一条记录
    // （id 就用运行时标识符，跟实测对得上）。
    for (app, list) in std::mem::take(&mut by_app) {
        let wins: Vec<WindowRecord> = list.into_iter().cloned().collect();
        if wins.is_empty() {
            continue;
        }
        if !seen_ids.insert(app.to_lowercase()) {
            continue;
        }
        let display_name = identity::app_display_name(&app);
        out.push(AppRecord {
            id: app,
            display_name: Some(display_name),
            is_running: true,
            windows: wins,
            last_used_date: None,
            use_count: None,
        });
    }

    // 正在跑、但 id 还是进程路径的记录，可能带着任务栏身份（那些自己调
    // SetAppUserModelID 的应用）：原版报的是那个 AUMID。
    {
        let taskbar = crate::win::taskbar::appids();
        if !taskbar.is_empty() {
            if trace {
                eprintln!("[trace] taskbar entries={}", taskbar.len());
            }
            for r in out
                .iter_mut()
                .filter(|r| r.is_running && r.id.starts_with("process:"))
            {
                if let Some(aumid) = r.windows.iter().find_map(|w| {
                    let t = w.title.trim().to_lowercase();
                    if t.is_empty() {
                        return None;
                    }
                    taskbar.get(&t).cloned().or_else(|| {
                        // "Foo — App" 这种标题，仍然属于任务栏按钮 "App"。
                        taskbar
                            .iter()
                            .find(|(name, _)| name.len() >= 3 && t.contains(name.as_str()))
                            .map(|(_, a)| a.clone())
                    })
                }) {
                    r.id = aumid;
                }
            }
        }
    }

    // 正在跑的应用会把同名的目录/UserAssist 副本吞掉：原版这种情况下只列
    // 一条，就是那条正在跑的。
    let running_displays: std::collections::HashSet<String> = out
        .iter()
        .filter(|r| r.is_running)
        .filter_map(|r| r.display_name.as_deref().map(|d| d.to_lowercase()))
        .collect();
    out.retain(|r| {
        r.is_running || !running_displays.contains(&r.display_name.as_deref().unwrap_or("").to_lowercase())
    });

    // 实测的顺序：正在跑的排前面（窗口数多的靠前），然后按 lastUsedDate
    // 倒序，再按显示名正序 —— 这样 A/B 对比才稳。
    out.sort_by(|a, b| {
        b.is_running
            .cmp(&a.is_running)
            .then_with(|| b.windows.len().cmp(&a.windows.len()))
            .then_with(|| {
                b.last_used_date
                    .as_deref()
                    .unwrap_or("")
                    .cmp(a.last_used_date.as_deref().unwrap_or(""))
            })
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    if trace {
        for (i, r) in out.iter().take(30).enumerate() {
            eprintln!("[trace] sorted[{i}] run={} d={:?} wins={} {} id={}", r.is_running, r.display_name, r.windows.len(), r.last_used_date.as_deref().unwrap_or("-"), &r.id[..r.id.len().min(60)]);
        }
        eprintln!("[trace] pre-truncate total={}", out.len());
    }
    out.truncate(40);
    out
}

/// 原版从来不列的条目（浏览器、终端、shell/系统 UWP）。
/// 词表是从二进制里内嵌的清单抠出来的（@ browser/terminal 进程表），再加上
/// 实测被过滤掉的那些具体 AUMID/ProgID。
pub fn is_filtered(id: &str, display: Option<&str>) -> bool {
    let idl = id.to_lowercase();
    let dl = display.unwrap_or("").to_lowercase();

    // 精确 id 和前缀（不区分大小写）。
    const PREFIXES: &[&str] = &[
        "microsoft.autogenerated.",
        "microsoft.windowsnotepad_",
        "microsoft.screensketch_",
        "microsoft.windowsterminal_",
        "microsoft.powershell_",
        "microsoft.zunemusic_",
        "microsoft.zunemovie_",
        "microsoft.windowsstore_",
        "microsoft.windows.photos_",
        "microsoft.windows.explorer",
        "microsoft.visualstudiocode",
        "microsoft.microsoftedge_",
        "clipchamp.",
        "microsoft.edge",
        "chrome",
        "msege",
        "msedge",
        "1f8b0f94.122165ae053f_j2p0p5q0044a6!cloudmusic",
    ];
    if PREFIXES.iter().any(|p| idl.starts_with(p)) {
        return true;
    }
    // 网页快捷方式（.url）从不列出；本地文件目标（.chm、.txt、.ini）会列。
    if idl.ends_with(".url") {
        return true;
    }
    const EXACT: &[&str] = &["qq", "chrome", "msege", "msedge", "browser"];
    if EXACT.contains(&idl.as_str()) {
        return true;
    }

    // 路径里任何位置出现浏览器可执行文件，都滤掉。
    const BROWSER_WORDS: &[&str] =
        &["\\chrome\\", "\\msedge\\", "\\brave\\", "\\opera\\", "\\firefox\\", "iexplore"];
    if BROWSER_WORDS.iter().any(|w| idl.contains(w)) {
        return true;
    }

    // 聊天/云客户端和 shell 辅助程序，就算以路径形式出现，原版也从不露头
    // （实测：QQ、网易云音乐、CredentialUIBroker）。
    const NOISE_PATHS: &[&str] = &[
        "\\qqnt\\", "\\tencent\\qqnt\\", "\\netease\\cloudmusic\\",
        "credentialuibroker", "\\windowsapps\\",
    ];
    if NOISE_PATHS.iter().any(|w| idl.contains(w)) {
        return true;
    }

    // 终端/控制台可执行文件：文件名照抄二进制里那张终端表。
    let stem = Path::new(&idl)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    const TERMINALS: &[&str] = &[
        "alacritty", "bash", "cmd", "cmder", "cmder64", "conemu", "conemu64", "conhost",
        "fluentterminal", "git-bash", "hyper", "kitty", "mintty", "mobaxterm", "openconsole",
        "powershell", "powershell_ise", "putty", "pwsh", "tabby", "terminal", "terminus",
        "termius", "tftp", "warp", "wezterm", "wezterm-gui", "windowsterminal", "wsl", "wt",
    ];
    if TERMINALS.contains(&stem.as_str()) {
        return true;
    }

    // 有些过滤规则看的是显示名。
    if dl == "character map" || dl == "charmap" {
        return true;
    }
    false
}

fn take_windows(
    by_app: &mut HashMap<String, Vec<&WindowRecord>>,
    id: &str,
    exe: Option<&str>,
) -> Vec<WindowRecord> {
    let key = match exe {
        Some(e) => format!("process:{e}"),
        None => id.to_string(),
    };
    by_app
        .remove(&key)
        .map(|list| list.into_iter().cloned().collect())
        .unwrap_or_default()
}

/// 这条裸 AUMID 条目是不是已经代表了这个打包 exe 路径？
fn folds_package_exe(aumid: &str, exe_path: &str) -> bool {
    let (family, _) = aumid.split_once('!').unwrap_or((aumid, ""));
    let (name, publisher) = match family.rsplit_once('_') {
        Some((n, p)) => (n, p),
        None => return false,
    };
    let p = exe_path.to_lowercase();
    p.contains(&format!("\\windowsapps\\{name}_")) && p.contains(&format!("__{publisher}\\"))
}

fn scan_lnk_entries() -> Vec<LnkEntry> {
    let mut out = Vec::new();
    for dir in lnk_dirs() {
        for lnk in walk_lnk(&dir) {
            let Some(target) = resolve_lnk(&lnk) else { continue };
            let stem = lnk
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let (id, exe) = match &target {
                LnkTarget::Exe(p) => (catalog_id_for_exe(p), Some(p.clone())),
                LnkTarget::Uri(u) => (u.clone(), None),
            };
            if id.trim().is_empty() {
                continue;
            }
            out.push(LnkEntry {
                path_lower: lnk.to_string_lossy().to_lowercase(),
                stem,
                id,
                exe,
            });
        }
    }
    out
}

fn lnk_dirs() -> Vec<PathBuf> {
    let mut dirs = start_menu_dirs();
    if let Ok(up) = std::env::var("USERPROFILE") {
        dirs.push(Path::new(&up).join("Desktop"));
    }
    if let Ok(pd) = std::env::var("PUBLIC") {
        dirs.push(Path::new(&pd).join("Desktop"));
    }
    dirs
}

/// 把 UserAssist 里 `{KnownFolderGUID}\rest` 形式的路径展开成文件系统路径。
fn expand_guid_prefix(path: &str) -> String {
    let map = [
        (PROGRAM_FILES_GUID, std::env::var("ProgramFiles").ok()),
        (PROGRAM_FILES_X86_GUID, std::env::var("ProgramFiles(x86)").ok()),
        (
            SYSTEM32_GUID,
            std::env::var("WINDIR").ok().map(|w| format!("{w}\\System32")),
        ),
    ];
    for (guid, root) in map {
        if let Some(root) = root {
            if starts_with_ignore_ascii_case(path, guid) {
                let rest_str = tail_bytes(path, guid.len());
                let rest = rest_str.trim_start_matches('\\');
                return format!("{root}\\{rest}");
            }
        }
    }
    path.to_string()
}

/// 按字节比较、不区分大小写的前缀判断（路径里可能有非 ASCII 字符，按字节
/// 切才安全）。
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn tail_bytes(s: &str, from: usize) -> String {
    String::from_utf8_lossy(&s.as_bytes()[from..]).into_owned()
}

fn start_menu_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        dirs.push(Path::new(&appdata).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    if let Ok(pd) = std::env::var("ProgramData") {
        dirs.push(Path::new(&pd).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    dirs
}

fn walk_lnk(dir: &Path) -> Vec<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    let mut out = Vec::new();
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

enum LnkTarget {
    Exe(String),
    Uri(String),
}

fn resolve_lnk(lnk: &Path) -> Option<LnkTarget> {
    unsafe {
        let link: IShellLinkW =
            CoCreateInstance(&CLSID_SHELL_LINK, None, CLSCTX_INPROC_SERVER).ok()?;
        let pf: IPersistFile = link.cast().ok()?;
        let wide: Vec<u16> = lnk.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        pf.Load(PCWSTR(wide.as_ptr()), STGM(0)).ok()?;

        // 先看参数里有没有协议 URI（steam://、ms-settings: 等等）。
        let mut argbuf = [0u16; 2048];
        if link.GetArguments(&mut argbuf).is_ok() {
            let args = cut(&argbuf);
            for tok in args.split_whitespace() {
                if tok.contains("://") {
                    let cleaned = tok.trim_matches('"').trim();
                    if cleaned.contains("://") {
                        return Some(LnkTarget::Uri(cleaned.to_string()));
                    }
                }
            }
        }

        let mut pathbuf = [0u16; 4096];
        if link.GetPath(&mut pathbuf, std::ptr::null_mut(), 2u32).is_ok() {
            let p = cut(&pathbuf);
            if !p.trim().is_empty() {
                // 目标是 URL 的，一样当 uri id 处理。
                if p.contains("://") {
                    return Some(LnkTarget::Uri(p));
                }
                return Some(LnkTarget::Exe(expand_env(&p)));
            }
        }
        // 协议快捷方式（steam://…、ms-settings:…）的 URI 只藏在特殊的
        // item-id 字段里，改成扫原始字节去找。
        if let Some(uri) = scan_uri_from_lnk_bytes(lnk) {
            return Some(LnkTarget::Uri(uri));
        }
        None
    }
}

/// 不做解析，直接在 .lnk 二进制里按 UTF-16LE 扫一遍，找 `scheme://…`。
fn scan_uri_from_lnk_bytes(lnk: &Path) -> Option<String> {
    let bytes = std::fs::read(lnk).ok()?;
    if bytes.len() < 8 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let printable = |u: u16| u >= 0x20 && u < 0x7f;
    let mut i = 0usize;
    while i < units.len() {
        if !printable(units[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut run = String::new();
        while j < units.len() && printable(units[j]) {
            run.push(units[j] as u8 as char);
            j += 1;
        }
        i = j + 1;
        if let Some(pos) = run.find("://") {
            let scheme = &run[..pos];
            let looks_like_uri = !scheme.is_empty()
                && scheme.len() <= 12
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
                && run.len() > pos + 3;
            if looks_like_uri {
                return Some(run);
            }
        }
    }
    None
}

fn cut(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn expand_env(s: &str) -> String {
    // .lnk 的目标一般已经展开好了，这里只顺手替一遍 %VAR%。
    let mut out = s.to_string();
    for (k, v) in [
        ("%APPDATA%", std::env::var("APPDATA").ok()),
        ("%LOCALAPPDATA%", std::env::var("LOCALAPPDATA").ok()),
        ("%ProgramData%", std::env::var("ProgramData").ok()),
    ] {
        if let Some(v) = v {
            out = out.replace(k, &v);
        }
    }
    out
}

/// 给目录里的 exe 路径定 id（实测到的那几种形式）。
fn catalog_id_for_exe(path: &str) -> String {
    if let Some(rel) = under_known(path, std::env::var("ProgramFiles").ok().as_deref(), PROGRAM_FILES_GUID) {
        return format!("{PROGRAM_FILES_GUID}\\{rel}");
    }
    if let Some(rel) = under_known(
        path,
        std::env::var("ProgramFiles(x86)").ok().as_deref(),
        PROGRAM_FILES_X86_GUID,
    ) {
        return format!("{PROGRAM_FILES_X86_GUID}\\{rel}");
    }
    path.to_string()
}

fn under_known(path: &str, root: Option<&str>, _guid: &str) -> Option<String> {
    let root = root?;
    if starts_with_ignore_ascii_case(path, root) {
        let rest = tail_bytes(path, root.len()).trim_start_matches('\\').to_string();
        if !rest.is_empty() {
            return Some(rest);
        }
    }
    None
}
