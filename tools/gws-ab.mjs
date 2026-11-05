// A/B get_window_state text tree on an arbitrary window: real helper vs engine.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const L = process.env.LOCALAPPDATA;
const root = path.join(L, 'OpenAI', 'Codex', 'runtimes', 'cua_node');
let realExe = '';
for (const v of fs.readdirSync(root)) {
  const p = path.join(root, v, 'bin', 'node_modules', '@oai', 'sky', 'bin', 'windows', 'codex-computer-use.exe');
  if (fs.existsSync(p)) realExe = p;
}
const ENGINE = path.resolve('target/debug/sky-engine.exe');
const bin = path.join(L, 'dsh-computer-use', 'bin');

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
  c.stdout.on('data', (x) => { buf += x; let i; while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i).trim(); buf = buf.slice(i + 1); if (!l) continue; let o; try { o = JSON.parse(l); } catch { continue; } const p = pend.get(o.id); if (p) { pend.delete(o.id); p(o); } } });
  return {
    req: (m, p = {}, b = 15000) => new Promise((res, rej) => {
      const id = nid++;
      const t = setTimeout(() => { pend.delete(id); rej(new Error('timeout ' + m)); }, b + 2000);
      pend.set(id, (o) => { clearTimeout(t); res(o); });
      c.stdin.write(JSON.stringify({ id, method: m, params: p, meta: { 'x-oai-cua-request-budget-ms': b, 'x-oai-cua-approved-app': process.env.CANON ?? appArg } }) + '\n');
    }),
    kill: () => c.kill(),
  };
}

const hwnd = Number(process.argv[2]);
const appArg = process.argv[3];
const real = open(realExe, path.join(L, 'dsh-computer-use', 'ab-real-home'));
const eng = open(ENGINE, path.join(L, 'dsh-computer-use', 'ab-eng-home'));
await new Promise((r) => setTimeout(r, 800));
const P = { window: { app: appArg, id: hwnd }, include_screenshot: false, include_text: true };
const [R, E] = await Promise.all([real.req('get_window_state', P), eng.req('get_window_state', P)]);
const dump = (name, o) => {
  if (!o.ok) { console.log(`=== ${name} ERR ===`); console.log(JSON.stringify(o.approvalRequest ?? o.error)); return; }
  const tree = o.result?.accessibility?.tree || '(none)';
  const lines = tree.split('\n');
  console.log(`=== ${name} (${lines.length} lines) ===`);
  console.log(lines.slice(0, 20).join('\n'));
};
dump('real', R);
dump('engine', E);
real.kill();
eng.kill();
process.exit(0);
