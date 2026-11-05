// Browser URL policy gate A/B: engine vs original, each with and without the
// documented local-testing escape hatch.
//
//   node tools/browser-gate-ab.mjs <hwnd> <app> [canon]
//
// Expected after the fix:
//   engine / without key -> the byte-exact "could not determine" refusal
//   engine / with key    -> full UIA tree
//   real   / without key -> the same refusal
//   real   / with key    -> full UIA tree
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const L = process.env.LOCALAPPDATA;
const bin = path.join(L, 'dsh-computer-use', 'bin');

function findReal() {
  const root = path.join(L, 'OpenAI', 'Codex', 'runtimes', 'cua_node');
  for (const v of fs.existsSync(root) ? fs.readdirSync(root) : []) {
    const p = path.join(root, v, 'bin', 'node_modules', '@oai', 'sky', 'bin', 'windows', 'codex-computer-use.exe');
    if (fs.existsSync(p)) return p;
  }
  return undefined;
}

const ENGINE = path.resolve(import.meta.dirname, '..', '..', 'sky-rs', 'target', 'debug', 'sky-engine.exe');
const REAL = findReal();

function open(exe, home) {
  const c = spawn(exe, ['--parent-pid', String(process.pid)], {
    cwd: bin,
    env: { ...process.env, CODEX_HOME: home, CODEX_CLI_PATH: path.join(bin, 'codex.exe') },
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  });
  let buf = '';
  const pend = new Map();
  let nid = 1;
  c.stdout.on('data', (x) => {
    buf += x;
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const l = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (!l) continue;
      let o;
      try { o = JSON.parse(l); } catch { continue; }
      const p = pend.get(o.id);
      if (p) { pend.delete(o.id); p(o); }
    }
  });
  const stderr = [];
  c.stderr.on('data', (x) => stderr.push(String(x)));
  return {
    req: (m, p = {}, meta = {}, b = 15000) => new Promise((res, rej) => {
      const id = nid++;
      const t = setTimeout(() => { pend.delete(id); rej(new Error('timeout ' + m)); }, b + 3000);
      pend.set(id, (o) => { clearTimeout(t); res(o); });
      c.stdin.write(JSON.stringify({ id, method: m, params: p, meta }) + '\n');
    }),
    kill: () => c.kill(),
    stderr: () => stderr.join(''),
  };
}

const hwnd = Number(process.argv[2]);
const appArg = process.argv[3];
const canon = process.argv[4] ?? 'chrome.exe';
if (!Number.isFinite(hwnd) || !appArg) {
  console.error('usage: node tools/browser-gate-ab.mjs <hwnd> <app> [canon]');
  process.exit(2);
}

const targets = [['engine', ENGINE]];
if (REAL) targets.push(['real', REAL]);
else console.log('(original codex-computer-use.exe not found; engine only)');

const KEY = 'x-codex-browser-use-security-mode';
const VAL = 'disabled-for-local-testing';

const results = [];
for (const [name, exe] of targets) {
  const h = open(exe, path.join(L, 'dsh-computer-use', `gate-${name}-home`));
  await new Promise((r) => setTimeout(r, 900));
  for (const mode of ['without', 'with', 'with-2nd', 'with-3rd']) {
    // Reset first: a refusal stops the turn until end_turn, so the second
    // probe would otherwise inherit the first one's stopped state.
    await h.req('end_turn', {}, { 'x-oai-cua-request-budget-ms': 5000 }).catch(() => {});
    const meta = { 'x-oai-cua-request-budget-ms': 15000, 'x-oai-cua-approved-app': canon };
    if (mode !== 'without') meta[KEY] = VAL;
    const o = await h.req('get_window_state',
      { window: { app: appArg, id: hwnd }, include_screenshot: false, include_text: true }, meta);
    let verdict;
    if (!o.ok) {
      const msg = typeof o.error === 'string' ? o.error : JSON.stringify(o.approvalRequest ?? o.error);
      verdict = msg.includes('could not determine the current browser URL') ? 'REFUSED(url-policy)' : `ERR ${msg}`;
    } else {
      const tree = o.result?.accessibility?.tree ?? '';
      verdict = `OK tree=${tree.split('\n').length} lines, shot=${(o.result?.screenshots ?? []).length}`;
      if (process.env.DUMP) verdict += `\n----- ${name}/${mode} tree -----\n${tree}\n----- end -----`;
    }
    results.push(`${name.padEnd(6)} / ${mode.padEnd(7)} -> ${verdict}`);
  }
  h.kill();
}
console.log(results.join('\n'));
process.exit(0);
