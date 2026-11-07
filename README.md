# computer-use-codex2dsh

给 DeepSeek Harness 用的 Windows 电脑操作插件。14 个 `sky__*` 工具，底层是自己用 Rust 重写的 Sky 助手（`sky-engine`），运行时不依赖 ChatGPT 或 Codex。

`computer-use-codex2dsh` 是一个独立的 Cordis 插件：它把 `engine/` 里的 `sky-engine` 接到 DSH 上，模型因此能看窗口、点击、打字、滚动、拖拽。引擎复刻的是 Sky `codex-computer-use.exe` 的 stdio JSON-RPC 行为。插件本身是 Node.js 写的客户端和监管进程，运行时不连 ChatGPT/Codex 的服务，不读账号数据，也不用网络。

## 它做什么

- 向 Harness 注册 14 个 `sky__*` 工具，占住 `computerUse` 的注册位。
- 第一次调用时把 `sky-engine` 和本地 `app-server` 桩复制到 `%LOCALAPPDATA%\dsh-computer-use\bin`，用 `--parent-pid` 启动并监管；卸载插件时把进程收干净。
- 截图经 MCP 适配器投影成图片附件；UIA 树带元素索引和窗口相对包围盒。插件这层另外做快照新鲜度校验、遮挡判断和自我排除。
- 截图支持 `scale` 缩放和 `region` 裁剪，省 token。在缩小或裁剪后的图上量的坐标可以直接用于点击、拖拽、滚动。同一张快照上的一串动作可以用 `sky__batch` 一次发完。

## 引擎

`engine/` 是 `codex-computer-use.exe` 的行为兼容重写（Rust，`windows` + `uiautomation`）：

- 20 个 RPC 方法都接上了：`list_apps`、`list_windows`、`get_window`、`get_window_state`、`activate_window`、`click`、`click_element`、`scroll_element`、`drag`、`scroll`、`type_text`、`press_key`、`set_value`、`perform_secondary_action`、`launch_app`、`end_turn`、`close`、`diagnostic_state`、`start_audio_recording`、`stop_audio_recording`。音频录制那两个只是占位：开了 `SKY_ENABLE_AUDIO=1` 也还是回 `not implemented`。其余的错误文案、审批/回合/预算 meta、130 退出码和 Esc 标记都对齐 [SKY_WINDOWS_PROTOCOL_SPEC.md](SKY_WINDOWS_PROTOCOL_SPEC.md)。
- 窗口和应用枚举（含 taskbar AUMID 探测、StartApps/UserAssist 使用记录）、WGC 窗口截图（WIC 编码 JPEG）、UIA 元素树和 pattern 动作、SendInput 输入、app-server 握手和策略查询、受限环境变量补全、快照缓存、`diagnostic_state`。

构建：

```sh
cd engine && cargo build --release
```

产物是 `engine/target/release/sky-engine.exe`。

接到插件上，二选一：

```yaml
# cordis.patch.yml 的 pluginConfig
helperPath: "C:/path/to/engine/target/release/sky-engine.exe"
```

不配 `helperPath` 也行，插件会去找本机 Codex/ChatGPT 桌面版自带的原版助手。两条路都支持。

## 工具

| 工具 | 干什么 |
|------|------|
| `sky__list_apps` / `sky__list_windows` / `sky__get_window` | 列应用和窗口，拿到窗口句柄 `{app, id}` |
| `sky__get_window_state` | 一次拿到 JPEG 截图和 UIA 树。树里的元素索引只对这次快照有效。可以传 `scale`（0.1–1）出小图，`region {x,y,width,height}`（逻辑窗口坐标）只截一块。树行尾带窗口相对包围盒 `@x,y WxH` |
| `sky__activate_window` | 把窗口提到前台。发任何输入之前都要先做，因为输入只进前台窗口 |
| `sky__click` | 按元素索引点（首选），或按快照坐标点（必须带 `screenshotId`） |
| `sky__drag` / `sky__scroll` | 按快照坐标拖、滚（必须带 `screenshotId`）。拖拽会按距离插值，到落点停一下再抬键，长距离手势不会断 |
| `sky__batch` | 一次按顺序执行最多 50 个动作（点击、拖拽、滚动、按键、文本、等待、激活）。整批只校验一次 `screenshotId`，坐标自动沿用同一张图的裁剪和缩放 |
| `sky__type_text` / `sky__press_key` | 输入字面文本 / 按组合键（`Enter`、`ctrl+s` 等） |
| `sky__set_value` / `sky__perform_secondary_action` | 按 UIA value pattern 写值 / 执行次级动作（Raise、Scroll Up/Down/Left/Right、Expand、Collapse） |
| `sky__launch_app` | 按应用 id、AUMID、exe 路径或应用名启动应用 |

## 前置要求

