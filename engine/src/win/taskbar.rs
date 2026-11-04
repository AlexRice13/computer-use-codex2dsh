// Taskbar AppUserModelIDs via UI Automation.
//
// The helper reports taskbar-pinned identities (e.g. Electron apps that call
// app.setAppUserModelId) as the running-app id. That identity is only
// readable from the Explorer taskbar button: its UIA element carries
// AutomationId "Appid: <AUMID>". Query Explorer through a PowerShell
// UIAutomation probe (same subprocess pattern as the Start-menu catalog) and
// cache the result; a stale cache is used when the probe fails.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCRIPT: &str = r#"
$ErrorActionPreference='SilentlyContinue'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, WindowsBase | Out-Null
$root=[System.Windows.Automation.AutomationElement]::RootElement
$cond=New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty,'Shell_TrayWnd')
$tb=$root.FindFirst([System.Windows.Automation.TreeScope]::Children,$cond)
$out=@()
if($tb){
  $bc=New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ControlTypeProperty,[System.Windows.Automation.ControlType]::Button)
  $btns=$tb.FindAll([System.Windows.Automation.TreeScope]::Descendants,$bc)
  foreach($b in $btns){
    $aid=$b.Current.AutomationId
    if($aid -and $aid.StartsWith('Appid:')){
      $out += [pscustomobject]@{ name=$b.Current.Name; aid=$aid.Substring(6).Trim() }
    }
  }
}
"#;

const TTL: Duration = Duration::from_secs(5 * 60);

/// title-lower -> AUMID
pub fn appids() -> HashMap<String, String> {
    cache().read()
}

struct Cache {
    inner: Mutex<HashMap<String, String>>,
    fetched_at: Mutex<Option<Instant>>,
    inflight: Mutex<bool>,
    cv: Condvar,
}

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Cache {
        inner: Mutex::new(HashMap::new()),
        fetched_at: Mutex::new(None),
        inflight: Mutex::new(false),
        cv: Condvar::new(),
    })
}

impl Cache {
    fn read(&self) -> HashMap<String, String> {
        dbg_log(&format!("[read] pid={} ts={}", std::process::id(), now_ms()));
        {
            let map = self.inner.lock().unwrap();
            let fresh = self
                .fetched_at
                .lock()
                .unwrap()
                .map(|t| t.elapsed() < TTL)
                .unwrap_or(false);
            if fresh && !map.is_empty() {
                return map.clone();
            }
        }
        // Refresh once; other callers wait briefly for the result.
        let started = {
            let mut busy = self.inflight.lock().unwrap();
            if *busy {
                let (g, _) = self.cv.wait_timeout(busy, Duration::from_secs(8)).unwrap();
                drop(g);
                return self.inner.lock().unwrap().clone();
            }
            *busy = true;
            true
        };
        let _ = started;
        let result = probe();
        let mut busy = self.inflight.lock().unwrap();
        *busy = false;
        self.cv.notify_all();
        drop(busy);
        if let Some(map) = result {
            if !map.is_empty() {
                *self.inner.lock().unwrap() = map.clone();
                *self.fetched_at.lock().unwrap() = Some(Instant::now());
                return map;
            }
        }
        // Failure: keep serving the stale snapshot.
        self.inner.lock().unwrap().clone()
    }
}

