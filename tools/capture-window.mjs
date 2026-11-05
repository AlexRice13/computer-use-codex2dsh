// Capture one window through the engine and write the JPEG to disk.
// Proves the helper's capture path is sound when the Harness viewer cannot
// decode the returned data URL.
//
//   node tools/capture-window.mjs <hwnd> <app> <out.jpg>
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const L = process.env.LOCALAPPDATA;
const bin = path.join(L, 'dsh-computer-use', 'bin');
const ENGINE = path.resolve(import.meta.dirname, '..', '..', 'sky-rs', 'target', 'debug', 'sky-engine.exe');

const [hwnd, app, out] = process.argv.slice(2);
if (!hwnd || !app || !out) {
  console.error('usage: node tools/capture-window.mjs <hwnd> <app> <out.jpg>');
  process.exit(2);
}

const child = spawn(ENGINE, ['--parent-pid', String(process.pid)], {
  cwd: bin,
  env: { ...process.env, CODEX_HOME: path.join(L, 'dsh-computer-use', 'capture-home'), CODEX_CLI_PATH: path.join(bin, 'codex.exe') },
  stdio: ['pipe', 'pipe', 'pipe'],
  windowsHide: true,
});
let buf = '';
const pend = new Map();
let nid = 1;
child.stdout.on('data', (x) => {
  buf += x;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    let o;
    try { o = JSON.parse(line); } catch { continue; }
    const p = pend.get(o.id);
    if (p) { pend.delete(o.id); p(o); }
  }
});
const req = (m, p = {}, meta = {}, b = 20000) => new Promise((res, rej) => {
  const id = nid++;
  const t = setTimeout(() => { pend.delete(id); rej(new Error('timeout ' + m)); }, b + 4000);
  pend.set(id, (o) => { clearTimeout(t); res(o); });
  child.stdin.write(JSON.stringify({ id, method: m, params: p, meta }) + '\n');
});

await new Promise((r) => setTimeout(r, 900));
const meta = {
  'x-oai-cua-request-budget-ms': 20000,
  'x-oai-cua-approved-app': app.toLowerCase() === 'chrome' ? 'chrome.exe' : app,
  'x-codex-browser-use-security-mode': 'disabled-for-local-testing',
};
// A background/occluded Chromium can refuse PrintWindow, so bring it up first.
await req('activate_window', { window: { app, id: Number(hwnd) } }, meta).catch(() => {});
await new Promise((r) => setTimeout(r, 400));
const o = await req('get_window_state',
  { window: { app, id: Number(hwnd) }, include_screenshot: true, include_text: false }, meta);
child.kill();

if (!o.ok) {
  console.error('engine error: ' + JSON.stringify(o.approvalRequest ?? o.error));
  process.exit(1);
}
const shot = (o.result?.screenshots ?? [])[0];
if (!shot?.url?.startsWith('data:image/jpeg;base64,')) {
  console.error('no jpeg in result: ' + JSON.stringify(o.result?.screenshots ?? null));
  process.exit(1);
}
const jpeg = Buffer.from(shot.url.split(',', 2)[1], 'base64');
fs.writeFileSync(out, jpeg);
console.log(`wrote ${out}  ${jpeg.length} bytes  ${shot.width}x${shot.height}  title="${o.result?.window?.title}"`);
process.exit(0);
