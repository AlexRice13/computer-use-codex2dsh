# computer-use-codex2dsh

> Give [DeepSeek Harness](https://github.com/deepseek-ai) (DSH) full Windows desktop control: 13 `sky__*` tools driven by the Sky computer-use helper (`codex-computer-use.exe`) from your own local OpenAI Codex/ChatGPT desktop installation.

`computer-use-codex2dsh` 是一个独立的 Windows「电脑操作」Cordis 插件：它把 Codex 桌面端附带的 Sky 原生助手（`codex-computer-use.exe`）接到 DeepSeek Harness 上，让模型可以直接查看窗口、点击、打字、滚动、拖拽。插件自身是一个纯 Node.js 的 stdio JSON-RPC 客户端/监管者——**运行时不连接 ChatGPT/Codex 的任何服务，不读取任何账号数据，不使用网络**。

## 它做什么

- 向 Harness 注册 13 个 `sky__*` 桌面工具（见下表），独占 `computerUse` 注册槽位。
- 首次调用时把 helper 与一个本地 `app-server` 桩进程暂存到 `%LOCALAPPDATA%\dsh-computer-use\bin`，以 `--parent-pid` 方式启动并监管；卸载插件时干净回收进程。
- 结果经 MCP 适配器投影：截图作为图像附件、UIA 树带元素索引；插件层补充了新鲜快照校验、遮挡启发式拒绝、自我排除等守卫。

## 工具一览

| 工具 | 说明 |
|------|------|
| `sky__list_apps` / `sky__list_windows` / `sky__get_window` | 枚举应用与窗口，解析窗口句柄 `{app, id}` |
| `sky__get_window_state` | 一次调用同时取 JPEG 截图与 UIA 可访问性树（元素索引仅对该快照有效） |
| `sky__activate_window` | 置前窗口（任何输入前的必要动作——输入发往前台窗口） |
| `sky__click` | 元素索引点击（首选）或快照坐标点击（必须携带 `screenshotId`） |
| `sky__drag` / `sky__scroll` | 快照坐标拖拽 / 滚动（必须携带 `screenshotId`） |
| `sky__type_text` / `sky__press_key` | 字面文本输入 / 组合键（`Enter`、`ctrl+s` 等） |
| `sky__set_value` / `sky__perform_secondary_action` | UIA 值模式写入 / 次级动作（Raise、Scroll Up/Down/Left/Right、Expand、Collapse） |
| `sky__launch_app` | 按应用 id 或可执行文件路径启动应用 |

## 前置要求

- **Windows 10/11**（helper 仅存在于 Windows 桌面端）。
- **DeepSeek Harness**（提供 `@deepseek-ai/cordis` `~4.0.5`、`dsh-tools` / `dsh-system-prompt` / `dsh-computer-use` `0.2.1-rc.5` 等 peer 依赖的宿主环境）。
- **本机已安装过一次 OpenAI Codex / ChatGPT 桌面版**——插件需要从
  `%LOCALAPPDATA%\OpenAI\Codex\runtimes\cua_node\*\bin\node_modules\@oai\sky\bin\windows\codex-computer-use.exe`
  复制原生助手文件；或通过配置 `helperPath` 手动指向你自留的一份 `codex-computer-use.exe`。
  **本仓库不包含、也不下载该二进制**，请遵守上游许可与服务条款。
- **Node.js**（`node.exe`，用于运行本地 `app-server` 桩；可用 `nodePath` 指定）。

## 安装

把本仓库放进 DSH profile 可解析的位置后，用 Harness 的 plugin manager 安装，或直接把 `cordis.patch.yml` 的两条 `insert` 合并进你的 profile composition，并让 `name:` 指向本包（`computer-use-codex2dsh`）。插件平台标记为 `win32`。

## 配置

| 选项 | 默认 | 说明 |
|------|------|------|
| `helperPath` | 自动发现 | `codex-computer-use.exe` 的绝对路径 |
| `nodePath` | 自动发现 | 用于运行 app-server 桩的 `node.exe` |
| `stageDir` | `%LOCALAPPDATA%\dsh-computer-use\bin` | helper/桩的暂存目录（须为 Medium 完整性级别） |
| `codexHome` | `%LOCALAPPDATA%\dsh-computer-use\codex-home` | helper 的私有 CODEX_HOME（中断标记、缓存） |
| `autoApprove` | `[]` | 免审批应用。大小写不敏感，接受 helper 报告的任意身份：app id、`process:<path>`、可执行文件名（`notepad` / `notepad.exe`）、AUMID 包前缀（`Microsoft.WindowsNotepad`）、含本地化的显示名（`记事本`）。Harness 自身永远不会被批准 |
| `enforceBrowserUrlPolicy` | `false` | 为 `true` 时不再发送绕过 helper 浏览器 URL 策略的本地测试 meta |
| `timeoutMs` / `launchTimeoutMs` | `10000` / `15000` | 单请求 / 启动超时 |

## 安全模型

- **逐应用审批（fail-closed）**：helper 首次触达某个应用时发出 `approvalRequest`，经 Harness 审批服务征询用户；没有审批服务或用户拒绝时一律拒绝。`allowed-once` 在同一 agent 会话内记住。
- **不可覆盖的自我排除**：任何输入类工具（点击/输入/按键/激活等）与审批层三重拒绝把 Harness 自己当作目标，`autoApprove` 也无法解锁。
- **物理 Esc**：helper 以 130 退出并写入中断标记后，本回合内所有调用短路为「用户已停止电脑操作」；回合推进或标记被清理即自动恢复。
- **坐标守卫**：坐标输入必须携带该窗口最新快照的 `screenshotId`；快照期被窗口级覆盖物压住的点直接拒绝（目标应用自己的小菜单不误伤）。任何输入送达后旧快照坐标即失效。
- 桌面操作对模型可见的护栏只是护栏：请在人工可监督的前提下使用。

## 已知限制（实测）

- 部分现代应用（UWP 记事本、Chrome 等）不向 helper 暴露 UIA 树 → 用截图 + 坐标/键盘驱动。
- `explorer.exe` 等共享系统进程的窗口报 `executable has multiple signatures`，无法寻址。
- 经 UWP stub 启动应用（如系统 `notepad.exe`）可能返回「did not expose a targetable window」——应用其实已启动，去 `sky__list_windows` 找它的包身份窗口。
- 遮挡检测是尽力而为：只有当快照同时捕获到覆盖窗口时才能判定；输入前务必先 `sky__activate_window`。

## 免责声明

非官方爱好者项目，与 OpenAI、DeepSeek 无关。`codex-computer-use.exe` 及其行为归上游所有；本仓库仅包含连接它的开源桥接代码。
