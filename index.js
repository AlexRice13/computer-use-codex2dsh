// 给 Harness 用的 Windows 电脑操作插件，独立运行，不依赖 ChatGPT 或 Codex。
//
// 桌面操作走逆向出来的 Sky 助手（codex-computer-use.exe），按观察到的
// stdio JSON-RPC 协议通信。助手和本地 app-server 桩进程由插件复制到
// %LOCALAPPDATA% 下的私有目录并启动，之后一直由插件看管；运行时不读
// ChatGPT/Codex 账号，也不联网。
//
// 协议证据见本包里的 SKY_WINDOWS_PROTOCOL_SPEC.md；sky-rs/（发布包里叫 engine/）
// 是用 Rust 重写的助手。
import Schema from '@deepseek-ai/schemastery';
import { ComputerUseProviderName } from '@deepseek-ai/dsh-computer-use/brand';

import { createApprovalBridge } from './lib/approvals.js';
import { createToolDefinitions } from './lib/tools.js';
import { ensureStaged } from './lib/staging.js';
import { SkyTransport } from './lib/transport.js';

/** 插件在 Cordis 注册表里的名字。 */
export const name = 'computer-use-sky';

/** 缺了这三个服务，插件就注册不了工具。 */
export const inject = ['computerUse', 'tools', 'systemPrompt'];

export const Config = Schema.object({
  helperPath: Schema.string().description('Absolute path to codex-computer-use.exe. Auto-discovered from %LOCALAPPDATA%\\OpenAI\\Codex\\runtimes\\cua_node when omitted.'),
  nodePath: Schema.string().description('Absolute path to node.exe, used to run the local app-server stub. Discovered from PATH when omitted.'),
  stageDir: Schema.string().description('Directory the helper, node launcher and app-server stub are staged into. Defaults to %LOCALAPPDATA%\\dsh-computer-use\\bin.'),
  codexHome: Schema.string().description('Private CODEX_HOME for the helper (interrupt markers, caches). Defaults to %LOCALAPPDATA%\\dsh-computer-use\\codex-home.'),
  autoApprove: Schema.array(Schema.string()).description('Apps approved without asking (case-insensitive). Accepts any identity the helper reports: app id, "process:<path>" value, executable name ("notepad" / "notepad.exe"), AUMID package prefix ("Microsoft.WindowsNotepad"), or display name including localized names ("记事本"). The Harness itself can never be approved.').default([]),
  enforceBrowserUrlPolicy: Schema.boolean().description('When true, do not send the local-testing meta that bypasses the helper browser URL policy gate.').default(false),
  timeoutMs: Schema.number().description('Per-request timeout in milliseconds.').default(10000),
  launchTimeoutMs: Schema.number().description('launch_app timeout in milliseconds.').default(15000),
});

const PROVIDER_NAME = 'sky-windows';

const GUIDANCE = `The sky__ computer-use tools operate this Windows host desktop through a locally staged native helper. Discover the exact app and window first, then take a fresh sky__get_window_state snapshot before acting; element_index values and cached bounds belong to that snapshot only and are invalidated by the next snapshot of the same window.

Pass the window handle as {app, id}, where app is the process:... address from a listing (not the sky__list_apps id) and id is the raw HWND. Prefer element targets from the latest tree; each tree row ends with window-relative logical bounds (@x,y WxH) for coordinate fallbacks, and when a window exposes no usable UIA tree (common for UWP apps such as Notepad), drive it from the screenshot with coordinates instead of re-requesting the tree. Keyboard and text input reaches the foreground window: call sky__activate_window on the target immediately before any input tool.

Screenshots are expensive: pass scale (0.1-1) to sky__get_window_state for a smaller JPEG and region {x,y,width,height} to capture only the panel you need. Coordinates measured on that smaller or cropped image stay valid for click/drag/scroll — the helper undoes the crop and scale. When you already know several measured points, send them together with sky__batch (clicks, drags, scrolls, keys, text, waits) instead of taking a new snapshot between every action; a paint stroke sequence or a repeated click is one call.

The helper settles the UI after each action, but a delivered click does not prove the requested outcome: verify it from fresh state. After a cancellation, inspect the current state before retrying because delivered input is not rolled back. Other sessions and applications may change the same desktop between calls.

The first use of an app asks the user for permission. A refusal is final for this conversation: do not retry that app or attempt to route around the refusal. When a tool result reports that the user stopped Computer Use with the physical Escape key, stop every sky__ call for the rest of the turn and send a final message noting the interruption.`;

