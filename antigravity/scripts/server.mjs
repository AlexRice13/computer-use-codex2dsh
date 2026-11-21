#!/usr/bin/env node
// Antigravity Computer Use stdio MCP server backed by codex-computer-use.exe.
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, realpathSync, unlinkSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { Server } from '@modelcontextprotocol/sdk/server/index.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { CallToolRequestSchema, ElicitResultSchema, ListToolsRequestSchema } from '@modelcontextprotocol/sdk/types.js';

import { approvalKeys, createApprovalBridge, promptWin32Approval } from './lib/approvals.mjs';
import { defaultStagingRoot, ensureStaged } from './lib/staging.mjs';
import { createToolDefinitions } from './lib/tools.mjs';
import { SkyTransport } from './lib/transport.mjs';

const VERSION = '0.2.0';
const ALLOWED_KEYS = new Set([
  'helperPath',
  'nodePath',
  'stageDir',
  'codexHome',
  'autoApprove',
  'denyApps',
  'approvalMode',
  'enforceBrowserUrlPolicy',
  'timeoutMs',
  'launchTimeoutMs',
  'turnIdleMs',
  'idleCloseMs',
  'locale',
]);

export const DEFAULT_DENY_APPS = [
  'Antigravity.exe',
  'Antigravity',
  'WindowsTerminal.exe',
  'Microsoft.WindowsTerminal',
];
export const DEFAULT_TURN_IDLE_MS = 30000;
export const DEFAULT_IDLE_CLOSE_MS = 15000;
const READ_ONLY_TOOLS = new Set([
  'sky__list_apps',
  'sky__list_windows',
  'sky__get_window',
  'sky__get_window_state',
]);

const GUIDANCE = `The sky__* tools control this Windows desktop through codex-computer-use.exe.

Workflow:
1. Discover the target app and window via sky__list_windows (or sky__list_apps + sky__launch_app, then poll sky__list_windows).
2. Pass window handles as {app, id}, where app is windows[].app from the listing (not list_apps[].id) and id is the raw HWND integer.
3. Capture fresh state with sky__get_window_state({window, include_screenshot: true, include_text: true}) before acting. element_index values and screenshotId belong only to that snapshot and are invalidated after state changes.
4. Prefer element_index actions (sky__click with element_index, sky__set_value, sky__perform_secondary_action) when the UIA tree exposes the control. When UIA is empty or incomplete (common in UWP/modern apps like Notepad or Paint), measure logical coordinates on the screenshot and pass {window, x, y, screenshotId}.
5. Call sky__activate_window immediately before keyboard/text input or coordinate interactions so the target window is in the foreground.
6. Do NOT send bare "Escape" via sky__press_key: codex-computer-use's keyboard hook treats VK_ESCAPE as a physical Escape interrupt. Close menus or dialogs by clicking UI buttons instead.
7. Apps require approval unless listed in autoApprove; Antigravity itself and Windows Terminal are always denied. A user refusal or physical Escape stop is final for the turn.
8. IMPORTANT — Exit Computer Use when done: Always call sky__close as soon as you finish your Computer Use steps in the turn (before sending your final reply to the user) so the agent cursor and status pill exit cleanly.`;

