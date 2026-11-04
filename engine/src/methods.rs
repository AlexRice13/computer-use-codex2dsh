use serde_json::{json, Value};

use crate::catalog;
use crate::state::{EngineState, Response};
use crate::win::enumerate;

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

    match method {
        "close" => Response::Result(json!({})),
        "end_turn" => {
            st.last_turn_key = None;
            st.approved_apps.clear();
            Response::Result(json!({}))
        }
        "diagnostic_state" => Response::Result(json!({
            "engine": "sky-engine",
            "version": env!("CARGO_PKG_VERSION"),
            "snapshotRevision": st.snapshot_revision,
            "accessibilityRevision": st.snapshot_revision,
            "accessibilitySnapshotCount": st
                .snapshots
                .values()
                .map(|s| s.elements.len())
                .sum::<usize>(),
            "captureCachedSessionCount": st
                .snapshots
                .values()
                .filter(|s| !s.shot_ids.is_empty())
                .count(),
            "lastCaptureInvalidationReason": st.last_capture_invalidation,
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
            let want_id = params.get("id").and_then(|v| {
                v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
            });
            let want_app = params.get("app").and_then(Value::as_str);
            let recs = enumerate::enumerate_top_level();
            let hit = recs.into_iter().find(|r| {
                want_id == Some(r.hwnd as i64)
                    && want_app.map(|a| a.eq_ignore_ascii_case(&r.app)).unwrap_or(true)
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
        "activate_window" => match resolve_window(params) {
            Ok((hwnd, _)) => {
                crate::win::input::activate(hwnd);
                Response::Result(json!({}))
            }
            Err(e) => Response::Error(e),
        },
        "type_text" => match resolve_window(params) {
            Ok((_hwnd, _)) => {
                let text = params.get("text").and_then(Value::as_str).unwrap_or("");
                crate::win::input::type_text(text);
                Response::Result(json!({}))
            }
            Err(e) => Response::Error(e),
        },
        "press_key" => match resolve_window(params) {
            Ok((_hwnd, _)) => {
                let key = params.get("key").and_then(Value::as_str).unwrap_or("");
                match crate::win::input::press_key(key) {
                    Ok(()) => Response::Result(json!({})),
                    Err(e) => Response::Error(e),
                }
            }
            Err(e) => Response::Error(e),
        },
        "click" => match resolve_window(params) {
            Ok((_hwnd, origin)) => {
                let x = snapshot_x(params, origin.0);
                let y = snapshot_y(params, origin.1);
                let button = match params.get("mouse_button").and_then(Value::as_str) {
                    Some("right") => crate::win::input::Button::Right,
                    Some("middle") => crate::win::input::Button::Middle,
                    _ => crate::win::input::Button::Left,
                };
                let count = params.get("click_count").and_then(Value::as_u64).unwrap_or(1) as u32;
                crate::win::input::click(x, y, button, count);
                Response::Result(json!({}))
            }
            Err(e) => Response::Error(e),
        },
        "scroll" => match resolve_window(params) {
            Ok((_hwnd, origin)) => {
                let x = snapshot_x(params, origin.0);
                let y = snapshot_y(params, origin.1);
                let sx = params.get("scrollX").and_then(Value::as_i64).unwrap_or(0) as i32;
                let sy = params.get("scrollY").and_then(Value::as_i64).unwrap_or(0) as i32;
                crate::win::input::wheel(x, y, sx, sy);
                Response::Result(json!({}))
            }
            Err(e) => Response::Error(e),
        },
        "drag" => match resolve_window(params) {
            Ok((_hwnd, origin)) => {
                let fx = snapshot_num(params, "from_x", origin.0);
                let fy = snapshot_num(params, "from_y", origin.1);
                let tx = snapshot_num(params, "to_x", origin.0);
                let ty = snapshot_num(params, "to_y", origin.1);
                crate::win::input::drag((fx, fy), (tx, ty));
                Response::Result(json!({}))
            }
            Err(e) => Response::Error(e),
        },
        "click_element" => match resolve_element(st, params) {
            Ok((hwnd, _el, rect)) => {
                let cx = rect.0 + rect.2 / 2;
                let cy = rect.1 + rect.3 / 2;
                crate::win::input::activate(hwnd);
                let button = match params.get("mouse_button").and_then(Value::as_str) {
                    Some("right") => crate::win::input::Button::Right,
                    Some("middle") => crate::win::input::Button::Middle,
                    _ => crate::win::input::Button::Left,
                };
                let count = params.get("click_count").and_then(Value::as_u64).unwrap_or(1) as u32;
                crate::win::input::click(cx, cy, button, count);
                Response::Result(json!({}))
            }
            Err(e) => Response::Error(e),
        },
        "scroll_element" => match resolve_element(st, params) {
            Ok((_hwnd, _el, rect)) => {
                let cx = rect.0 + rect.2 / 2;
                let cy = rect.1 + rect.3 / 2;
                let sx = params.get("scrollX").and_then(Value::as_i64).unwrap_or(0) as i32;
                let sy = params.get("scrollY").and_then(Value::as_i64).unwrap_or(0) as i32;
                crate::win::input::wheel(cx, cy, sx, sy);
                Response::Result(json!({}))
            }
            Err(e) => Response::Error(e),
        },
        "set_value" => match resolve_element(st, params) {
            Ok((_hwnd, el, _rect)) => {
                let want = params.get("value").and_then(Value::as_str).unwrap_or("");
                match el.get_cached_pattern::<uiautomation::patterns::UIValuePattern>() {
                    Ok(pattern) => match pattern.set_value(want) {
                        Ok(()) => Response::Result(json!({})),
                        Err(e) => Response::Error(format!("set value failed: {e}")),
                    },
                    Err(e) => Response::Error(format!("element does not support ValuePattern: {e}")),
                }
            }
            Err(e) => Response::Error(e),
        },
        "perform_secondary_action" => match resolve_element(st, params) {
            Ok((hwnd, el, rect)) => {
                let action = params.get("action").and_then(Value::as_str).unwrap_or("");
                let cx = rect.0 + rect.2 / 2;
                let cy = rect.1 + rect.3 / 2;
                use uiautomation::patterns::{
                    UIExpandCollapsePattern, UIScrollPattern,
                };
                use uiautomation::types::ScrollAmount;
                let outcome: Result<(), String> = match action {
                    "Expand" => el
                        .get_cached_pattern::<UIExpandCollapsePattern>()
                        .map_err(|e| format!("element does not support ExpandCollapse: {e}"))
                        .and_then(|p| p.expand().map_err(|e| format!("expand failed: {e}"))),
                    "Collapse" => el
                        .get_cached_pattern::<UIExpandCollapsePattern>()
                        .map_err(|e| format!("element does not support ExpandCollapse: {e}"))
                        .and_then(|p| p.collapse().map_err(|e| format!("collapse failed: {e}"))),
                    "Raise" => {
                        crate::win::input::activate(hwnd);
                        Ok(())
                    }
                    "Scroll Up" => el
                        .get_cached_pattern::<UIScrollPattern>()
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
                        .get_cached_pattern::<UIScrollPattern>()
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
                        .get_cached_pattern::<UIScrollPattern>()
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
                        .get_cached_pattern::<UIScrollPattern>()
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
            Err(e) => Response::Error(e),
        },
        "get_window_state" => {
            let include_shot = params
                .get("include_screenshot")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let include_text = params
                .get("include_text")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            match resolve_window(params) {
                Ok((hwnd, origin)) => {
                    // Echo the window descriptor the caller addressed.
                    let recs = enumerate::enumerate_top_level();
                    let rec = recs.iter().find(|r| r.hwnd as isize == hwnd.0 as isize);
                    let window = json!({
                        "app": params.get("window").and_then(|w| w.get("app")).and_then(Value::as_str)
                            .map(Value::from).unwrap_or_else(|| json!(rec.map(|r| r.app.clone()).unwrap_or_default())),
                        "id": hwnd.0 as isize,
                        "title": rec.map(|r| r.title.clone()).unwrap_or_default(),
                    });
                    let mut result = json!({ "window": window });

                    if include_shot {
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
                                result["screenshots"] = json!([]);
                            }
                        }
                    }

                    if include_text {
                        match crate::win::uia::build_tree(hwnd) {
                            Ok(built) => {
                                let mut acc = json!({});
                                if !built.tree.is_empty() {
                                    acc["tree"] = json!(built.tree);
                                }
                                if let Some(f) = built.focused {
                                    acc["focused"] = f;
                                }
                                let shot_ids: Vec<String> = result
                                    .get("screenshots")
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|s| s.get("id").and_then(Value::as_str).map(String::from))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                st.snapshots.insert(
                                    hwnd.0 as isize,
                                    crate::state::WindowSnapshot {
                                        hwnd: hwnd.0 as isize,
                                        taken_ms: std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .map(|d| d.as_millis() as u64)
                                            .unwrap_or(0),
                                        origin,
                                        elements: built.elements,
                                        shot_ids,
                                    },
                                );
                                st.snapshot_revision += 1;
                                result["accessibility"] = acc;
                            }
                            Err(_) => result["accessibility"] = json!({}),
                        }
                    }
                    Response::Result(result)
                }
                Err(e) => Response::Error(e),
            }
        }
        "launch_app" => {
            let app = params.get("app").and_then(Value::as_str).unwrap_or("");
            match crate::launch::launch(app) {
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

/// Resolve params.window ({app?, id}) to an HWND plus the window's screen
/// origin (snapshot-relative coordinates are expressed from that origin).
fn resolve_window(params: &Value) -> Result<(windows::Win32::Foundation::HWND, (i32, i32)), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;
    let win = params
        .get("window")
        .ok_or_else(|| "missing window".to_string())?;
    let id = win
        .get("id")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok())))
        .ok_or_else(|| "missing window id".to_string())?;
    let hwnd = HWND(id as *mut core::ffi::c_void);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err("window is not a usable app window".into());
        }
    }
    if let Some(app) = win.get("app").and_then(Value::as_str) {
        // The app hint must still name the same process address.
        let recs = enumerate::enumerate_top_level();
        if !recs.iter().any(|r| r.hwnd as i64 == id && r.app.eq_ignore_ascii_case(app)) {
            // Windows move between enumeration passes; only a changed app
            // identity is fatal, a vanished window still accepts input if
            // IsWindow passed above.
            let still_alive = recs.iter().any(|r| r.hwnd as i64 == id);
            if !still_alive {
                let exact_app = recs.iter().any(|r| r.app.eq_ignore_ascii_case(app));
                if !exact_app {
                    return Err("window is not a usable app window".into());
                }
            }
        }
    }
    let mut rect = windows::Win32::Foundation::RECT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);
    }
    Ok((hwnd, (rect.left, rect.top)))
}

