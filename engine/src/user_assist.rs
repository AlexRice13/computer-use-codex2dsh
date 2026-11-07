// UserAssist 里记着用户跑过哪些程序（HKCU），原版 helper 那份挑过的
// list_apps 条目就是从这儿来的（lastUsedDate / useCount，连条目集合本身也是）。
//
// 值名是 ROT13 编码过的路径。这台系统（Win11 24H2，头部标记 0x68）上 72 字节
// 的数据里，0x04 处的 u32 是运行次数，0x40 处的 u64 是最后运行的 FILETIME。
// 拿六个已知应用跟原版 helper 的 useCount/lastUsedDate 对过，位置就是这么定的。

use std::collections::HashMap;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    REG_SAM_FLAGS,
};
use windows::Win32::System::Time::FileTimeToSystemTime;

#[derive(Clone, Debug)]
pub struct AssistInfo {
    /// ROT13 解出来的值名：一个 exe 路径、`{GUID}\…` 形式的已知文件夹路径、
    /// 一个 `.lnk` 路径，或者裸 AUMID。
    pub decoded: String,
    pub use_count: u32,
    pub last_used_date: Option<String>,
}

fn rot13(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' => (b'a' + ((c as u8 - b'a' + 13) % 26)) as char,
            'A'..='Z' => (b'A' + ((c as u8 - b'A' + 13) % 26)) as char,
            _ => c,
        })
        .collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn filetime_to_utc_date(ft: u64) -> Option<String> {
    if ft < 116_444_736_000_000_000 {
        return None;
    }
    let f = FILETIME {
        dwLowDateTime: (ft & 0xffff_ffff) as u32,
        dwHighDateTime: (ft >> 32) as u32,
    };
    unsafe {
        let mut st = SYSTEMTIME::default();
        FileTimeToSystemTime(&f, &mut st).ok()?;
        Some(format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay))
    }
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .unwrap_or(0)
}

fn le_u64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8)
        .map(|w| u64::from_le_bytes(w.try_into().unwrap()))
        .unwrap_or(0)
}

unsafe fn open(parent: HKEY, sub: &str) -> Option<HKEY> {
    let mut h = HKEY::default();
    if RegOpenKeyExW(
        parent,
        PCWSTR(wide(sub).as_ptr()),
        None,
        REG_SAM_FLAGS(KEY_READ.0),
        &mut h,
    )
    .is_ok()
    {
        Some(h)
    } else {
        None
    }
}

