# computer-use-codex2dsh

> Give [DeepSeek Harness](https://github.com/deepseek-ai) (DSH) full Windows desktop control: 14 `sky__*` tools backed by an **open-source native reimplementation** of the Sky computer-use helper — no ChatGPT/Codex installation required at runtime.

`computer-use-codex2dsh` 是一个独立的 Windows「电脑操作」Cordis 插件：它把开源原生引擎 `sky-engine`（`engine/`，Rust，逆向复刻 Sky `codex-computer-use.exe` 的 stdio JSON-RPC 契约）接到 DeepSeek Harness 上，让模型可以直接查看窗口、点击、打字、滚动、拖拽。插件自身是一个纯 Node.js 的 stdio JSON-RPC 客户端/监管者——**运行时不连接 ChatGPT/Codex 的任何服务，不读取任何账号数据，不使用网络**。

## 它做什么

- 向 Harness 注册 14 个 `sky__*` 桌面工具（见下表），独占 `computerUse` 注册槽位。
- 首次调用时把 `sky-engine` 与一个本地 `app-server` 桩进程暂存到 `%LOCALAPPDATA%\dsh-computer-use\bin`，以 `--parent-pid` 方式启动并监管；卸载插件时干净回收进程。
- 结果经 MCP 适配器投影：截图作为图像附件、UIA 树带元素索引和窗口相对包围盒；插件层补充了新鲜快照校验、遮挡启发式拒绝、自我排除等守卫。
- 截图支持 `scale` 缩放与 `region` 局部裁剪（省 token），在缩小/裁剪后的图上量出的坐标仍可直接用于点击、拖拽、滚动；同一快照内的一串动作可以用 `sky__batch` 一次发完。

## 引擎：`engine/`（sky-rs）

`engine/` 是 `codex-computer-use.exe` 的行为兼容开源重实现（Rust，`windows` + `uiautomation` crate）：

- 完整的 20 方法 RPC 面（`list_apps` / `list_windows` / `get_window` / `get_window_state` / `activate_window` / `click` / `click_element` / `scroll_element` / `drag` / `scroll` / `type_text` / `press_key` / `set_value` / `perform_secondary_action` / `launch_app` / `end_turn` / `close` / `diagnostic_state` / `start|stop_audio_recording`），逐条对齐 `SKY_WINDOWS_PROTOCOL_SPEC.md` 的错误文案、审批/回合/预算 meta、130/Esc 中断标记等语义；
- 窗口/应用枚举（含 taskbar AUMID 探测、StartApps/UserAssist 使用度）、WGC 窗口截图（WIC JPEG 编码）、UIA 元素树与 pattern 动作、SendInput 输入、app-server 握手与策略查询、受限环境变量强化、快照缓存与 `diagnostic_state`。

构建（Windows，[Rust toolchain](https://rustup.rs/)）：

```sh
cd engine && cargo build --release
# 产物：engine/target/release/sky-engine.exe
```

然后把它接到插件（配置任选其一）：

```yaml
# cordis.patch.yml 的 pluginConfig
helperPath: "C:/path/to/engine/target/release/sky-engine.exe"
```

不设 `helperPath` 时插件仍会自动发现本机已安装的 Codex/ChatGPT 桌面版自带的原版助手——两条路线都支持。

## 工具一览

| 工具 | 说明 |
|------|------|
| `sky__list_apps` / `sky__list_windows` / `sky__get_window` | 枚举应用与窗口，解析窗口句柄 `{app, id}` |
| `sky__get_window_state` | 一次调用同时取 JPEG 截图与 UIA 可访问性树（元素索引仅对该快照有效）。可选 `scale`（0.1–1，缩放截图）与 `region {x,y,width,height}`（逻辑窗口坐标裁剪）；树行尾附窗口相对包围盒 `@x,y WxH` |
| `sky__activate_window` | 置前窗口（任何输入前的必要动作——输入发往前台窗口） |
| `sky__click` | 元素索引点击（首选）或快照坐标点击（必须携带 `screenshotId`） |
| `sky__drag` / `sky__scroll` | 快照坐标拖拽 / 滚动（必须携带 `screenshotId`）；拖拽按距离插值并在落点停留后显式抬起，长距离手势可正常提交 |
| `sky__batch` | 一次调用顺序执行最多 50 个动作（点击/拖拽/滚动/按键/文本/等待/激活）；整批只校验一次 `screenshotId`，坐标自动带上同一快照的裁剪与缩放参数 |
| `sky__type_text` / `sky__press_key` | 字面文本输入 / 组合键（`Enter`、`ctrl+s` 等） |
| `sky__set_value` / `sky__perform_secondary_action` | UIA 值模式写入 / 次级动作（Raise、Scroll Up/Down/Left/Right、Expand、Collapse） |
| `sky__launch_app` | 按应用 id、AUMID、可执行文件路径或应用名启动应用 |

## 前置要求

- **Windows 10/11**（桌面自动化仅存在于 Windows）。
- **DeepSeek Harness `0.2.2-rc.1`**（peer 依赖 `@deepseek-ai/dsh-system-prompt` / `dsh-tools` 为 `^0.2.2-rc.1`，cordis 为 `~4.1.0`；0.2.1 及更早版本会被 bundle 版本校验拒绝）。
- **Node.js**（`node.exe`，用于运行本地 `app-server` 桩；可用 `nodePath` 指定）。
- 使用内置开源引擎时**无需**安装 OpenAI Codex / ChatGPT 桌面版。

## 安装

`plugin_manager` 对本地目录使用 `link:` 协议，**不会**安装包的 `dependencies`，所以先把运行时依赖装进包内：

```sh
npm install --omit=dev
```

然后把本仓库放进 DSH profile 可解析的位置，用 Harness 的 plugin manager 安装，或直接把 `cordis.patch.yml` 的两条 `insert` 合并进你的 profile composition，并让 `name:` 指向本包（`computer-use-codex2dsh`）。插件平台标记为 `win32`。

## 配置

| 选项 | 默认 | 说明 |
|------|------|------|
| `helperPath` | 自动发现 | 助手二进制绝对路径——推荐指向 `engine/target/release/sky-engine.exe`；留空则自动发现本机 Codex/ChatGPT 桌面版原版助手 |
| `nodePath` | 自动发现 | 用于运行 app-server 桩的 `node.exe` |
| `stageDir` | `%LOCALAPPDATA%\dsh-computer-use\bin` | 助手/桩的暂存目录（须为 Medium 完整性级别） |
| `codexHome` | `%LOCALAPPDATA%\dsh-computer-use\codex-home` | 助手的私有 CODEX_HOME（中断标记、缓存） |
| `autoApprove` | `[]` | 免审批应用。大小写不敏感，接受助手报告的任意身份：app id、`process:<path>`、可执行文件名（`notepad` / `notepad.exe`）、AUMID 包前缀（`Microsoft.WindowsNotepad`）、含本地化的显示名（`记事本`）。Harness 自身永远不会被批准 |
| `enforceBrowserUrlPolicy` | `false` | 为 `true` 时不再发送绕过助手浏览器 URL 策略的本地测试 meta |
| `timeoutMs` / `launchTimeoutMs` | `10000` / `15000` | 单请求 / 启动超时 |

## 安全模型

- **逐应用审批（fail-closed）**：助手首次触达某个应用时发出 `approvalRequest`，经 Harness 审批服务征询用户；没有审批服务或用户拒绝时一律拒绝。`allowed-once` 在同一 agent 会话内记住。
- **不可覆盖的自我排除**：任何输入类工具（点击/输入/按键/激活等）与审批层三重拒绝把 Harness 自己当作目标，`autoApprove` 也无法解锁。
- **物理 Esc**：只认**真实**按键——注入的 `SendInput`（包括代理自己的 `press_key Escape`）带 `LLKHF_INJECTED`/`SKY_ESC` 标记，不会触发中断。助手以 130 退出并写入中断标记后，本回合内所有调用短路为「用户已停止电脑操作」；回合推进或标记被清理即自动恢复。
- **坐标守卫**：坐标输入必须携带该窗口最新快照的 `screenshotId`；快照期被窗口级覆盖物压住的点直接拒绝（目标应用自己的小菜单不误伤）。任何输入送达后旧快照坐标即失效。
- 桌面操作对模型可见的护栏只是护栏：请在人工可监督的前提下使用。

## 已知限制（实测）

- 部分现代应用不向助手暴露完整 UIA 树（视窗口状态而定）→ 用截图 + 坐标/键盘驱动。
- UIA 树 / 截图依赖跨进程连接：若助手进程被运行环境限制（受限令牌、命名管道封禁），跨进程 UIA 只会得到非客户区骨架、WGC 拿不到帧——插件经宿主正常派生时不受影响（实测：原生对话框 27 行完整树 + `hasContent` 截图）。引擎按原版行为做 structure-changed 事件订阅唤醒（`wake_provider`）以提升挂起进程的树覆盖。
- 已知线级偏差：`approvalRequest` 的 JSON 字段顺序与原版不同（字母序 vs 结构定义序），JSON 语义等价，对所有解析方透明。
- `explorer.exe` 等共享系统进程的窗口报 `executable has multiple signatures`，无法寻址。
- 锁屏会话期间截图返回权限错误并降级为无截图（与原版行为一致）。
- 遮挡检测是尽力而为：只有当快照同时捕获到覆盖窗口时才能判定；输入前务必先 `sky__activate_window`。

## 协议逆向依据

`SKY_WINDOWS_PROTOCOL_SPEC.md` 记录了对原版 `codex-computer-use.exe`（Rust 原生二进制）的全部逆向证据：20 方法名单、审批流、中断标记、浏览器 URL 策略门禁、枚举身份、截图/输入语义及全部锚点地址。`engine/` 以该 spec 为行为基准，并通过与原版助手的 A/B 差分探针验证一致性。

## 免责声明

非官方爱好者项目，与 OpenAI、DeepSeek 无关。`engine/` 为本仓库原创代码；原版 `codex-computer-use.exe`（如你选择使用）及其行为归上游所有，请遵守上游许可与服务条款。