/**
 * 申请 computerUse 的独占注册位，拉起一个助手进程，再把工具注册给模型。
 * 启动中途失败会回滚已经注册的东西；卸载时先停助手，再放注册位。
 */
export async function apply(ctx, config) {
  const lifetime = new AbortController();
  const pending = new Set();

  ctx.on('internal/plugin', (fiber) => {
    if (fiber === ctx.fiber && fiber.uid === null) lifetime.abort();
  }, { global: true });

  // 每个会话当前的 turn 号，从会话日志里刷新。助手要靠一对非空的
  // (session_id, turn_id) 做 Esc 中断和回合清理。
  const turns = new Map();
  ctx.on('session/event', (session, event) => {
    if (event?.type !== 'turn/start') return;
    const turn = event?.data?.turn ?? event?.turn;
    if (Number.isFinite(turn)) turns.set(String(session.id), turn);
  });

  const decide = createApprovalBridge(ctx, config);

  let transport;
  const ensureTransport = () => {
    if (transport !== undefined && transport.alive) return transport;
    transport = undefined;
    const staged = ensureStaged(config ?? {}, ctx.get?.('locale') ?? 'en');
    const created = new SkyTransport({
      ...staged,
      timeoutMs: config?.timeoutMs,
      launchTimeoutMs: config?.launchTimeoutMs,
      enforceBrowserUrlPolicy: config?.enforceBrowserUrlPolicy,
      onApproval: decide,
    });
    created.onExit(() => {
      if (transport === created) transport = undefined;
    });
    transport = created;
    return created;
  };

  let ready = Promise.resolve();
  const dispose = ctx.effect(function* () {
    yield ctx.computerUse.register(ComputerUseProviderName(PROVIDER_NAME));
    yield async () => {
      lifetime.abort();
      await ready.catch(() => {});
      await Promise.allSettled([...pending]);
      if (transport !== undefined) {
        const closing = transport;
        transport = undefined;
        await closing.close().catch(() => {});
        closing.kill();
      }
    };
    const child = ctx.plugin({
      name: 'computer-use-sky-runtime',
      inject: ['tools', 'systemPrompt'],
      apply: mountRuntime,
    });
    yield child.dispose;
    ready = Promise.resolve(child).then(() => {});
  }, 'computer-use-sky.runtime');

  try {
    await ready;
  } catch (error) {
    await dispose();
    throw error;
  }

/** 工具注册在子插件里；助手进程的收尾在外层。 */
  function mountRuntime(inner) {
    for (const definition of createToolDefinitions(inner, {
      ensureTransport,
      turnProvider: (sessionId) => {
        const turn = turns.get(String(sessionId));
        return turn === undefined ? '0' : String(turn);
      },
    })) {
      inner.tools.register(definition);
    }
    inner.systemPrompt.section({
      name: 'computer-use:sky-windows',
      order: inner.systemPrompt.getSectionOrder('TOOL_COMPUTER_USE'),
      text: GUIDANCE,
    });
    inner.on('tools/execute', async (exec, next) => {
      if (!exec.name.startsWith('sky__')) return next();
      const upstream = exec.signal;
      exec.signal = AbortSignal.any([upstream, lifetime.signal]);
      const operation = Promise.resolve().then(next);
      pending.add(operation);
      try {
        return await operation;
      } finally {
        pending.delete(operation);
        exec.signal = upstream;
      }
    });
    inner.logger?.info?.('computer-use-sky: provider mounted; the native helper starts on first use');
  }
}
