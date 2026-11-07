pub struct RpcOptions {
    pub parent_pid: u32,
}

pub enum Invocation {
    Rpc(RpcOptions),
    CursorManager,
}

/// 走 RPC 这条路时 `--parent-pid <pid>` 必填（看门狗，父进程一死就退出）。
/// `--system-cursor-manager` 是原版把自己重启成光标管理器时用的模式。
/// 不认识的参数一律回 `unknown argument: <a>`（原版 exe 里就是这个字）。
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
                // Codex CLI 的 notify 衔接参数：`--previous-notify <json>`。
                // Node 宿主根本不用它，收下就丢。
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
