use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, GetWindowThreadProcessId};

use crate::catalog;
use crate::state::{EngineState, Response};
use crate::win::enumerate;

// ---------------------------------------------------------------------------
// Strict parameter parsing. The original helper parses every request into
// typed structs, so errors are byte-exact serde messages prefixed with
// `parse {method} params: `.
// ---------------------------------------------------------------------------

fn parse_params<T: DeserializeOwned>(method: &str, params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|e| format!("parse {method} params: {e}"))
}

fn always_true() -> bool {
    true
}

#[derive(Deserialize)]
struct WinRef {
    #[serde(default)]
    app: Option<String>,
    id: u64,
}

#[derive(Deserialize)]
struct WindowOnly {
    window: WinRef,
}

#[derive(Deserialize)]
struct GwsParams {
    window: WinRef,
    #[serde(default = "always_true", rename = "include_screenshot")]
    include_screenshot: bool,
    #[serde(default = "always_true", rename = "include_text")]
    include_text: bool,
}

#[derive(Deserialize)]
struct TypeParams {
    window: WinRef,
    text: String,
}

#[derive(Deserialize)]
struct KeyParams {
    window: WinRef,
    key: String,
}

#[derive(Deserialize)]
struct ClickParams {
    window: WinRef,
    x: i32,
    y: i32,
    /// The original parses coordinates but never requires the id; the
    /// wrapper enforces freshness on its side.
    #[serde(default, rename = "screenshotId")]
    #[allow(dead_code)]
    screenshot_id: Option<String>,
    #[serde(default, rename = "mouse_button")]
    mouse_button: Option<String>,
    #[serde(default, rename = "click_count")]
    click_count: Option<u64>,
}

#[derive(Deserialize)]
struct ScrollParams {
    window: WinRef,
    x: i32,
    y: i32,
    #[serde(default, rename = "screenshotId")]
    #[allow(dead_code)]
    screenshot_id: Option<String>,
    #[serde(default, rename = "scrollX")]
    scroll_x: i64,
    #[serde(default, rename = "scrollY")]
    scroll_y: i64,
}

#[derive(Deserialize)]
struct DragParams {
    window: WinRef,
    #[serde(rename = "from_x")]
    from_x: i32,
    #[serde(rename = "from_y")]
    from_y: i32,
    #[serde(rename = "to_x")]
    to_x: i32,
    #[serde(rename = "to_y")]
    to_y: i32,
    #[serde(default, rename = "screenshotId")]
    #[allow(dead_code)]
    screenshot_id: Option<String>,
}

#[derive(Deserialize)]
struct ElementParams {
    window: WinRef,
    #[serde(rename = "element_index")]
    element_index: u64,
    #[serde(default, rename = "mouse_button")]
    mouse_button: Option<String>,
    #[serde(default, rename = "click_count")]
    click_count: Option<u64>,
    #[serde(default, rename = "scrollX")]
    scroll_x: i64,
    #[serde(default, rename = "scrollY")]
    scroll_y: i64,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    action: Option<String>,
}

#[derive(Deserialize)]
struct GetWindowFlat {
    #[serde(default)]
    app: Option<String>,
    id: u64,
}

#[derive(Deserialize)]
struct LaunchParams {
    app: String,
}

// ---------------------------------------------------------------------------
// Window resolution — the original's order and exact messages:
//   id == 0        -> "window id is required"
//   pid missing    -> "foreground window did not report a process id"
//   unaddressable  -> "window is not a usable app window"
//   no app given   -> "window.app is required"
//   app mismatch   -> "window id {id} no longer belongs to {app}; current owner is {owner}"
// ---------------------------------------------------------------------------

struct Resolved {
    hwnd: HWND,
    origin: (i32, i32),
    size: (i32, i32),
    owner: String,
    pid: u32,
}

