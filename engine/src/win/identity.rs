use std::mem::size_of;

use windows::core::{GUID, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, PROPERTYKEY, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Variant::VARENUM;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{
    SHCreateItemFromParsingName, IShellItem, SIGDN_NORMALDISPLAY,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;

/// PKEY_AppUserModel_ID —— {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}，pid 26。
const PKEY_APPUSERMODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
    pid: 26,
};

pub fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 2048];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

pub fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: i32 = 0;
    unsafe {
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut i32 as *mut _,
            size_of::<i32>() as u32,
        );
    }
    cloaked != 0
}

pub fn extended_bounds(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
        .ok()
        .map(|_| r)
    }
}

/// SHGetPropertyStoreForWindow —— windows 0.61 的绑定漏了 dwFlags 参数，x64
/// 下调用会把栈搞坏，所以这里自己声明，自己从 shell32 里取。
type GPSFW = unsafe extern "system" fn(
    HWND,
    *const windows::core::GUID,
    *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT;

fn gpsfw() -> Option<GPSFW> {
    unsafe {
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
        let h = LoadLibraryA(windows::core::s!("shell32.dll")).ok()?;
        GetProcAddress(h, windows::core::s!("SHGetPropertyStoreForWindow"))
            .map(|a| std::mem::transmute::<*mut core::ffi::c_void, GPSFW>(a as *mut _))
    }
}

/// 打包应用的窗口，从 shell 属性仓库里取它的 AUMID。
pub fn window_aumid(hwnd: HWND) -> Option<String> {
    if let Some(a) = aumid_of(hwnd) {
        return Some(a);
    }
    // ILaunchSourceAppUserModelId（任务栏的启动信息）能补上 Electron 和转发
    // 启动器这类窗口没有 PKEY 的应用（比如 com.deepseek.dsh）。
    if let Some(a) = launch_source_aumid(hwnd) {
        return Some(a);
    }
    // 由 ApplicationFrameHost 托管的 UWP 窗口，AUMID 挂在被托管的子窗口上，
    // 所以挨个走子窗口，谁给得出来就用谁。
    use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;
    unsafe extern "system" fn cb(child: HWND, lparam: windows::Win32::Foundation::LPARAM) -> windows::core::BOOL {
        let slot = &mut *(lparam.0 as *mut Option<String>);
        if slot.is_none() {
            if let Some(a) = aumid_of(child) {
                *slot = Some(a);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<String> = None;
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(cb), windows::Win32::Foundation::LPARAM(&mut found as *mut _ as isize));
    }
    found
}

fn aumid_of(hwnd: HWND) -> Option<String> {
    let f = gpsfw()?;
    unsafe {
        let mut pv: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = f(hwnd, &IPropertyStore::IID, &mut pv);
        if hr.is_err() || pv.is_null() {
            if std::env::var("SKY_TRACE").is_ok() {
                eprintln!("[trace] GPSFW hwnd={hwnd:?} hr={hr:?}");
            }
            return None;
        }
        let store: IPropertyStore = IPropertyStore::from_raw(pv);
        let mut var = store.GetValue(&PKEY_APPUSERMODEL_ID as *const PROPERTYKEY).ok()?;
        let inner = &*var.Anonymous.Anonymous;
        let vt = inner.vt;
        let mut out = None;
        // VT_LPWSTR 的值是 31
        if vt == VARENUM(31) {
            let pw = inner.Anonymous.pwszVal.0;
            if !pw.is_null() {
                let mut len = 0usize;
                while *pw.add(len) != 0 {
                    len += 1;
                    if len > 4096 {
                        break;
                    }
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(pw, len)));
            }
        }
        let _ = PropVariantClear(&mut var);
        drop(store);
        out.filter(|s| !s.trim().is_empty())
    }
}

fn launch_source_aumid(hwnd: HWND) -> Option<String> {
    use windows::Win32::UI::Shell::ILaunchSourceAppUserModelId;
    let f = gpsfw()?;
    unsafe {
        let mut pv: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = f(hwnd, &ILaunchSourceAppUserModelId::IID, &mut pv);
        if std::env::var("SKY_TRACE").is_ok() {
            eprintln!("[trace] LSAUMID hwnd={hwnd:?} hr={hr:?}");
        }
        if hr.is_err() || pv.is_null() {
            return None;
        }
        let src: ILaunchSourceAppUserModelId = ILaunchSourceAppUserModelId::from_raw(pv);
        let pw = src.GetAppUserModelId().ok()?;
        if pw.0.is_null() {
            return None;
        }
        let s = pw.to_string().ok();
        CoTaskMemFree(Some(pw.0 as *const _));
        s.filter(|s| !s.trim().is_empty())
    }
}

pub fn process_image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 4096];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(h);
        if ok && len > 0 {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

/// 这些宿主/壳进程从来不代表窗口自己的身份（真 helper 里是同一份名单，它
/// 的字符串里一个个都出现过）。
pub const EXCLUDED_HOST_PROCESSES: &[&str] = &[
    "applicationframehost.exe",
    "backgroundtaskhost.exe",
    "_proxy.exe",
    "ctfmon.exe",
    "inputapp.exe",
    "lockapp.exe",
    "appruntimebroker.exe",
    "searchapp.exe",
    "searchhost.exe",
    "shellexperiencehost.exe",
    "sihost.exe",
    "startmenuexperiencehost.exe",
    "tabtip.exe",
    "textinputhost.exe",
];

fn is_excluded_host_process(image_path: &str) -> bool {
    let name = image_path.rsplit('\\').next().unwrap_or("");
    EXCLUDED_HOST_PROCESSES
        .iter()
        .any(|e| name.eq_ignore_ascii_case(e))
}

/// 窗口身份（spec §C.1）：shell 往窗口上盖了 AUMID 就用 AUMID（走属性仓
/// 库），没有就用拼出来的 `process:<image path>`。装在商店目录下但没包身份
/// 的进程、以及壳宿主进程，都认不出来（真 helper 也跳过它们）。
pub fn app_id_for_window(hwnd: HWND, pid: u32) -> Option<String> {
    if let Some(aumid) = window_aumid(hwnd) {
        return Some(aumid);
    }
    let path = process_image_path(pid)?;
    let name = path.rsplit('\\').next().unwrap_or("").to_lowercase();
    // Chromium 系浏览器按产品名认，不按路径（实测：Chrome 窗口的 app 是
    // "Chrome"，Edge 窗口的是 "MSEdge"）。
    // 白名单顺序（chrome/msedge/brave/opera）跟原版二进制里内嵌的 exe 名单
    // 一致。
    match name.as_str() {
        "chrome.exe" => return Some("Chrome".into()),
        "msedge.exe" => return Some("MSEdge".into()),
        "brave.exe" => return Some("Brave".into()),
        "opera.exe" => return Some("Opera".into()),
        _ => {}
    }
    if path.to_lowercase().contains("\\windowsapps\\") {
        // 打包应用（UWP/WinUI3）的窗口：从进程的包身份推出 AUMID（实测：
        // 现在的记事本窗口，就算窗口上没有 shell 的 AUMID 属性，原版也报它
        // 的 AUMID）。
        if let Some(full) = package_full_name_for_pid(pid) {
            // 终端这一类窗口是故意排除的。原版把 WindowsTerminal 的 AUMID
            // 和它的终端 token 名单摆在一起：conhost/fluentterminal/
            // git-bash/hyperkit/wsl/wt。
            if full.to_lowercase().starts_with("microsoft.windowsterminal_") {
                return None;
            }
            if let Some(a) = aumid_from_package_full_name(&full) {
                return Some(a);
            }
        }
        return None;
    }
    if is_excluded_host_process(&path) {
        return None;
    }
    Some(format!("process:{path}"))
}

/// 进程的包全名（没打包的进程返回 None）。
pub fn package_full_name_for_pid(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    type GetPackageFullNameFn = unsafe extern "system" fn(HANDLE, *mut u32, *mut u16) -> u32;
    let f: Option<GetPackageFullNameFn> = unsafe {
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
        let h = LoadLibraryA(windows::core::s!("kernel32.dll")).ok()?;
        GetProcAddress(h, windows::core::s!("GetPackageFullName"))
            .map(|a| std::mem::transmute::<*mut core::ffi::c_void, GetPackageFullNameFn>(a as *mut _))
    };
    let f = f?;
    let trace = std::env::var("SKY_TRACE").is_ok();
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let mut len = 0u32;
                let rc0 = f(h, &mut len, std::ptr::null_mut());
                if trace {
                    eprintln!("[trace] GetPackageFullName size pid={pid} rc={rc0} len={len}");
                }
                if len == 0 {
                    return None;
                }
                let mut buf = vec![0u16; len as usize];
                let rc = f(h, &mut len, buf.as_mut_ptr());
                if rc != 0 {
                    if trace {
                        eprintln!("[trace] GetPackageFullName fill pid={pid} rc={rc}");
                    }
                    return None;
                }
                let full = String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]);
                if trace {
                    eprintln!("[trace] GetPackageFullName pid={pid} full={full}");
                }
                if full.trim().is_empty() {
                    None
                } else {
                    Some(full)
                }
            }
            Err(e) => {
                if trace {
                    eprintln!("[trace] OpenProcess pid={pid} failed {e:?}");
                }
                None
            }
        }
    }
}

