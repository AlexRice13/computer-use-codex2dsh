// Round-4: compare list_windows enumeration between real helper and engine on the live desktop.
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

function open(name, exe, home) {
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
      c.stdin.write(JSON.stringify({ id, method: m, params: p, meta: { 'x-oai-cua-request-budget-ms': b } }) + '\n');
    }),
    close: async () => { try { const id = nid++; const pr = new Promise((r) => pend.set(id, r)); c.stdin.write(JSON.stringify({ id, method: 'close', params: {}, meta: {} }) + '\n'); await Promise.race([pr, new Promise((r) => setTimeout(r, 2500))]); } catch {} c.kill(); },
  };
}

const real = open('real', realExe, path.join(L, 'dsh-computer-use', 'ab-real-home'));
const eng = open('engine', ENGINE, path.join(L, 'dsh-computer-use', 'ab-eng-home'));
await new Promise((r) => setTimeout(r, 900));
const [R, E] = await Promise.all([real.req('list_windows'), eng.req('list_windows')]);
const norm = (s) => (s || '').toLowerCase().replace(/\s+/g, ' ').trim();
const wr = new Map((R.result || []).map((w) => [w.id, w]));
const we = new Map((E.result || []).map((w) => [w.id, w]));
console.log(`real=${wr.size} engine=${we.size}`);
const ids = new Set([...wr.keys(), ...we.keys()]);
for (const id of [...ids].sort((a, b) => a - b)) {
  const a = wr.get(id), b = we.get(id);
  if (!a) console.log(`ONLY-ENG  ${id} "${b.title}" <${b.app}>`);
  else if (!b) console.log(`ONLY-REAL ${id} "${a.title}" <${a.app}>`);
  else if (norm(a.title) !== norm(b.title) || norm(a.app) !== norm(b.app)) console.log(`MISMATCH  ${id} real="${a.title}"<${a.app}> eng="${b.title}"<${b.app}>`);
}
await real.close();
await eng.close();
process.exit(0);
