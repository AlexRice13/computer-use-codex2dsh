use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::methods;
use crate::state::{EngineState, Response};

/// Known RPC methods (exe blob at @0x1253170, byte-exact):
/// anything else answers `unsupported method: <name>`.
const KNOWN_METHODS: &[&str] = &[
    "start_audio_recording",
    "stop_audio_recording",
    "get_window_state",
    "list_apps",
    "launch_app",
    "list_windows",
    "get_window",
    "diagnostic_state",
    "activate_window",
    "perform_secondary_action",
    "set_value",
    "click_element",
    "scroll_element",
    "click",
    "drag",
    "scroll",
    "type_text",
    "press_key",
    "close",
    "end_turn",
];

fn turn_key_from(meta: &Value) -> Option<String> {
    let get = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            meta.get(*k)
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
    };
    let session = get(&["thread-id", "session_id", "sessionId", "conversation_id", "conversationId"])?;
    let turn = get(&["turn-id", "turn_id", "turnId"])?;
    Some(format!("{session}\u{0}{turn}"))
}

pub fn serve(policy: crate::policy::Policy) {
    // The dispatch thread serves all handlers inline (COM identity, capture,
    // input); initialize COM once for its whole lifetime.
    let _com = crate::win::com::ComGuard::new();
    let state = Arc::new(Mutex::new(EngineState::new(policy)));
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout().lock();

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break, // stdin closed
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(trimmed) else {
            continue; // unreadable input frame: ignore (client never sends these)
        };
        // Only numeric ids get responses (spec §A.4); notifications ignored.
        let Some(id) = frame.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            let _ = writeln!(stdout, "{}", error_line(id, "method is required"));
            let _ = stdout.flush();
            continue;
        };
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        let meta = frame.get("meta").cloned().unwrap_or_else(|| json!({}));
        let budget_ms = meta
            .get("x-oai-cua-request-budget-ms")
            .and_then(Value::as_u64)
            .unwrap_or(10_000);

        // Turn transitions: a new turn key clears session approvals lazily;
        // end_turn is answered below (the client drives previous-turn end_turn
        // itself, per spec §E.3).
        if let Some(key) = turn_key_from(&meta) {
            let mut st = state.lock().unwrap();
            if st.last_turn_key.as_deref() != Some(key.as_str()) {
                st.last_turn_key = Some(key.clone());
                st.approved_apps.clear();
                crate::escape::set_turn(&key);
            }
        }

        if !KNOWN_METHODS.contains(&method) {
            let _ = writeln!(
                stdout,
                "{}",
                error_line(id, &format!("unsupported method: {method}"))
            );
            let _ = stdout.flush();
            continue;
        }

        // Per-request budget (x-oai-cua-request-budget-ms). Handlers bound
        // their own work (UIA tree walk, capture polling) below the budget,
        // so we run serially on this thread and answer the budget error when
        // elapsed time exceeded it — identical wire text to the threaded
        // original, just delivered after the handler's internal cap.
        let started = std::time::Instant::now();
        let resp = {
            let mut st = state.lock().unwrap();
            methods::dispatch(&mut st, &method, &params, &meta)
        };
        let elapsed = started.elapsed().as_millis() as u64;
        let line = if elapsed > budget_ms.max(1) {
            error_line(id, "computer-use request budget exhausted")
        } else {
            render(id, resp)
        };
        let is_close = method == "close";
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
        if is_close {
            break;
        }
    }
}

fn error_line(id: i64, msg: &str) -> String {
    json!({ "id": id, "ok": false, "error": msg }).to_string()
}

fn render(id: i64, resp: Response) -> String {
    match resp {
        Response::Result(v) => json!({ "id": id, "ok": true, "result": v }).to_string(),
        Response::Error(msg) => error_line(id, &msg),
        Response::Approval {
            app,
            display_name,
            risk_level,
            allow_persistent_approval,
        } => {
            let mut ar = json!({
                "app": app,
                "displayName": display_name,
                "allowPersistentApproval": allow_persistent_approval,
            });
            if let Some(r) = risk_level.filter(|r| r == "high" || r == "low") {
                ar["riskLevel"] = json!(r);
            }
            json!({ "id": id, "ok": false, "approvalRequest": ar }).to_string()
        }
    }
}