unsafe fn subkey_names(h: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    let mut idx = 0u32;
    loop {
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let rc = RegEnumKeyExW(
            h,
            idx,
            Some(windows::core::PWSTR(buf.as_mut_ptr())),
            &mut len,
            None,
            None,
            None,
            None,
        );
        if rc.is_err() || len == 0 {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
        idx += 1;
    }
    out
}

/// 把所有 GUID 子键下的 UserAssist 应用启动记录都收进来。
pub fn collect() -> HashMap<String, AssistInfo> {
    let trace = std::env::var("SKY_TRACE").is_ok();
    let mut map: HashMap<String, AssistInfo> = HashMap::new();
    unsafe {
        let root = match open(
            HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\UserAssist",
        ) {
            Some(h) => h,
            None => {
                if trace {
                    eprintln!("[trace] UA root open failed");
                }
                return map;
            }
        };
        let guids = subkey_names(root);
        if trace {
            eprintln!("[trace] UA guids={}", guids.len());
        }
        // 原版只认应用启动这一个 GUID；RunMRU 之类的只会加噪音（TaskBar /
        // ImplicitAppShortcuts 的 lnk、只跑过一次的 exe）。
        const APP_LAUNCH_GUID: &str =
            "{CEBFF5CD-ACE2-4F4F-9178-9926F41749EA}";
        let guids: Vec<String> = guids
            .into_iter()
            .filter(|g| g.eq_ignore_ascii_case(APP_LAUNCH_GUID))
            .collect();
        for guid in guids {
            let Some(gh) = open(root, &guid) else { continue };
            let Some(count) = open(gh, "Count") else {
                let _ = RegCloseKey(gh);
                continue;
            };
            let mut idx = 0u32;
            let mut kept = 0usize;
            loop {
                let mut nbuf = [0u16; 4096];
                let mut nlen = nbuf.len() as u32;
                let mut data = vec![0u8; 4096];
                let mut dlen = data.len() as u32;
                let rc = RegEnumValueW(
                    count,
                    idx,
                    Some(windows::core::PWSTR(nbuf.as_mut_ptr())),
                    &mut nlen,
                    None,
                    None,
                    Some(data.as_mut_ptr()),
                    Some(&mut dlen),
                );
                if rc.0 == 122 {
                    // 122 是缓冲区不够，按它给的真实大小再读一次。
                    data = vec![0u8; dlen as usize];
                    nlen = nbuf.len() as u32;
                    let mut dlen2 = dlen;
                    let rc2 = RegEnumValueW(
                        count,
                        idx,
                        Some(windows::core::PWSTR(nbuf.as_mut_ptr())),
                        &mut nlen,
                        None,
                        None,
                        Some(data.as_mut_ptr()),
                        Some(&mut dlen2),
                    );
                    if rc2.is_err() {
                        idx += 1;
                        continue;
                    }
                } else if rc.0 != 0 {
                    break;
                }
                if nlen == 0 {
                    idx += 1;
                    continue;
                }
                if data.len() < 0x48 {
                    idx += 1;
                    continue;
                }
                let name = String::from_utf16_lossy(&nbuf[..nlen as usize]);
                let decoded = rot13(&name).trim().to_string();
                if decoded.is_empty() {
                    idx += 1;
                    continue;
                }
                if trace && decoded.to_lowercase().contains("uu_laun") {
                    eprintln!(
                        "[trace] UA raw guid={guid} decoded={decoded} dlen={} f40={:X} uc={}",
                        dlen,
                        if data.len() >= 0x48 { le_u64(&data, 0x40) } else { 0 },
                        if data.len() >= 8 { le_u32(&data, 4) } else { 0 },
                    );
                }
                if trace {
                    let dl = decoded.to_lowercase();
                    if dl.contains("notepad") || dl.contains("terminal") || dl.contains("screenshots")
                        || dl.contains("chrome") || dl.contains("msedge") || dl.contains("cloudmusic")
                        || dl.contains("visualstudiocode") || dl.contains("explorer")
                    {
                        eprintln!("[trace] UA probe2 guid={guid} decoded={decoded}");
                    }
                }
                let key = decoded.to_lowercase();
                let use_count = le_u32(&data, 4);
                // 数据里 0x40 处只放 FILETIME 的高 32 位（精度 429 秒，取个日期
                // 足够了），UTC。
                let last_used_date = filetime_to_utc_date(le_u32(&data, 0x40) as u64 * 4_294_967_296);
                let entry = map.entry(key).or_insert_with(|| AssistInfo {
                    decoded: decoded.clone(),
                    use_count: 0,
                    last_used_date: None,
                });
                if use_count > entry.use_count {
                    entry.use_count = use_count;
                }
                if last_used_date.is_some() && entry.last_used_date.is_none() {
                    entry.last_used_date = last_used_date;
                }
                kept += 1;
                idx += 1;
            }
            if trace {
                eprintln!("[trace] UA {guid} values kept={kept}");
            }
            let _ = RegCloseKey(count);
            let _ = RegCloseKey(gh);
        }
        let _ = RegCloseKey(root);
    }
    if trace {
        for k in map.keys() {
            if k.contains("uu_") || k.contains("wiztree") || k.contains("steam") || k.contains("lceda") {
                eprintln!("[trace] UA probe key [{k}]");
            }
        }
    }
    // 实测出来的底线：记录没有 last-used 日期时，原版照样输出 useCount: 0
    // （CPU-Z、clash-verge、Docker Desktop 都显示成 "0|-"），所以 userassist
    // 的记录一条都不丢，原样留着。
    map
}
