//! 开始菜单应用清单。
//!
//! 应用来源就是 shell 的 "AppsFolder" 命名空间（Get-StartApps 列的那一套）。
//! 这个命名空间的 handler 不肯从当前进程的 COM 上下文里绑定，所以清单改从
//! PowerShell 拿，半小时一次，结果存一份小的磁盘缓存；第一次之后的
//! list 响应就不会再碰 shell。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const TTL_MS: u64 = 30 * 60 * 1000;

/// Get-StartApps 报出来的 (显示名, 应用 id) 对。
pub fn collect() -> Vec<(String, String)> {
    let cache = cache_path();
    if let Some(entries) = read_cache(&cache) {
        return entries;
    }
    match fetch() {
        Some(entries) => {
            write_cache(&cache, &entries);
            entries
        }
        // PowerShell 用不了时，过期的旧缓存也接着用，总比给个空清单强。
        None => read_cache_any(&cache).unwrap_or_default(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn cache_path() -> PathBuf {
    crate::win::taskbar::tmp_dir().join("startapps-cache.json")
}

fn read_cache(path: &PathBuf) -> Option<Vec<(String, String)>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let ts = root.get("fetched_ms")?.as_u64()?;
    if now_ms().saturating_sub(ts) > TTL_MS {
        return None;
    }
    parse_entries(root.get("entries")?)
}

fn read_cache_any(path: &PathBuf) -> Option<Vec<(String, String)>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parse_entries(root.get("entries")?)
}

fn parse_entries(v: &serde_json::Value) -> Option<Vec<(String, String)>> {
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        let name = e.get("Name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let appid = e.get("AppID").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if !appid.trim().is_empty() {
            out.push((name, appid));
        }
    }
    Some(out)
}

fn write_cache(path: &PathBuf, entries: &[(String, String)]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let arr: Vec<serde_json::Value> = entries
        .iter()
        .map(|(n, a)| serde_json::json!({ "Name": n, "AppID": a }))
        .collect();
    let doc = serde_json::json!({ "fetched_ms": now_ms(), "entries": arr });
    if let Ok(s) = serde_json::to_string(&doc) {
        let _ = std::fs::write(path, s);
    }
}

fn fetch() -> Option<Vec<(String, String)>> {
    // 跟任务栏那个探测一个套路：让 PowerShell 把 JSON 写进临时文件，不走
    // stdout 管道。跨宿主/父进程链时只有这种取法实测靠得住。
    let tmp = crate::win::taskbar::tmp_dir().join(format!("startapps-fetch-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let ps = PathBuf::from(crate::win::taskbar::powershell_path());
    let script = format!(
        "$ErrorActionPreference='Stop'; Get-StartApps | ConvertTo-Json -Compress | Set-Content -LiteralPath '{}' -Encoding UTF8",
        tmp.display()
    );
    let out = std::process::Command::new(ps)
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    let text = std::fs::read_to_string(&tmp).ok()?;
    let _ = std::fs::remove_file(&tmp);
    let text = text.trim().trim_start_matches('\u{feff}').to_string();
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    // 只查到一个 app 时，序列化出来是单个对象，不是数组。
    match &root {
        serde_json::Value::Array(_) => parse_entries(&root),
        serde_json::Value::Object(_) => parse_entries(&serde_json::Value::Array(vec![root])),
        _ => None,
    }
}