pub fn powershell_path() -> String {
    // The engine may run with a stripped environment (no WINDIR/PATH), so
    // fall back to the well-known location.
    for var in ["WINDIR", "SystemRoot", "windir"] {
        if let Ok(w) = std::env::var(var) {
            if !w.trim().is_empty() {
                return format!("{w}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
            }
        }
    }
    "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".to_string()
}

/// Scratch directory; tests point SKY_TMP_DIR at a writable path when the
/// default temp dir is not writable in their sandbox. The engine may run
/// with a stripped environment (no TMP/TEMP/LOCALAPPDATA), so the last
/// resort is the executable's own directory (always writable for us).
pub(crate) fn tmp_dir() -> PathBuf {
    for var in ["SKY_TMP_DIR", "TMP", "TEMP", "TMPDIR"] {
        if let Ok(d) = std::env::var(var) {
            if !d.trim().is_empty() {
                let p = PathBuf::from(d);
                if p.is_dir() {
                    return p;
                }
            }
        }
    }
    if let Ok(l) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(l).join("dsh-sky-engine");
        let _ = std::fs::create_dir_all(&p);
        if p.is_dir() {
            return p;
        }
    }
    let sys = std::env::temp_dir();
    if sys.is_dir() && std::fs::OpenOptions::new().create(true).append(true).open(sys.join(".dsh-wtest")).is_ok()
    {
        let _ = std::fs::remove_file(sys.join(".dsh-wtest"));
        return sys;
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or(sys)
}

fn dbg_log(msg: &str) {
    if std::env::var("SKY_TRACE").is_err() {
        return;
    }
    let path = tmp_dir().join("dsh-sky-taskbar-debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "{}", msg);
    }
}

fn probe() -> Option<HashMap<String, String>> {
    let ps = powershell_path();
    // Write the JSON to a temp file instead of piping stdout: a piped
    // console child breaks UIAutomation's cross-process channel from this
    // process type (verified: pipe → empty, file → complete).
    let tmp = tmp_dir().join(format!("dsh-sky-taskbar-{}.json", std::process::id()));
    let cmd = format!(
        "{SCRIPT}\n\"{0}\" | Set-Content -LiteralPath '{1}' -Encoding UTF8",
        "$($out | ConvertTo-Json -Compress)",
        tmp.display()
    );
    let script_path = tmp_dir().join(format!("dsh-sky-taskbar-{}.ps1", std::process::id()));
    // PS 5.1 needs a BOM to read UTF-8 scripts correctly.
    let mut script_bytes = vec![0xEFu8, 0xBB, 0xBF];
    script_bytes.extend_from_slice(cmd.as_bytes());
    if let Err(e) = std::fs::write(&script_path, &script_bytes) {
        if std::env::var("SKY_TRACE").is_ok() {
            eprintln!("[trace] taskbar script write failed path={} err={e}", script_path.display());
        }
        return None;
    }
    if std::env::var("SKY_TRACE").is_ok() {
        eprintln!("[trace] taskbar script written {}", script_path.display());
    }
    let spawned = std::process::Command::new(ps.clone())
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", &script_path.to_string_lossy()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let child = match spawned {
        Ok(c) => c,
        Err(e) => {
            dbg_log(&format!("[spawn-fail] ps={ps} err={e}"));
            if std::env::var("SKY_TRACE").is_ok() {
                eprintln!("[trace] taskbar spawn failed ps={ps} err={e}");
            }
            return None;
        }
    };
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            dbg_log(&format!("[wait-fail] err={e}"));
            return None;
        }
    };
    if !out.status.success() {
        dbg_log(&format!(
            "[status-fail] {:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>()
        ));
        if std::env::var("SKY_TRACE").is_ok() {
            eprintln!(
                "[trace] taskbar probe failed ps={ps} status={:?} stderr={}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>()
            );
        }
        return None;
    }
    let raw = std::fs::read(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);
    let text = match raw {
        Some(b) => {
            String::from_utf8_lossy(b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&b))
                .to_string()
        }
        None => {
            dbg_log("[no-json-file]");
            return None;
        }
    };
    let trimmed = text.trim();
    dbg_log(&format!(
        "[probe] status={:?} json_len={} head={}",
        out.status.code(),
        trimmed.len(),
        trimmed.chars().take(160).collect::<String>()
    ));
    if std::env::var("SKY_TRACE").is_ok() {
        eprintln!(
            "[trace] taskbar stdout({})={}",
            trimmed.len(),
            trimmed.chars().take(240).collect::<String>()
        );
    }
    if trimmed.is_empty() || trimmed == "null" {
        return Some(HashMap::new());
    }
    let values: Vec<serde_json::Value> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).ok()?
    } else {
        vec![serde_json::from_str(trimmed).ok()?]
    };
    let mut map = HashMap::new();
    for v in values {
        let (Some(name), Some(aid)) = (v.get("name").and_then(|n| n.as_str()), v.get("aid").and_then(|a| a.as_str())) else {
            continue;
        };
        if let Some(title) = button_title(name) {
            map.insert(title.to_lowercase(), aid.to_string());
        }
    }
    Some(map)
}

/// "DeepSeek Harness - 1 个运行窗口" → "DeepSeek Harness"
fn button_title(name: &str) -> Option<&str> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    // The count suffix is localized; the stable separator is " - N " where N
    // is a number, optionally followed by anything.
    if let Some(idx) = name.rfind(" - ") {
        let tail = &name[idx + 3..];
        let digits_len = tail.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits_len > 0 {
            let after = tail[digits_len..].trim_start();
            if after.is_empty() || after.starts_with(char::is_whitespace) || starts_with_cjk_count(after) {
                let head = name[..idx].trim();
                if !head.is_empty() {
                    return Some(head);
                }
            }
        }
    }
    Some(name)
}

fn starts_with_cjk_count(s: &str) -> bool {
    // Localized forms put the unit right after the number ("3 个运行窗口").
    s.starts_with(char::is_whitespace)
        || s.starts_with('个')
        || s.starts_with('個')
        || s.starts_with("running")
}

#[allow(dead_code)]
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[allow(dead_code)]
fn cache_path() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|l| PathBuf::from(l).join("dsh-sky-engine").join("taskbar-aumids.json"))
        .unwrap_or_default()
}