export function resolveConfig(raw = {}) {
  if (raw === null || typeof raw !== 'object' || Array.isArray(raw)) {
    throw new TypeError('config must be a JSON object');
  }
  const unknown = Object.keys(raw).filter((key) => !ALLOWED_KEYS.has(key));
  if (unknown.length > 0) throw new TypeError(`unknown config key(s): ${unknown.join(', ')}`);

  const root = defaultStagingRoot();
  const helperPath = typeof raw.helperPath === 'string' && raw.helperPath.trim() !== '' ? raw.helperPath.trim() : undefined;
  const nodePath = typeof raw.nodePath === 'string' && raw.nodePath.trim() !== '' ? raw.nodePath.trim() : undefined;
  const stageDir = typeof raw.stageDir === 'string' && raw.stageDir.trim() !== '' ? raw.stageDir.trim() : path.join(root, 'bin');
  const codexHome = typeof raw.codexHome === 'string' && raw.codexHome.trim() !== '' ? raw.codexHome.trim() : path.join(root, 'codex-home');

  const autoApprove = raw.autoApprove ?? [];
  if (!Array.isArray(autoApprove) || autoApprove.some((x) => typeof x !== 'string')) {
    throw new TypeError('autoApprove must be an array of strings');
  }
  const denyApps = raw.denyApps ?? DEFAULT_DENY_APPS;
  if (!Array.isArray(denyApps) || denyApps.some((x) => typeof x !== 'string')) {
    throw new TypeError('denyApps must be an array of strings');
  }
  const approvalMode = raw.approvalMode ?? 'dialog';
  if (!['dialog', 'elicit-only', 'deny'].includes(approvalMode)) {
    throw new TypeError('approvalMode must be "dialog", "elicit-only", or "deny"');
  }
  const enforceBrowserUrlPolicy = raw.enforceBrowserUrlPolicy === true;
  const timeoutMs = raw.timeoutMs ?? 10000;
  if (typeof timeoutMs !== 'number' || !Number.isFinite(timeoutMs) || timeoutMs < 1000) {
    throw new TypeError('timeoutMs must be a number >= 1000');
  }
  const launchTimeoutMs = raw.launchTimeoutMs ?? 15000;
  if (typeof launchTimeoutMs !== 'number' || !Number.isFinite(launchTimeoutMs) || launchTimeoutMs < 1000) {
    throw new TypeError('launchTimeoutMs must be a number >= 1000');
  }
  const turnIdleMs = raw.turnIdleMs ?? DEFAULT_TURN_IDLE_MS;
  if (typeof turnIdleMs !== 'number' || !Number.isFinite(turnIdleMs) || turnIdleMs < 1000) {
    throw new TypeError('turnIdleMs must be a number >= 1000');
  }
  const idleCloseMs = raw.idleCloseMs ?? DEFAULT_IDLE_CLOSE_MS;
  if (typeof idleCloseMs !== 'number' || !Number.isFinite(idleCloseMs) || idleCloseMs < 0) {
    throw new TypeError('idleCloseMs must be a number >= 0');
  }
  const locale = raw.locale ?? 'zh';
  if (typeof locale !== 'string') throw new TypeError('locale must be a string');

  return {
    helperPath,
    nodePath,
    stageDir,
    codexHome,
    autoApprove,
    denyApps,
    approvalMode,
    enforceBrowserUrlPolicy,
    timeoutMs,
    launchTimeoutMs,
    turnIdleMs,
    idleCloseMs,
    locale,
  };
}

function denyMatcher(list) {
  const deny = new Set(list.map((x) => x.trim().toLowerCase()).filter((x) => x !== ''));
  return (...values) => values.some((value) => approvalKeys(value, undefined).some((key) => deny.has(key)));
}

const denyMessage = (app) => `computer-use refuses to control "${app}": it is protected by "denyApps".`;
const errorResult = (text) => ({ isError: true, content: [{ type: 'text', text }] });

