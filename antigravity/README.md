# Computer Use (`codex-computer-use` Backend)

Windows 桌面操作插件，通过 stdio MCP 服务（`codex-computer-use`）将 15 个 `sky__*` 桌面控制工具接入 Antigravity，后端使用原生 `codex-computer-use.exe`。

## 组件与自动退出保障

- **MCP 服务** (`codex-computer-use`)：由 `scripts/server.mjs` 启动，按需拉起并暂存 `codex-computer-use.exe` 到 `%LOCALAPPDATA%\agy-computer-use`。
- **任务完成自动退出**：
  1. **主动退出工具 (`sky__close`)**：智能体完成桌面操作后立即调用，发送 `end_turn` 与 `close` 收起代理光标和顶部状态条，同时保留当前会话的应用授权记忆。
  2. **回合结束钩子 (`hooks.json` -> `Stop`)**：每次对话回合结束时，无论正常结束、报错还是用户取消，`scripts/stop-hook.mjs` 都会通过本地命名管道自动关闭仍在运行的 `codex-computer-use.exe`。
  3. **空闲自动回收 (`idleCloseMs`)**：默认连续 15 秒无新调用也会自动收起助手进程。
- **Skill** (`computer-use`)：提供窗口发现、UIA 控件树快照、截图坐标操作及收尾退出规范。

## 配置 (`config.json`)

位于插件根目录 `~/.gemini/config/plugins/computer-use/config.json`（也可在 `${PLUGIN_DATA}/config.json` 中覆盖）：

| 字段 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `helperPath` | 自动发现 | 显式指定 `codex-computer-use.exe` 路径；省略时自动从 `%LOCALAPPDATA%\OpenAI\Codex\runtimes\cua_node` 或内置备份加载 |
| `autoApprove` | `[]` | 免弹窗直接放行的应用列表（支持进程名、AUMID 前缀或显示名，如 `"chrome.exe"`、`"notepad"`、`"画图"`） |
| `denyApps` | `["Antigravity.exe", "Antigravity", "WindowsTerminal.exe", "Microsoft.WindowsTerminal"]` | 始终禁止控制的应用 |
| `approvalMode` | `"dialog"` | 审批方式：`"dialog"`（原生置顶确认框）、`"elicit-only"` 或 `"deny"` |
| `idleCloseMs` | `15000` | 无操作自动退出助手进程的空闲毫秒数（`0` 表示仅靠 `sky__close` 和 `Stop` 钩子退出） |
| `locale` | `"zh"` | 状态提示条与审批弹窗语言（`"zh"` 或 `"en"`） |
| `timeoutMs` | `10000` | 单次操作超时时间（毫秒） |
| `launchTimeoutMs` | `15000` | 启动应用超时时间（毫秒） |
| `turnIdleMs` | `30000` | 两次调用间隔超过该毫秒数视为新回合 |