fn resolve_win(w: &WinRef) -> Result<Resolved, String> {
    if w.id == 0 {
        return Err("window id is required".into());
    }
    let hwnd = HWND(w.id as *mut core::ffi::c_void);
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0 {
        return Err("foreground window did not report a process id".into());
    }
    // The original checks the app hint before resolving the owner.
    let app = w
        .app
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "window.app is required".to_string())?;
    // Owner identity: enumerate's (taskbar-corrected) form first, then the
    // shell property store / image-path fallback.
    let owner = enumerate::enumerate_top_level()
        .into_iter()
        .find(|r| r.hwnd as u64 == w.id)
        .map(|r| r.app)
        .or_else(|| crate::win::identity::app_id_for_window(hwnd, pid))
        .ok_or_else(|| "window is not a usable app window".to_string())?;
    if !owner.eq_ignore_ascii_case(app) {
        return Err(format!(
            "window id {} no longer belongs to {}; current owner is {}",
            w.id, app, owner
        ));
    }
    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
    }
    Ok(Resolved {
        hwnd,
        origin: (rect.left, rect.top),
        size: (rect.right - rect.left, rect.bottom - rect.top),
        owner,
        pid,
    })
}

// ---------------------------------------------------------------------------
// Per-app approval gate (spec §B.1). The original gates every app-touching
// operation behind a first-touch approvalRequest, even when the (stub)
// policy answers default_app_access=allow. Enumeration/diagnostic methods
// are not gated. Canonical identity shapes (observed):
//   process windows -> lowercase file name ("deepseek harness.exe")
//   packaged apps   -> the AUMID verbatim
//   browsers        -> lowercase exe file name + riskLevel "high"
// ---------------------------------------------------------------------------

/// Chromium-family product tokens produced by identity::app_id_for_window.
pub fn browser_exe(owner: &str) -> Option<&'static str> {
    match owner {
        "Chrome" => Some("chrome.exe"),
        "MSEdge" => Some("msedge.exe"),
        "Brave" => Some("brave.exe"),
        "Opera" => Some("opera.exe"),
        _ => None,
    }
}

/// True when the resolved owner is a whitelisted browser window.
pub fn is_browser_owner(owner: &str) -> bool {
    browser_exe(owner).is_some()
}

/// Byte-exact refusal the original emits whenever it cannot pin down the
/// active URL of a browser window (measured: every browser touch in this
/// environment ends up on this branch — the original has no reliable URL
/// channel here). The turn stays stopped until end_turn.
pub const BROWSER_URL_POLICY_REFUSAL: &str = "Computer Use has been stopped for this turn because it could not determine the current browser URL on Windows with enough confidence to enforce policy. Stop your work and send a final message noting why Computer Use ended.";

fn canonical_approval_app(owner: &str) -> (String, String) {
    if let Some(exe) = browser_exe(owner) {
        let display = match owner {
            "Chrome" => "Google Chrome",
            "MSEdge" => "Microsoft Edge",
            "Brave" => "Brave Browser",
            _ => "Opera",
        };
        return (exe.to_string(), display.to_string());
    }
    if let Some(path) = owner
        .strip_prefix("process:")
        .or_else(|| owner.strip_prefix("process:\\"))
    {
        let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
        let display = file
            .strip_suffix(".exe")
            .or_else(|| file.strip_suffix(".EXE"))
            .unwrap_or(file)
            .to_string();
        return (file.to_lowercase(), display);
    }
    // AUMID: the raw Get-StartApps table is the original's display-name
    // source (catalog rows alone miss unused packages like Calculator).
    let display = crate::startapps::collect()
        .into_iter()
        .find(|(_n, id)| id.eq_ignore_ascii_case(owner))
        .map(|(n, _)| n)
        .filter(|d| !d.trim().is_empty())
        .or_else(|| {
            let recs = enumerate::enumerate_top_level();
            let cats = catalog::build_catalog(&recs);
            cats.iter()
                .find(|c| c.id.eq_ignore_ascii_case(owner))
                .and_then(|c| c.display_name.clone())
                .filter(|d| !d.is_empty())
        })
        .unwrap_or_else(|| {
            owner
                .rsplit(['!', '_'])
                .next()
                .unwrap_or(owner)
                .to_string()
        });
    (owner.to_string(), display)
}

