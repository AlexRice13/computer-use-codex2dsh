// launch_app A/B: real helper vs engine, same app string, same env shape.
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
    stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true,
  });
  let buf = '';
  const pend = new Map();
  let nid = 1;
  c.stdout.on('data', (x) => { buf += x; let i; while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i).trim(); buf = buf.slice(i + 1); if (!l) continue; let o; try { o = JSON.parse(l); } catch { continue; } const p = pend.get(o.id); if (p) { pend.delete(o.id); p(o); } } });
  return {
    req: (m, p = {}, b = 15000, canon) => new Promise((res, rej) => {
      const id = nid++;
      const t = setTimeout(() => { pend.delete(id); rej(new Error('timeout ' + m)); }, b + 2000);
      pend.set(id, (o) => { clearTimeout(t); res(o); });
      c.stdin.write(JSON.stringify({ id, method: m, params: p, meta: { 'x-oai-cua-request-budget-ms': b, ...(canon ? { 'x-oai-cua-approved-app': canon } : {}) } }) + '\n');
    }),
    kill: () => c.kill(),
  };
}

const appArg = process.argv[2];
const canon = process.argv[3] ?? 'notepad.exe';
const real = open(realExe, path.join(L, 'dsh-computer-use', 'ab-real-home'));
const eng = open(ENGINE, path.join(L, 'dsh-computer-use', 'ab-eng-home'));
await new Promise((r) => setTimeout(r, 800));
const [R, E] = await Promise.all([
  real.req('launch_app', { app: appArg }, 15000, canon),
  eng.req('launch_app', { app: appArg }, 15000, canon),
]);
const fmt = (o) => (o.ok ? 'OK ' + JSON.stringify(o.result) : 'ERR ' + JSON.stringify(o.approvalRequest ?? o.error));
console.log('real  :', fmt(R));
console.log('engine:', fmt(E));
real.kill();
eng.kill();
await new Promise((r) => setTimeout(r, 4000));
process.exit(0);