/// 包全名 → 应用 id → 主 AUMID（优先 `…!App` 那个）。
pub fn aumid_from_package_full_name(full_name: &str) -> Option<String> {
    use windows::Win32::Storage::Packaging::Appx::{
        ClosePackageInfo, GetPackageApplicationIds, OpenPackageInfoByFullName,
    };
    let trace = std::env::var("SKY_TRACE").is_ok();
    unsafe {
        let wide: Vec<u16> = full_name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut reference: *mut core::ffi::c_void = std::ptr::null_mut();
        if OpenPackageInfoByFullName(PCWSTR(wide.as_ptr()), None, &mut reference as *mut _ as *mut _)
            .is_err()
        {
            if trace {
                eprintln!("[trace] OpenPackageInfoByFullName failed for {full_name}");
            }
            return None;
        }
        let mut size = 0u32;
        let mut id_count = 0u32;
        // 探大小的调用一般返回 ERROR_INSUFFICIENT_BUFFER (122)。
        let qrc = GetPackageApplicationIds(reference as *const _, &mut size, None, Some(&mut id_count));
        if trace {
            eprintln!("[trace] GetPackageApplicationIds query rc={qrc:?} size={size} count={id_count}");
        }
        let ok = (qrc.is_ok() || qrc.0 == 122) && size > 0 && id_count > 0;
        let mut out = None;
        if ok {
            let mut buf = vec![0u8; size as usize];
            let rrc = GetPackageApplicationIds(reference as *const _, &mut size, Some(buf.as_mut_ptr()), Some(&mut id_count));
            if rrc.is_ok() || rrc.0 == 122 {
                // 内存布局（在 win11 24H2 上看到的）：开头一个 u64 的
                // reference，后面是首尾相接、NUL 结尾的 UTF-16 应用 id。
                let mut ids: Vec<String> = Vec::new();
                let mut cur: Vec<u16> = Vec::new();
                for c in buf[8.min(buf.len())..].chunks_exact(2) {
                    let u = u16::from_le_bytes([c[0], c[1]]);
                    if u == 0 {
                        if !cur.is_empty() {
                            let s = String::from_utf16_lossy(&cur);
                            if s.contains('!') {
                                ids.push(s);
                            }
                            cur.clear();
                        }
                    } else {
                        cur.push(u);
                    }
                }
                if trace {
                    eprintln!("[trace] app ids: {ids:?}");
                }
                // 优先主应用 id（…!App 那种），没有就取第一个。
                out = ids
                    .iter()
                    .find(|id| id.rsplit('!').next().unwrap_or("").eq_ignore_ascii_case("App"))
                    .cloned()
                    .or_else(|| ids.into_iter().next());
            }
        }
        let _ = ClosePackageInfo(reference as *const _);
        out
    }
}