export function createSkyMcpServer(config, overrides = {}) {
  const sessionId = overrides.sessionId ?? randomUUID();
  const now = overrides.now ?? Date.now;
  const logger = overrides.logger ?? {
    info: (...args) => console.error('[computer-use]', ...args),
    warn: (...args) => console.error('[computer-use]', ...args),
  };
  const isDenied = denyMatcher(config.denyApps);
  const zh = config.locale.startsWith('zh');

  const server = new Server(
    { name: 'codex-computer-use', version: VERSION },
    { capabilities: { tools: {} }, instructions: GUIDANCE },
  );

  const supportsForm = () => {
    const elicitation = server.getClientCapabilities()?.elicitation;
    return elicitation !== undefined && (elicitation.form !== undefined || elicitation.url === undefined);
  };

  const promptDialog = overrides.promptDialog ?? promptWin32Approval;

  const approvalService = {
    async request({ app, displayName, riskLevel, reason, displayReason, signal }) {
      if (config.approvalMode === 'deny') return 'denied';
      if (config.approvalMode === 'dialog' && process.platform === 'win32') {
        return promptDialog({
          app: app ?? displayName,
          displayName: displayName ?? app,
          riskLevel,
          locale: config.locale,
          signal,
        });
      }
      if (supportsForm()) {
        try {
          const answer = await server.request({
            method: 'elicitation/create',
            params: {
              mode: 'form',
              message: (zh ? displayReason?.zh : displayReason?.en) ?? reason,
              requestedSchema: {
                type: 'object',
                properties: {
                  allow: {
                    type: 'boolean',
                    title: zh ? '允许' : 'Allow',
                    description: zh ? '本会话内允许 Computer Use 控制此应用' : 'Let Computer Use control this app for this session',
                  },
                },
                required: ['allow'],
              },
            },
          }, ElicitResultSchema, { signal });
          return answer.action === 'accept' && answer.content?.allow === true ? 'allowed-once' : 'denied';
        } catch {
          return 'denied';
        }
      }
      return 'denied';
    },
  };

  const bridge = createApprovalBridge({ get: (name) => (name === 'approval' ? approvalService : undefined), logger }, config);
  const decide = async (approval, info) => {
    if (isDenied(approval.app, approval.displayName)) {
      throw new Error(denyMessage(approval.displayName ?? approval.app));
    }
    return bridge(approval, info);
  };

  let transport;
  let turn = 1;
  let lastActive = 0;
  let inFlight = 0;
  let idleTimer = null;

  const clearIdleTimer = () => {
    if (idleTimer !== null) {
      clearTimeout(idleTimer);
      idleTimer = null;
    }
  };

  const onHelperActivated = overrides.onHelperActivated ?? (() => {});
  const onHelperReleased = overrides.onHelperReleased ?? (() => {});

  async function releaseTransport() {
    clearIdleTimer();
    onHelperReleased();
    const closing = transport;
    transport = undefined;
    if (closing !== undefined) {
      turn += 1;
      await closing.close().catch(() => {});
      closing.kill();
    }
  }

  const armIdleTimer = () => {
    clearIdleTimer();
    if (inFlight === 0 && transport !== undefined && transport.alive && config.idleCloseMs > 0) {
      idleTimer = setTimeout(() => {
        idleTimer = null;
        if (inFlight === 0) void releaseTransport();
      }, config.idleCloseMs);
      idleTimer.unref?.();
    }
  };

  const ensureTransport = () => {
    if (overrides.ensureTransport !== undefined) {
      transport = overrides.ensureTransport();
      onHelperActivated();
      return transport;
    }
    if (transport !== undefined && transport.alive) {
      onHelperActivated();
      return transport;
    }
    transport = undefined;
    const staged = ensureStaged(config, config.locale);
    const created = new SkyTransport({
      ...staged,
      timeoutMs: config.timeoutMs,
      launchTimeoutMs: config.launchTimeoutMs,
      enforceBrowserUrlPolicy: config.enforceBrowserUrlPolicy,
      onApproval: decide,
    });
    created.onExit(() => {
      if (transport === created) {
        transport = undefined;
        clearIdleTimer();
        onHelperReleased();
      }
    });
    transport = created;
    onHelperActivated();
    return created;
  };

  const definitions = createToolDefinitions({
    ensureTransport,
    releaseTransport,
    turnProvider: () => String(turn),
  });
  const byName = new Map(definitions.map((d) => [d.name, d]));

  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: definitions.map((d) => ({
      name: d.name,
      description: d.description,
      inputSchema: d.parameters,
      ...(READ_ONLY_TOOLS.has(d.name) ? { annotations: { readOnlyHint: true } } : {}),
    })),
  }));

  server.setRequestHandler(CallToolRequestSchema, async (request, extra) => {
    const definition = byName.get(request.params.name);
    if (definition === undefined) return errorResult(`Unknown tool: ${request.params.name}`);
    const args = request.params.arguments ?? {};
    clearIdleTimer();
    inFlight += 1;
    const started = now();
    if (lastActive !== 0 && started - lastActive >= config.turnIdleMs) turn += 1;
    lastActive = started;
    try {
      const target = [args.window?.app, args.app].find((value) => typeof value === 'string');
      if (target !== undefined && isDenied(target)) return errorResult(denyMessage(target));
      return await definition.execute(args, {
        agent: { id: sessionId },
        callId: String(extra.requestId),
        signal: extra.signal,
      });
    } catch (error) {
      return errorResult(String(error?.message ?? error));
    } finally {
      inFlight = Math.max(0, inFlight - 1);
      lastActive = now();
      armIdleTimer();
    }
  });

  async function close() {
    await releaseTransport();
    await server.close().catch(() => {});
  }

  return { server, decide, releaseTransport, close, sessionId, turn: () => turn };
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--config' && argv[i + 1] !== undefined) out.config = argv[(i += 1)];
    else if (argv[i].startsWith('--config=')) out.config = argv[i].slice('--config='.length);
    else throw new TypeError(`unknown argument: ${argv[i]} (usage: server.mjs [--config <file.json>])`);
  }
  return out;
}