- Windows 10/11。
- DSH 0.2.2-rc.1。peer 依赖 `@deepseek-ai/dsh-system-prompt` 和 `dsh-tools` 是 `^0.2.2-rc.1`，cordis 是 `~4.1.0`；0.2.1 及更早会被 bundle 的版本校验挡住。
- Node.js。要 `node.exe` 来跑本地 app-server 桩，可以用 `nodePath` 指定。
- 用自带引擎时不用装 OpenAI Codex 或 ChatGPT 桌面版。

## 安装

`plugin_manager` 装本地目录用的是 `link:` 协议，不会装包的 `dependencies`。先把运行时依赖装进包里：

```sh
npm install --omit=dev
```

然后把仓库放到 DSH profile 能找到的位置，用 plugin manager 安装；或者把 `cordis.patch.yml` 里的两条 `insert` 合进 profile composition，`name:` 指向 `computer-use-codex2dsh`。插件标记为 `win32`。

## 配置

| 选项 | 默认 | 说明 |
|------|------|------|
| `helperPath` | 自动发现 | 助手二进制的绝对路径，推荐指到 `engine/target/release/sky-engine.exe`。留空就去自动发现本机 Codex/ChatGPT 桌面版自带的原版助手 |
| `nodePath` | 自动发现 | 跑 app-server 桩的 `node.exe` |
| `stageDir` | `%LOCALAPPDATA%\dsh-computer-use\bin` | 助手和桩的暂存目录，必须是 Medium 完整性级别 |
| `codexHome` | `%LOCALAPPDATA%\dsh-computer-use\codex-home` | 助手私有的 CODEX_HOME，放中断标记和缓存 |
| `autoApprove` | `[]` | 不用问就放行的应用，大小写不敏感。助手报的哪种身份都能写：app id、`process:<path>`、exe 文件名（`notepad` / `notepad.exe`）、AUMID 包前缀（`Microsoft.WindowsNotepad`）、本地化显示名（`记事本`）。Harness 自己永远不会被放行 |
| `enforceBrowserUrlPolicy` | `false` | 设成 `true` 就不再发那个绕过助手浏览器 URL 策略的本地测试 meta |
| `timeoutMs` / `launchTimeoutMs` | `10000` / `15000` | 单次请求超时 / 启动超时，单位毫秒 |

## 安全模型

- 逐应用审批，默认拒绝：助手第一次碰某个应用会发 `approvalRequest`，插件转给 Harness 的审批服务问用户。没有审批服务，或者用户拒绝，就不放行。`allowed-once` 只在一个 agent 会话里记住。
- Harness 自己不能被操作：所有输入类工具（点击、输入、按键、激活等）和审批层都会把指向 Harness 的目标挡掉，`autoApprove` 也解不开。
- 物理 Esc：只认真实按键。注入的 `SendInput`（包括插件自己发的 `press_key Escape`）带 `LLKHF_INJECTED`/`SKY_ESC` 标记，不会触发中断。助手以 130 退出并写中断标记后，本回合剩下的调用都会直接报“用户已停止电脑操作”；回合往前走或者标记被清掉就恢复。
- 坐标守卫：坐标输入必须带该窗口最新快照的 `screenshotId`。快照时被别的窗口盖住的点直接拒绝；目标应用自己的小菜单不算遮挡。输入一送达，旧快照的坐标就作废。
- 这些护栏只是护栏，桌面操作请在能人工盯着的情况下用。

## 已知限制（实测）

- 有些现代应用不给完整的 UIA 树（看窗口状态），只能靠截图加坐标或键盘。
- UIA 树和截图都走跨进程连接。助手进程如果被运行环境限制（受限令牌、命名管道封禁），跨进程 UIA 只能拿到非客户区骨架，WGC 也拿不到帧。插件从宿主正常派生时没有这个问题（实测原生对话框能拿到 27 行完整树和 `hasContent` 截图）。引擎按原版行为订阅 structure-changed 事件来唤醒挂起的 provider（`wake_provider`）。
- 已知协议偏差：`approvalRequest` 的 JSON 字段顺序和原版不同（字母序 vs 结构体定义序）。JSON 语义一样，解析方感觉不到。
- `explorer.exe` 这类共享系统进程的窗口会报 `executable has multiple signatures`，没法寻址。
- 锁屏期间截图返回权限错误，降级成无截图，和原版一致。
- 遮挡判断只能做到“尽力”：快照里同时抓到了覆盖窗口才能判断。发输入前先 `sky__activate_window`。

## 协议依据

`SKY_WINDOWS_PROTOCOL_SPEC.md` 记了原版 `codex-computer-use.exe` 的全部逆向证据：20 个方法、审批流、中断标记、浏览器 URL 策略、枚举身份、截图/输入语义、锚点地址。`engine/` 按这份 spec 写，并用 A/B 探针和原版逐项对比过。

## 免责声明

非官方项目，和 OpenAI、DeepSeek 都没有关系。`engine/` 是本仓库自己写的；原版 `codex-computer-use.exe`（如果你选择用它）归上游，请遵守它的许可和服务条款。