fn snapshot_num(params: &Value, key: &str, origin: i32) -> i32 {    params
        .get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)))
        .unwrap_or(0) as i32
        + origin
}

fn snapshot_x(params: &Value, origin: i32) -> i32 {
    snapshot_num(params, "x", origin)
}

/// Resolve params.window + element_index against the latest snapshot of
/// that window. Returns the HWND, a cloned UIElement, and its screen rect.
fn resolve_element(
    st: &EngineState,
    params: &Value,
) -> Result<(windows::Win32::Foundation::HWND, uiautomation::UIElement, (i32, i32, i32, i32)), String> {
    let (hwnd, _) = resolve_window(params)?;
    let index = params
        .get("element_index")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok())))
        .ok_or_else(|| "missing element_index".to_string())? as usize;
    let snap = st
        .snapshots
        .get(&(hwnd.0 as isize))
        .ok_or_else(|| "no snapshot for window; call get_window_state first".to_string())?;
    let rec = snap
        .elements
        .get(index)
        .ok_or_else(|| format!("element_index {index} is out of range for the latest snapshot ({} elements)", snap.elements.len()))?;
    Ok((hwnd, rec.element.clone(), rec.rect))
}

fn snapshot_y(params: &Value, origin: i32) -> i32 {
    snapshot_num(params, "y", origin)
}
