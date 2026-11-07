use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::methods;
use crate::state::{EngineState, Response};

/// 认得的 RPC 方法（原版 exe 里在 @0x1253170，逐字节对过）：
/// 不在表里的回 `unsupported method: <name>`。
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
    // 分发线程上所有 handler 都是内联跑的（COM 身份、截图、输入），
    // 所以这里一次初始化 COM，管到进程结束。
    let _com = crate::win::com::ComGuard::new();
    let state = Arc::new(Mutex::new(EngineState::new(policy)));
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout().lock();

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break, // stdin 关了
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(trimmed) else {
            continue; // 解析不了的帧就跳过（客户端不会发这种）
        };
        // 只有数字 id 才回包（规范 §A.4）；通知一律不理。
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

        // 换 turn：见到新的 turn key 就把会话里的批准清掉，顺手清、不提前清；
        // end_turn 在下面正常回包（上一轮的 end_turn 是客户端自己发的，
        // 见规范 §E.3）。
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

        // 每个请求的时间预算（x-oai-cua-request-budget-ms）。handler 自己会
        // 在预算内收手（UIA 树遍历、截图轮询都有上限），所以这里就串行跑，
        // 超了才回预算错误——错误文案跟原版多线程版一模一样，只是要等
        // handler 自己的上限到了才发得出去。
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