/// Process file name used by the tree header line and lastWindowIdentity.
fn process_file_name(owner: &str) -> String {
    owner
        .strip_prefix("process:")
        .or_else(|| owner.strip_prefix("process:\\"))
        .map(|p| p.rsplit(['\\', '/']).next().unwrap_or(p).to_string())
        .unwrap_or_default()
}

/// The `App:` field of the tree header line: file name for process
/// windows, shell display name for packaged apps.
fn header_app_name(owner: &str) -> String {
    let file = process_file_name(owner);
    if !file.is_empty() {
        return file;
    }
    canonical_approval_app(owner).1
}

/// Returns Some(approval response) when the app has not been approved in
/// this turn yet; None when execution may proceed.
fn approval_required(st: &EngineState, owner: &str) -> Option<Response> {
    let (app_key, display_name) = canonical_approval_app(owner);
    if st.approved_apps.contains(&app_key.to_lowercase()) {
        return None;
    }
    let risk = if is_browser_owner(owner) { "high" } else { "low" };
    Some(Response::Approval {
        app: app_key,
        display_name,
        risk_level: Some(risk.into()),
        allow_persistent_approval: true,
    })
}

/// After approval, browser windows hit the URL policy gate. In this
/// environment the original can never pin the active URL down, so every
/// approved browser touch lands on the byte-exact refusal (measured on
/// live Chrome and Edge windows); the rest of the turn stays stopped.
fn browser_policy_gate(st: &mut EngineState, owner: &str) -> Option<Response> {
    if is_browser_owner(owner) {
        st.browser_policy_stopped = true;
        return Some(Response::Error(BROWSER_URL_POLICY_REFUSAL.into()));
    }
    None
}

/// Resolve window + gate approval in one step for all touching methods.
fn resolve_and_approve(st: &mut EngineState, w: &WinRef) -> Result<Resolved, Response> {
    let r = resolve_win(w).map_err(Response::Error)?;
    if let Some(a) = approval_required(st, &r.owner) {
        return Err(a);
    }
    if let Some(b) = browser_policy_gate(st, &r.owner) {
        return Err(b);
    }
    Ok(r)
}

