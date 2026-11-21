// Staging: copies codex-computer-use.exe, node launcher (codex.exe), and the
// app-server stub into a dedicated Medium-integrity directory under %LOCALAPPDATA%.
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { STUB_VERSION, stubSource } from './stub-app-server.mjs';

const HELPER_NAME = 'codex-computer-use.exe';
const LAUNCHER_NAME = 'codex.exe';
const STUB_NAME = 'app-server';
const STAMP_NAME = '.agy-stage.json';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const VENDOR_HELPER = path.resolve(HERE, '..', 'vendor', HELPER_NAME);

export function defaultStagingRoot() {
  const base = process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
  return path.join(base, 'agy-computer-use');
}

function defaultLocalAppData() {
  return process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
}

function safeReaddir(dir) {
  try { return readdirSync(dir); } catch { return []; }
}

export function discoverHelper() {
  const envBin = process.env.OAI_SKY_WINDOWS_BIN?.trim();
  if (envBin) {
    try {
      if (statSync(envBin).isFile() && statSync(envBin).size > 0) return path.resolve(envBin);
    } catch {}
  }

  const runtimes = path.join(defaultLocalAppData(), 'OpenAI', 'Codex', 'runtimes', 'cua_node');
  if (existsSync(runtimes)) {
    let best;
    let bestMtime = 0;
    for (const dir of safeReaddir(runtimes)) {
      for (const pkg of ['sky', 'cua']) {
        const candidate = path.join(runtimes, dir, 'bin', 'node_modules', '@oai', pkg, 'bin', 'windows', HELPER_NAME);
        try {
          const info = statSync(candidate);
          if (info.isFile() && info.size > 0 && info.mtimeMs > bestMtime) {
            best = candidate;
            bestMtime = info.mtimeMs;
          }
        } catch {}
      }
    }
    if (best) return best;
  }

  try {
    const info = statSync(VENDOR_HELPER);
    if (info.isFile() && info.size > 0) return VENDOR_HELPER;
  } catch {}

  return undefined;
}

function discoverNode(configured) {
  if (typeof configured === 'string' && configured.trim() !== '') {
    const candidate = configured.trim();
    try {
      if (statSync(candidate).isFile()) return candidate;
    } catch {}
    return undefined;
  }
  const candidates = [];
  if (typeof process.execPath === 'string' && /node(\.exe)?$/i.test(process.execPath)) {
    candidates.push(process.execPath);
  }
  const found = spawnSync('where.exe', ['node'], { encoding: 'utf8', windowsHide: true });
  if (found.status === 0) candidates.push(...found.stdout.split(/\r?\n/).map((x) => x.trim()).filter(Boolean));
  candidates.push(
    'C:\\Program Files\\nodejs\\node.exe',
    'D:\\Program Files\\nodejs\\node.exe',
    path.join(process.env.ProgramFiles || 'C:\\Program Files', 'nodejs', 'node.exe'),
  );
  for (const candidate of candidates) {
    try {
      if (candidate && statSync(candidate).isFile()) return candidate;
    } catch {}
  }
  return undefined;
}

function sameFile(src, dst) {
  try {
    const a = statSync(src);
    const b = statSync(dst);
    return a.isFile() && b.isFile() && a.size === b.size && a.mtimeMs <= b.mtimeMs + 1000 && b.size > 0;
  } catch {
    return false;
  }
}

function copyInto(src, dst, forceName) {
  const target = path.join(dst, forceName);
  if (!sameFile(src, target)) copyFileSync(src, target);
  return target;
}

function lowLabelPresent(target) {
  const probe = spawnSync('icacls.exe', [target], { encoding: 'utf8', windowsHide: true });
  if (probe.status !== 0) return false;
  return /Low Mandatory Level/i.test(probe.stdout || '');
}

