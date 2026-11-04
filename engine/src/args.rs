pub struct RpcOptions {
    pub parent_pid: u32,
}

pub enum Invocation {
    Rpc(RpcOptions),
    CursorManager,
}

/// argv: `--parent-pid <pid>` is required on the RPC path (die-with-parent
/// watchdog). `--system-cursor-manager` is the cursor-manager re-exec mode.
/// Unknown argv → `unknown argument: <a>` (exe literal).
pub fn parse_args() -> Result<Invocation, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut parent_pid: Option<u32> = None;
    let mut cursor_manager = false;

    let mut i = 0usize;
    while i < argv.len() {
        let a = argv[i].as_str();
        i += 1;
        match a {
            "--parent-pid" => {
                if i >= argv.len() {
                    return Err("missing parent pid".into());
                }
                let v = argv[i].as_str();
                i += 1;
                parent_pid = Some(v.trim().parse::<u32>().map_err(|_| "parse parent pid".to_string())?);
            }
            "--system-cursor-manager" => cursor_manager = true,
            "--previous-notify" => {
                // Codex CLI notify integration: `--previous-notify <json>`.
                // A Node host never uses it; accept and drop.
                if i >= argv.len() {
                    return Err("missing value for --previous-notify".into());
                }
                i += 1;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if cursor_manager {
        return Ok(Invocation::CursorManager);
    }
    let parent_pid = parent_pid.ok_or_else(|| "missing parent pid".to_string())?;
    Ok(Invocation::Rpc(RpcOptions { parent_pid }))
}