/// 打包进程（AFH 托管的 UWP 内容进程）的 AUMID，走包全名 → 应用 id。
pub fn aumid_for_hosted_pid(pid: u32) -> Option<String> {
    let full = package_full_name_for_pid(pid)?;
    aumid_from_package_full_name(&full)
}

/// 给人看的应用名：打包应用用 shell 显示名，exe 用产品名，最后才退回 exe
/// 的主文件名。
pub fn app_display_name(app_id: &str) -> String {
    if looks_like_aumid(app_id) {
        if let Some(name) = aumid_display_name(app_id) {
            return name;
        }
    }
    let path = app_id.strip_prefix("process:").unwrap_or(app_id);
    // shell 显示名优先：它跟 helper 报出来的开始菜单品牌名一致（比如
    // "TechPowerUp GPU-Z"、本地化过的名字，等等）。
    if let Some(item) = shell_item_for(path) {
        if let Ok(name) = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) } {
            if !name.0.is_null() {
                let s = unsafe { name.to_string().unwrap_or_default() };
                let s = s.trim();
                if !s.is_empty() {
                    // 解析出来的显示名带着扩展名。helper 报的名字从来不带，
                    // 所以这里砍掉。
                    if s.len() > 4 && s.to_lowercase().ends_with(".exe") {
                        return s[..s.len() - 4].to_string();
                    }
                    return s.to_string();
                }
            }
        }
    }
    if let Some(pn) = product_name(path) {
        return pn;
    }
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| app_id.to_string())
}