export function loadMergedConfig(configPath) {
  let base = {};
  if (configPath !== undefined && existsSync(configPath)) {
    base = JSON.parse(readFileSync(configPath, 'utf8'));
  }
  const pluginDataConfig = process.env.PLUGIN_DATA ? path.join(process.env.PLUGIN_DATA, 'config.json') : undefined;
  if (pluginDataConfig && existsSync(pluginDataConfig)) {
    const override = JSON.parse(readFileSync(pluginDataConfig, 'utf8'));
    base = { ...base, ...override };
  }
  return resolveConfig(base);
}

async function main(argv) {
  const args = parseArgs(argv);
  const config = loadMergedConfig(args.config);

  const activeDir = path.join(config.codexHome, 'active-servers');
  const markerFile = path.join(activeDir, `${process.pid}.json`);
  const pipePath = process.platform === 'win32'
    ? `\\\\.\\pipe\\agy-computer-use-${process.pid}`
    : path.join(activeDir, `agy-cu-${process.pid}.sock`);

  const writeMarker = () => {
    try {
      mkdirSync(activeDir, { recursive: true });
      writeFileSync(markerFile, JSON.stringify({ pid: process.pid, pipe: pipePath }), 'utf8');
    } catch {}
  };
  const removeMarker = () => {
    try {
      if (existsSync(markerFile)) unlinkSync(markerFile);
    } catch {}
  };

  const { server, releaseTransport, close } = createSkyMcpServer(config, {
    onHelperActivated: writeMarker,
    onHelperReleased: removeMarker,
  });

  const controlServer = net.createServer((socket) => {
    socket.setEncoding('utf8');
    let received = false;
    const handle = async () => {
      if (received) return;
      received = true;
      await releaseTransport().catch(() => {});
      try { socket.end('ok\n'); } catch {}
    };
    socket.on('data', (chunk) => {
      if (chunk.includes('release')) void handle();
    });
    socket.on('error', () => {});
  });
  controlServer.on('error', () => {});
  try {
    mkdirSync(activeDir, { recursive: true });
    controlServer.listen(pipePath);
    controlServer.unref();
  } catch {}

  let stopping = false;
  const stop = async () => {
    if (stopping) return;
    stopping = true;
    removeMarker();
    try { controlServer.close(); } catch {}
    await close();
    process.exit(0);
  };
  process.once('exit', removeMarker);
  process.stdin.once('end', stop);
  process.once('SIGINT', stop);
  process.once('SIGTERM', stop);
  await server.connect(new StdioServerTransport());
}

function realPath(file) {
  try { return realpathSync(file).toLowerCase(); } catch { return ''; }
}

if (process.argv[1] !== undefined && realPath(process.argv[1]) === realPath(fileURLToPath(import.meta.url))) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(`[computer-use] ${error?.stack ?? error}`);
    process.exit(1);
  });
}