/// Latest-snapshot element lookup for element_index actions.
fn snapshot_element(
    st: &EngineState,
    r: &Resolved,
    index: u64,
) -> Result<(uiautomation::UIElement, (i32, i32, i32, i32)), String> {
    let snap = st
        .snapshots
        .get(&(r.hwnd.0 as isize))
        .ok_or_else(|| "no snapshot for window; call get_window_state first".to_string())?;
    let rec = snap
        .elements
        .get(index as usize)
        .ok_or_else(|| format!("element_index {index} is out of range for the latest snapshot ({} elements)", snap.elements.len()))?;
    Ok((rec.element.clone(), rec.rect))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Serialized RPC dispatch (the channel is single-threaded by contract).
pub fn dispatch(st: &mut EngineState, method: &str, params: &Value, meta: &Value) -> Response {
    // An approved re-send carries x-oai-cua-approved-app for this session.
    if let Some(app) = meta
        .get("x-oai-cua-approved-app")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
    {
        st.approved_apps.insert(app);
    }

    // Once the browser URL policy has stopped the turn, every touching
    // method keeps answering with the same refusal until end_turn.
    const TURN_SAFE: [&str; 6] = ["close", "end_turn", "diagnostic_state", "list_apps", "list_windows", "get_window"];
    if st.browser_policy_stopped && !TURN_SAFE.contains(&method) {
        return Response::Error(BROWSER_URL_POLICY_REFUSAL.into());
    }

    match method {
        "close" => Response::Result(json!({})),
        "end_turn" => {
            st.last_turn_key = None;
            st.approved_apps.clear();
            st.browser_policy_stopped = false;
            Response::Result(json!({}))
        }
        "diagnostic_state" => Response::Result(json!({
            "cacheDiagnostics": {
                "accessibilityRevision": st.snapshot_revision,
                "accessibilitySnapshotCount": st.snapshot_count,
                "captureCachedSessionCount": st
                    .snapshots
                    .values()
                    .filter(|s| !s.shot_ids.is_empty())
                    .count(),
            },
            "lastWindowIdentity": st.last_window_identity,
        })),

        "list_windows" => {
            let recs = enumerate::enumerate_top_level();
            let arr: Vec<Value> = recs
                .iter()
                .map(|r| json!({ "app": r.app, "id": r.hwnd, "title": r.title }))
                .collect();
            Response::Result(Value::Array(arr))
        }
        "get_window" => {
            let p: GetWindowFlat = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let recs = enumerate::enumerate_top_level();
            let hit = recs.into_iter().find(|r| {
                p.id == r.hwnd as u64
                    && p.app
                        .as_deref()
                        .map(|a| a.eq_ignore_ascii_case(&r.app))
                        .unwrap_or(true)
            });
            match hit {
                Some(r) => Response::Result(json!({ "app": r.app, "id": r.hwnd, "title": r.title })),
                // The JS client renders a null result as its own error; mirror
                // the raw contract.
                None => Response::Result(Value::Null),
            }
        }
        "list_apps" => {
            let recs = enumerate::enumerate_top_level();
            let apps = catalog::build_catalog(&recs);
            let arr: Vec<Value> = apps
                .into_iter()
                .map(|a| {
                    let mut v = json!({
                        "id": a.id,
                        "isRunning": a.is_running,
                        "windows": a
                            .windows
                            .iter()
                            .map(|w| json!({ "app": w.app, "id": w.hwnd, "title": w.title }))
                            .collect::<Vec<Value>>(),
                    });
                    if let Some(d) = a.display_name.filter(|s| !s.is_empty()) {
                        v["displayName"] = json!(d);
                    }
                    if let Some(d) = a.last_used_date {
                        v["lastUsedDate"] = json!(d);
                    }
                    if let Some(c) = a.use_count {
                        v["useCount"] = json!(c);
                    }
                    v
                })
                .collect();
            Response::Result(Value::Array(arr))
        }

        "activate_window" => {
            let p: WindowOnly = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window) {
                Ok(r) => {
                    crate::win::input::activate(r.hwnd);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "type_text" => {
            let p: TypeParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window) {
                Ok(_r) => {
                    crate::win::input::type_text(&p.text);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "press_key" => {
            let p: KeyParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window) {
                Ok(_r) => match crate::win::input::press_key(&p.key) {
                    Ok(()) => Response::Result(json!({})),
                    Err(e) => Response::Error(e),
                },
                Err(resp) => resp,
            }
        }
        "click" => {
            let p: ClickParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window) {
                Ok(r) => {
                    let x = r.origin.0 + p.x;
                    let y = r.origin.1 + p.y;
                    let button = match p.mouse_button.as_deref() {
                        Some("right") => crate::win::input::Button::Right,
                        Some("middle") => crate::win::input::Button::Middle,
                        _ => crate::win::input::Button::Left,
                    };
                    crate::win::input::click(x, y, button, p.click_count.unwrap_or(1) as u32);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "scroll" => {
            let p: ScrollParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window) {
                Ok(r) => {
                    let x = r.origin.0 + p.x;
                    let y = r.origin.1 + p.y;
                    crate::win::input::wheel(x, y, p.scroll_x as i32, p.scroll_y as i32);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "drag" => {
            let p: DragParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window) {
                Ok(r) => {
                    let from = (r.origin.0 + p.from_x, r.origin.1 + p.from_y);
                    let to = (r.origin.0 + p.to_x, r.origin.1 + p.to_y);
                    crate::win::input::drag(from, to);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }

        "click_element" => {
            let p: ElementParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window)
                .and_then(|r| snapshot_element(st, &r, p.element_index).map(|e| (r, e)).map_err(Response::Error))
            {
                Ok((_r, (_el, rect))) => {
                    let cx = rect.0 + rect.2 / 2;
                    let cy = rect.1 + rect.3 / 2;
                    crate::win::input::activate(_r.hwnd);
                    let button = match p.mouse_button.as_deref() {
                        Some("right") => crate::win::input::Button::Right,
                        Some("middle") => crate::win::input::Button::Middle,
                        _ => crate::win::input::Button::Left,
                    };
                    crate::win::input::click(cx, cy, button, p.click_count.unwrap_or(1) as u32);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "scroll_element" => {
            let p: ElementParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window)
                .and_then(|r| snapshot_element(st, &r, p.element_index).map(|e| (r, e)).map_err(Response::Error))
            {
                Ok((_r, (_el, rect))) => {
                    let cx = rect.0 + rect.2 / 2;
                    let cy = rect.1 + rect.3 / 2;
                    crate::win::input::wheel(cx, cy, p.scroll_x as i32, p.scroll_y as i32);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "set_value" => {
            let p: ElementParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let want = match p.value.clone() {
                Some(v) => v,
                None => return Response::Error(format!("parse {method} params: missing field `value`")),
            };
            match resolve_and_approve(st, &p.window)
                .and_then(|r| snapshot_element(st, &r, p.element_index).map(|e| (r, e)).map_err(Response::Error))
            {
                Ok((_r, (el, _rect))) => {
                    match el.get_pattern::<uiautomation::patterns::UIValuePattern>() {
                        Ok(pattern) => match pattern.set_value(&want) {
                            Ok(()) => Response::Result(json!({})),
                            Err(e) => Response::Error(format!("set value failed: {e}")),
                        },
                        Err(e) => {
                            Response::Error(format!("element does not support ValuePattern: {e}"))
                        }
                    }
                }
                Err(resp) => resp,
            }
        }
        "perform_secondary_action" => {
            let p: ElementParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let action = match p.action.clone() {
                Some(a) => a,
                None => {
                    return Response::Error(format!("parse {method} params: missing field `action`"))
                }
            };
            match resolve_and_approve(st, &p.window)
                .and_then(|r| snapshot_element(st, &r, p.element_index).map(|e| (r, e)).map_err(Response::Error))
            {
                Ok((_r, (el, rect))) => {
                    let cx = rect.0 + rect.2 / 2;
                    let cy = rect.1 + rect.3 / 2;
                    use uiautomation::patterns::{UIExpandCollapsePattern, UIScrollPattern};
                    use uiautomation::types::ScrollAmount;
                    let outcome: Result<(), String> = match action.as_str() {
                        "Expand" => el
                            .get_pattern::<UIExpandCollapsePattern>()
                            .map_err(|e| format!("element does not support ExpandCollapse: {e}"))
                            .and_then(|p| p.expand().map_err(|e| format!("expand failed: {e}"))),
                        "Collapse" => el
                            .get_pattern::<UIExpandCollapsePattern>()
                            .map_err(|e| format!("element does not support ExpandCollapse: {e}"))
                            .and_then(|p| p.collapse().map_err(|e| format!("collapse failed: {e}"))),
                        "Raise" => {
                            crate::win::input::activate(_r.hwnd);
                            Ok(())
                        }
                        "Scroll Up" => el
                            .get_pattern::<UIScrollPattern>()
                            .map_err(|e| format!("element does not support Scroll: {e}"))
                            .and_then(|p| {
                                p.scroll(ScrollAmount::NoAmount, ScrollAmount::SmallDecrement)
                                    .map_err(|e| format!("scroll failed: {e}"))
                            })
                            .or_else(|_| {
                                crate::win::input::wheel(cx, cy, 0, 3);
                                Ok(())
                            }),
                        "Scroll Down" => el
                            .get_pattern::<UIScrollPattern>()
                            .map_err(|e| format!("element does not support Scroll: {e}"))
                            .and_then(|p| {
                                p.scroll(ScrollAmount::NoAmount, ScrollAmount::SmallIncrement)
                                    .map_err(|e| format!("scroll failed: {e}"))
                            })
                            .or_else(|_| {
                                crate::win::input::wheel(cx, cy, 0, -3);
                                Ok(())
                            }),
                        "Scroll Left" => el
                            .get_pattern::<UIScrollPattern>()
                            .map_err(|e| format!("element does not support Scroll: {e}"))
                            .and_then(|p| {
                                p.scroll(ScrollAmount::SmallDecrement, ScrollAmount::NoAmount)
                                    .map_err(|e| format!("scroll failed: {e}"))
                            })
                            .or_else(|_| {
                                crate::win::input::wheel(cx, cy, 3, 0);
                                Ok(())
                            }),
                        "Scroll Right" => el
                            .get_pattern::<UIScrollPattern>()
                            .map_err(|e| format!("element does not support Scroll: {e}"))
                            .and_then(|p| {
                                p.scroll(ScrollAmount::SmallIncrement, ScrollAmount::NoAmount)
                                    .map_err(|e| format!("scroll failed: {e}"))
                            })
                            .or_else(|_| {
                                crate::win::input::wheel(cx, cy, -3, 0);
                                Ok(())
                            }),
                        other => Err(format!("unsupported secondary action: {other}")),
                    };
                    match outcome {
                        Ok(()) => Response::Result(json!({})),
                        Err(e) => Response::Error(e),
                    }
                }
                Err(resp) => resp,
            }
        }

        "get_window_state" => {
            let p: GwsParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let r = match resolve_win(&p.window).map_err(Response::Error) {
                Ok(r) => r,
                Err(resp) => return resp,
            };
            // Observed order: approval, then the both-false include check,
            // then the minimized refusal (an approved persistent home
            // answers both-false with the window echo even while iconic).
            if let Some(a) = approval_required(st, &r.owner) {
                return a;
            }
            if let Some(b) = browser_policy_gate(st, &r.owner) {
                return b;
            }
            if !p.include_screenshot && !p.include_text {
                return Response::Error(
                    "get_window_state must request include_text, include_screenshot, or both".into(),
                );
            }
            if unsafe { windows::Win32::UI::WindowsAndMessaging::IsIconic(r.hwnd).as_bool() } {
                return Response::Error(
                    "window is minimized; call activate_window, refresh with get_window, then retry get_window_state".into(),
                );
            }
            let hwnd = r.hwnd;
            let recs = enumerate::enumerate_top_level();
            let rec = recs.iter().find(|w| w.hwnd as u64 == p.window.id);
            let title = rec.map(|w| w.title.clone()).unwrap_or_default();
            let mut result = json!({
                "window": { "app": r.owner.clone(), "id": hwnd.0 as isize, "title": title },
                "screenshots": json!([]),
            });

            if p.include_screenshot {
                match crate::win::capture::capture_window_jpeg(hwnd) {
                    Ok((jpeg, w, h)) => {
                        st.last_capture_invalidation = None;
                        use base64::Engine as _;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
                        result["screenshots"] = json!([{
                            "id": "shot-1",
                            "url": format!("data:image/jpeg;base64,{b64}"),
                            "originX": 0,
                            "originY": 0,
                            "width": w,
                            "height": h,
                            "zIndex": 0,
                            "hasContent": true,
                        }]);
                    }
                    // Uncapturable (locked session, protected window):
                    // degrade to no screenshots like the original.
                    Err(e) => {
                        st.last_capture_invalidation = Some(e);
                    }
                }
            }

            if p.include_text {
                // Snapshot cache: replay the rendered tree while the window
                // is unchanged (the original keeps the revision stable).
                let key = hwnd.0 as isize;
                let hit = st.snapshots.get(&key).and_then(|s| {
                    if s.title == title
                        && s.origin == r.origin
                        && s.size == r.size
                        && !s.tree.is_empty()
                    {
                        Some((s.tree.clone(), s.focused_element.clone(), s.document_text.clone()))
                    } else {
                        None
                    }
                });
                let (tree, focused_element, document_text) = match hit {
                    Some(t) => t,
                    None => {
                        let app_name = header_app_name(&r.owner);
                        match crate::win::uia::build_tree(hwnd, &app_name) {
                            Ok(built) => {
                                let shot_ids: Vec<String> = result
                                    .get("screenshots")
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|s| {
                                                s.get("id").and_then(Value::as_str).map(String::from)
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let tup = (
                                    built.tree.clone(),
                                    built.focused_element.clone(),
                                    built.document_text.clone(),
                                );
                                st.snapshots.insert(
                                    key,
                                    crate::state::WindowSnapshot {
                                        hwnd: key,
                                        taken_ms: std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .map(|d| d.as_millis() as u64)
                                            .unwrap_or(0),
                                        origin: r.origin,
                                        size: r.size,
                                        elements: built.elements,
                                        shot_ids,
                                        tree: built.tree,
                                        focused_element: built.focused_element,
                                        document_text: built.document_text,
                                        title: title.clone(),
                                    },
                                );
                                // Original: +1 revision / +3 snapshot entries
                                // per freshly built tree.
                                st.snapshot_revision += 1;
                                st.snapshot_count += 3;
                                st.last_window_identity = Some(json!({
                                    "appId": r.owner,
                                    "bounds": {
                                        "originX": r.origin.0,
                                        "originY": r.origin.1,
                                        "width": r.size.0,
                                        "height": r.size.1,
                                    },
                                    "inputHwnd": (hwnd.0 as isize).to_string(),
                                    "processId": r.pid,
                                    "processName": process_file_name(&r.owner),
                                    "rootHwnd": (hwnd.0 as isize).to_string(),
                                    "snapshotRevision": st.snapshot_revision,
                                    "title": title,
                                }));
                                tup
                            }
                            Err(_) => (String::new(), None, None),
                        }
                    }
                };
                let mut acc = json!({});
                if !tree.is_empty() {
                    acc["tree"] = json!(tree);
                    if let Some(f) = focused_element {
                        acc["focused_element"] = json!(f);
                    }
                    if let Some(d) = document_text {
                        acc["document_text"] = json!(d);
                    }
                }
                result["accessibility"] = acc;
            }
            result["cacheDiagnostics"] = json!({
                "accessibilityRevision": st.snapshot_revision,
                "accessibilitySnapshotCount": st.snapshot_count,
                "captureCachedSessionCount": st
                    .snapshots
                    .values()
                    .filter(|s| !s.shot_ids.is_empty())
                    .count(),
            });
            Response::Result(result)
        }
        "launch_app" => {
            let p: LaunchParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let plan = match crate::launch::plan(&p.app) {
                Ok(plan) => plan,
                Err(e) => return Response::Error(e),
            };
            let owner = crate::launch::plan_owner(&plan);
            if let Some(a) = approval_required(st, &owner) {
                return a;
            }
            match crate::launch::execute(&plan) {
                Ok(()) => Response::Result(json!({})),
                Err(e) => Response::Error(e),
            }
        }
        "start_audio_recording" | "stop_audio_recording" => {
            // Double-gated by SKY_ENABLE_AUDIO="1" in the real helper.
            if std::env::var("SKY_ENABLE_AUDIO").ok().as_deref() != Some("1") {
                return Response::Error("computer audio recording is not enabled".into());
            }
            Response::Error(format!("not implemented: {method}"))
        }
        other => Response::Error(format!("unsupported method: {other}")),
    }
}
