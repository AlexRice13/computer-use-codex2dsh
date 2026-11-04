// Standalone Windows computer-use provider for the Harness.
//
// It drives the desktop through the reverse-engineered Sky helper
// (codex-computer-use.exe) over the observed stdio JSON-RPC contract, fully
// detached from ChatGPT and Codex at runtime: the helper and a local
// app-server stub are staged into a private %LOCALAPPDATA% directory and
// supervised by this plugin. Nothing is read from a ChatGPT/Codex account,
// and no network access is used.
//
// See SKY_WINDOWS_PROTOCOL_SPEC.md in this package for the protocol evidence
// and engine/ for the native Rust reimplementation of the helper.
import Schema from '@deepseek-ai/schemastery';
import { ComputerUseProviderName } from '@deepseek-ai/dsh-computer-use/brand';

import { createApprovalBridge } from './lib/approvals.js';
import { createToolDefinitions } from './lib/tools.js';
import { ensureStaged } from './lib/staging.js';
import { SkyTransport } from './lib/transport.js';

/** Cordis plugin identity for the standalone Sky computer-use provider. */
export const name = 'computer-use-sky';

/** Services required before the provider can publish its tool surface. */
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

const GUIDANCE = `The sky__ computer-use tools operate this Windows host desktop through a locally staged native helper. Discover the exact app and window first, then take a fresh sky_get_window_state snapshot before acting; element_index values and cached bounds belong to that snapshot only and are invalidated by the next snapshot of the same window.

Pass the window handle as {app, id}, where app is the process:... address from a listing (not the sky__list_apps id) and id is the raw HWND. Prefer element targets from the latest tree; when a window exposes no usable UIA tree (common for UWP apps such as Notepad), drive it from the screenshot with coordinates instead of re-requesting the tree. Keyboard and text input reaches the foreground window: call sky__activate_window on the target immediately before any input tool.

The helper settles the UI after each action, but a delivered click does not prove the requested outcome: verify it from fresh state. After a cancellation, inspect the current state before retrying because delivered input is not rolled back. Other sessions and applications may change the same desktop between calls.

The first use of an app asks the user for permission. A refusal is final for this conversation: do not retry that app or attempt to route around the refusal. When a tool result reports that the user stopped Computer Use with the physical Escape key, stop every sky__ call for the rest of the turn and send a final message noting the interruption.`;

/**
 * Own the exclusive computer-use registration, one helper process and the
 * model-facing tool surface. Startup failures roll back every registration;
 * unload stops the helper before the registration is released.
 */
export async function apply(ctx, config) {
  const lifetime = new AbortController();
  const pending = new Set();

  ctx.on('internal/plugin', (fiber) => {
    if (fiber === ctx.fiber && fiber.uid === null) lifetime.abort();
  }, { global: true });

  // Per-session turn numbers, refreshed from the session log. The helper needs
  // a non-blank (session_id, turn_id) pair for ESC interrupts and turn cleanup.
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

  /** The child owns tool registrations; the outer effect owns helper teardown. */
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
