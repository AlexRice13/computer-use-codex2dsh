//! Start-menu application catalog.
//!
//! The shell "AppsFolder" namespace (what Get-StartApps enumerates) is the
//! helper's curated application source. Namespace handlers refuse to bind
//! from this process's COM context, so the catalog is fetched through
//! PowerShell once per half hour and kept in a small on-disk cache; list
//! responses after the first call never touch the shell again.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const TTL_MS: u64 = 30 * 60 * 1000;

/// (display name, application id) pairs as reported by Get-StartApps.
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
        // Fall back to the previous cache even when stale; better than an
        // empty catalog when PowerShell is unavailable.
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
    let ps = PathBuf::from(crate::win::taskbar::powershell_path());
    let out = std::process::Command::new(ps)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-StartApps | ConvertTo-Json -Compress",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim_start_matches('\u{feff}').to_string();
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Single-app results serialize as one object rather than an array.
    match &root {
        serde_json::Value::Array(_) => parse_entries(&root),
        serde_json::Value::Object(_) => parse_entries(&serde_json::Value::Array(vec![root])),
        _ => None,
    }
}