export function ensureStaged(config = {}, locale = 'zh') {
  const root = defaultStagingRoot();
  const stageDir = config.stageDir ? path.resolve(config.stageDir) : path.join(root, 'bin');
  const codexHome = config.codexHome ? path.resolve(config.codexHome) : path.join(root, 'codex-home');
  mkdirSync(stageDir, { recursive: true });
  mkdirSync(path.join(codexHome, 'computer-use'), { recursive: true });

  const sourceHelper = config.helperPath ? path.resolve(config.helperPath) : discoverHelper();
  if (!sourceHelper || !existsSync(sourceHelper)) {
    if (config.helperPath) {
      throw new Error(`The configured "helperPath" does not exist: ${path.resolve(config.helperPath)}. `
        + 'Point it at an existing codex-computer-use.exe or omit it to auto-discover.');
    }
    throw new Error('No codex-computer-use.exe was found. Set "helperPath" in config.json to an existing '
      + 'codex-computer-use.exe, or install the OpenAI Codex / ChatGPT desktop app.');
  }

  const sourceNode = discoverNode(config.nodePath);
  if (!sourceNode) {
    throw new Error(config.nodePath
      ? `The configured "nodePath" is not a file: ${config.nodePath}.`
      : 'No node.exe was found to run the local app-server stub. Set "nodePath" in config.json or install Node.js.');
  }

  const stampPath = path.join(stageDir, STAMP_NAME);
  let stamp;
  try { stamp = JSON.parse(readFileSync(stampPath, 'utf8')); } catch { stamp = undefined; }
  const exePath = path.join(stageDir, HELPER_NAME);
  const launcherPath = path.join(stageDir, LAUNCHER_NAME);
  const stubPath = path.join(stageDir, STUB_NAME);
  const helperStat = statSync(sourceHelper);
  const nodeStat = statSync(sourceNode);
  const expected = {
    helper: sourceHelper,
    helperSize: helperStat.size,
    helperMtimeMs: helperStat.mtimeMs,
    node: sourceNode,
    nodeSize: nodeStat.size,
    nodeMtimeMs: nodeStat.mtimeMs,
    stubVersion: STUB_VERSION,
  };
  const stampOk = stamp
    && stamp.helper === expected.helper
    && stamp.helperSize === expected.helperSize
    && stamp.helperMtimeMs === expected.helperMtimeMs
    && stamp.node === expected.node
    && stamp.nodeSize === expected.nodeSize
    && stamp.nodeMtimeMs === expected.nodeMtimeMs
    && stamp.stubVersion === STUB_VERSION
    && existsSync(exePath)
    && existsSync(launcherPath)
    && existsSync(stubPath);
  if (!stampOk) {
    copyInto(sourceHelper, stageDir, HELPER_NAME);
    copyInto(sourceNode, stageDir, LAUNCHER_NAME);
    writeFileSync(stubPath, stubSource(), 'utf8');
    writeFileSync(stampPath, JSON.stringify({ ...expected, stagedAt: new Date().toISOString() }, null, 2), 'utf8');
  }

  if (lowLabelPresent(exePath) || lowLabelPresent(stageDir)) {
    throw new Error(`The staging directory "${stageDir}" inherits a Low Mandatory Label; a Low-integrity helper cannot operate the desktop. `
      + 'Point "stageDir" in config.json at a clean Medium-integrity directory under %LOCALAPPDATA%.');
  }

  const isZh = locale === 'zh' || (typeof locale === 'string' && locale.startsWith('zh'));
  const pillPath = path.join(codexHome, 'computer-use', 'config.json');
  const wantPill = JSON.stringify({
    accentColor: '#339cff',
    direction: 'ltr',
    locale: typeof locale === 'string' ? locale : 'zh',
    strings: {
      usingComputer: isZh ? 'Antigravity 正在使用你的电脑' : 'Antigravity is using your computer',
      escToCancel: isZh ? '按 Esc 取消' : 'Esc to cancel',
    },
  });
  try {
    if (readFileSync(pillPath, 'utf8') !== wantPill) writeFileSync(pillPath, wantPill, 'utf8');
  } catch {
    try { writeFileSync(pillPath, wantPill, 'utf8'); } catch {}
  }

  return { exePath, launcherPath, stageDir, codexHome, sourceHelper };
}