/// 查显示名共用的 SHCreateItemFromParsingName 小工具。
fn shell_item_for(path: &str) -> Option<IShellItem> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok() }
}

pub fn looks_like_aumid(id: &str) -> bool {
    // 包族那种形式，比如 Microsoft.WindowsNotepad_8wekyb3d8bbwe!App
    (id.contains('_') && id.contains('!')) || id.starts_with("windows.")
}

fn aumid_display_name(aumid: &str) -> Option<String> {
    unsafe {
        let path = format!("shell:AppsFolder\\{aumid}");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let name: PWSTR = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        if name.0.is_null() {
            return None;
        }
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        s.filter(|s: &String| !s.trim().is_empty())
    }
}

/// 从 VS_VERSION_INFO 里取 ProductName，尽力而为。
pub fn product_name(exe_path: &str) -> Option<String> {
    let wide: Vec<u16> = exe_path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None) as usize;
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size as u32, data.as_mut_ptr() as *mut _).ok()?;
        let sub: Vec<u16> = "\\VarFileInfo\\Translation"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len: u32 = 0;
        if !VerQueryValueW(data.as_ptr() as *const _, PCWSTR(sub.as_ptr()), &mut ptr, &mut len).as_bool()
            || ptr.is_null()
            || len < 4
        {
            return None;
        }
        let translations = std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize);
        let lang = translations[0];
        let cp = translations[1];
        let key = format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\ProductName");
        let keyw: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
        if !VerQueryValueW(data.as_ptr() as *const _, PCWSTR(keyw.as_ptr()), &mut ptr, &mut len).as_bool()
            || ptr.is_null()
            || len == 0
        {
            return None;
        }
        let chars = std::slice::from_raw_parts(ptr as *const u16, (len - 1).max(0) as usize);
        let s = String::from_utf16_lossy(chars);
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// 暴露出来，目录那边才能用 CoCreateInstance 加载 .lnk 文件。
pub const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_C000_000000000046);
