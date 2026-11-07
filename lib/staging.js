// 暂存：把助手和 app-server 桩的启动器复制到 %LOCALAPPDATA% 下一个私有的
// Medium 完整性目录，避免从 ChatGPT/Codex 自己的目录里运行。助手的完整性级别
// 决定它能看见哪些窗口（见 SKY_WINDOWS_PROTOCOL_SPEC §C.1）。
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import os from 'node:os';

import { STUB_VERSION, stubSource } from './stub-app-server.js';

const HELPER_NAME = 'codex-computer-use.exe';
const LAUNCHER_NAME = 'codex.exe';
const STUB_NAME = 'app-server';
const STAMP_NAME = '.dsh-stage.json';

function defaultRoot() {
  const base = process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
  return path.join(base, 'dsh-computer-use');
}

function defaultLocalAppData() {
  return process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
}

function discoverHelper() {
  const runtimes = path.join(defaultLocalAppData(), 'OpenAI', 'Codex', 'runtimes', 'cua_node');
  if (!existsSync(runtimes)) return undefined;
  let best;
  let bestMtime = 0;
  for (const dir of safeReaddir(runtimes)) {
    const candidate = path.join(runtimes, dir, 'bin', 'node_modules', '@oai', 'sky', 'bin', 'windows', HELPER_NAME);
    try {
      const info = statSync(candidate);
      if (info.isFile() && info.size > 0 && info.mtimeMs > bestMtime) {
        best = candidate;
        bestMtime = info.mtimeMs;
      }
    } catch {}
  }
  return best;
}

function discoverNode(configured) {
  // 用户显式配了路径就只认这个路径。写错要当场报错，不能悄悄退回 PATH 里
  // 随便找到的 node.exe。
  if (typeof configured === 'string' && configured.trim() !== '') {
    const candidate = configured.trim();
    try {
      if (statSync(candidate).isFile()) return candidate;
    } catch {}
    return undefined;
  }
  const candidates = [];
  if (process.env.DSH_CUA_NODE_PATH) candidates.push(process.env.DSH_CUA_NODE_PATH);
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

function safeReaddir(dir) {
  try { return readdirSync(dir); } catch { return []; }
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

/**
 * 把助手、复制过去的 node 启动器和 app-server 桩准备好，返回各自的路径。
 * 引擎或 node 找不到时抛出带处理办法的错误。
 */
export function ensureStaged(config, locale = 'en') {
  const root = defaultRoot();
  const stageDir = config.stageDir ? path.resolve(config.stageDir) : path.join(root, 'bin');
  const codexHome = config.codexHome ? path.resolve(config.codexHome) : path.join(root, 'codex-home');
  mkdirSync(stageDir, { recursive: true });
  mkdirSync(path.join(codexHome, 'computer-use'), { recursive: true });

  const sourceHelper = config.helperPath ? path.resolve(config.helperPath) : discoverHelper();
  if (!sourceHelper || !existsSync(sourceHelper)) {
    if (config.helperPath) {
      throw new Error(`The configured "helperPath" does not exist: ${path.resolve(config.helperPath)}. `
        + 'Point it at the built engine (engine/target/release/sky-engine.exe) or at an existing codex-computer-use.exe.');
    }
    throw new Error('No helper executable was found: set the plugin config "helperPath" to the built engine '
      + '(engine/target/release/sky-engine.exe) or to an existing codex-computer-use.exe. Without it the plugin only '
      + 'auto-discovers %LOCALAPPDATA%\\OpenAI\\Codex\\runtimes\\cua_node, which requires the ChatGPT/Codex desktop app.');
  }
  const sourceNode = discoverNode(config.nodePath);
  if (!sourceNode) {
    throw new Error(config.nodePath
      ? `The configured "nodePath" is not a file: ${config.nodePath}.`
      : 'No node.exe was found to run the local app-server stub. Set the plugin config "nodePath" to a node.exe, or install Node.js.');
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
    // 只看大小会漏掉“长度没变的重新编译”，所以再比一次 mtime。
    // copyFileSync 会保留 mtime，上面的 sameFile 判断才成立。
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

  // 完整性检查：Low 标签的助手几乎看不到窗口，也没法往 Medium 窗口发输入。
  // 这种情况直接报错，别让它带病运行。
  if (lowLabelPresent(exePath) || lowLabelPresent(stageDir)) {
    throw new Error(`The staging directory "${stageDir}" inherits a Low Mandatory Label; a Low-integrity helper cannot operate the desktop. `
      + 'Point the plugin config "stageDir" at a clean directory (for example another folder under %LOCALAPPDATA%).');
  }

  // 状态胶囊用中性文案，不用 ChatGPT 写死的那套。
  const pillPath = path.join(codexHome, 'computer-use', 'config.json');
  const wantPill = JSON.stringify({
    accentColor: '#339cff',
    direction: 'ltr',
    locale: typeof locale === 'string' ? locale : 'en',
    strings: {
      usingComputer: locale === 'zh' || locale?.startsWith('zh') ? 'DSH 正在使用你的电脑' : 'DSH is using your computer',
      escToCancel: locale === 'zh' || locale?.startsWith('zh') ? '按 Esc 取消' : 'Esc to cancel',
    },
  });
  try {
    if (readFileSync(pillPath, 'utf8') !== wantPill) writeFileSync(pillPath, wantPill, 'utf8');
  } catch {
    try { writeFileSync(pillPath, wantPill, 'utf8'); } catch {}
  }

  return { exePath, launcherPath, stageDir, codexHome };
}
