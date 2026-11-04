use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};

use crate::policy::{policy_from_config_read, Policy};

/// The helper is a client of `codex app-server`: it spawns
/// `<CODEX_CLI_PATH> app-server` and blocks on three calls before serving any
/// computer-use RPC (measured live; the dsh host supplies a stub that answers
/// these synchronously). Request ids on this leg are strings, not numbers.
pub fn run_app_server_handshake() -> Result<Policy, String> {
    let cli = std::env::var("CODEX_CLI_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "failed to launch codex app-server".to_string())?;

    let mut child: Child = Command::new(&cli)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "failed to launch codex app-server".to_string())?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open codex app-server stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open codex app-server stdout".to_string())?;
    let mut stdin = stdin;
    let mut reader = BufReader::new(stdout);

    let call = |id: &str, method: &str, params: Value, reader: &mut BufReader<std::process::ChildStdout>, stdin: &mut std::process::ChildStdin| -> Result<Value, String> {
        let frame = json!({ "id": id, "method": method, "params": params });
        writeln!(stdin, "{frame}").map_err(|_| "failed to write app-server request".to_string())?;
        stdin.flush().map_err(|_| "failed to write app-server request".to_string())?;
        loop {
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .map_err(|_| "failed to read app-server response".to_string())?;
            if n == 0 {
                return Err("codex app-server exited before returning response".to_string());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
                continue; // skip non-JSON lines like a banner
            };
            if obj.get("id").and_then(Value::as_str) == Some(id) {
                if let Some(err) = obj.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("codex app-server rejected ");
                    return Err(format!("codex app-server rejected {msg}"));
                }
                return Ok(obj.get("result").cloned().unwrap_or(Value::Null));
            }
            // notifications / other ids: ignore
        }
    };

    let init = call(
        "1",
        "initialize",
        json!({
            "clientInfo": {
                "name": "codex-computer-use",
                "title": "Windows Computer Use Helper",
                "version": "0.1.0"
            },
            "capabilities": { "experimentalApi": true },
            "optOutNotificationMethods": []
        }),
        &mut reader,
        &mut stdin,
    )?;
    if init.get("userAgent").and_then(Value::as_str).is_none() {
        return Err("failed waiting for codex app-server".to_string());
    }

    let reqs = call("2", "configRequirements/read", json!({}), &mut reader, &mut stdin)?;
    if reqs.is_null() || reqs.get("requirements").is_none() {
        // `requirements: null` is a legal answer; a whole missing field is not.
        if reqs.get("requirements").is_none() {
            return Err("configRequirements/read did not return requirements".to_string());
        }
    }

    let cfg = call("3", "config/read", json!({}), &mut reader, &mut stdin)?;
    let config = cfg
        .get("config")
        .cloned()
        .ok_or_else(|| "config/read did not return config".to_string())?;
    let policy = policy_from_config_read(&config)?;

    // Handshake done; the stub child is disposable from here on.
    drop(stdin);
    let _ = child.wait();

    if !policy.allow_browser_and_computer_use {
        return Err("Computer Use is disabled by policy".to_string());
    }
    Ok(policy)
}
