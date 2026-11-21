// Unit tests for the Antigravity Computer Use plugin: tool surface (including
// sky__close), self-protection, dialog approval memory, and Stop hook teardown.
// Run after `npm install`: node test/verify.test.mjs
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';

import { createSkyMcpServer, resolveConfig } from '../scripts/server.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const pluginRoot = path.resolve(here, '..');

let closeCount = 0;
let promptCount = 0;
let fakeAlive = true;
const fakeTransport = {
  get alive() { return fakeAlive; },
  async request() { return []; },
  async close() { closeCount += 1; fakeAlive = false; },
  kill() { fakeAlive = false; },
};

let released = 0;
const sky = createSkyMcpServer(resolveConfig({ idleCloseMs: 0 }), {
  ensureTransport: () => { fakeAlive = true; return fakeTransport; },
  onHelperReleased: () => { released += 1; },
  promptDialog: async () => { promptCount += 1; return 'allowed-once'; },
  logger: { info() {}, warn() {} },
});

const client = new Client({ name: 'test-client', version: '0.2.0' });
const [sSide, cSide] = InMemoryTransport.createLinkedPair();
await Promise.all([sky.server.connect(sSide), client.connect(cSide)]);

const { tools } = await client.listTools();
assert.equal(tools.length, 15);
assert.ok(tools.some((t) => t.name === 'sky__close'));

// Protected host apps are blocked before reaching the helper
const denied = await client.callTool({
  name: 'sky__get_window_state',
  arguments: { window: { app: 'process:C:\\Program Files\\Antigravity\\Antigravity.exe', id: 1 } },
});
assert.equal(denied.isError, true);
assert.match(denied.content[0].text, /denyApps/);

// Activate transport, approve an app once, then verify sky__close shuts helper down while keeping session grant
await client.callTool({ name: 'sky__list_windows', arguments: {} });
assert.equal(await sky.decide({ app: 'chrome.exe', displayName: 'Google Chrome' }, { agent: { id: 's1' } }), true);
assert.equal(promptCount, 1);

const closedOut = await client.callTool({ name: 'sky__close', arguments: {} });
assert.notEqual(closedOut.isError, true);
assert.equal(closeCount, 1);
assert.equal(released >= 1, true);
assert.equal(sky.turn(), 2);

assert.equal(await sky.decide({ app: 'chrome.exe', displayName: 'Google Chrome' }, { agent: { id: 's1' } }), true);
assert.equal(promptCount, 1);

await client.close();
await sky.close();

// Stop hook returns clean {} on stdout with exit code 0
const hookRes = spawnSync(process.execPath, [path.join(pluginRoot, 'scripts', 'stop-hook.mjs')], {
  cwd: pluginRoot,
  input: JSON.stringify({ hookEventName: 'Stop', terminationReason: 'NO_TOOL_CALL' }),
  encoding: 'utf8',
});
assert.equal(hookRes.status, 0);
assert.equal(hookRes.stdout.trim(), '{}');

console.log('PASS  antigravity plugin tests');
