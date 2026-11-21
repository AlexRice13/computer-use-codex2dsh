// Approval bridge: protects Antigravity itself, remembers per-session grants,
// uses MCP elicitation when supported by the client, and falls back to a native
// Win32 TopMost confirmation dialog on Windows when the client lacks elicitation.
import { spawn } from 'node:child_process';

const SELF_EXACT = new Set([
  'antigravity',
  'antigravity.exe',
  'com.google.antigravity',
  'com.deepseek.dsh',
  'deepseek harness',
  'deepseek harness.exe',
]);

export function approvalKeys(app, displayName) {
  const keys = new Set();
  const add = (value) => {
    if (typeof value !== 'string') return;
    const key = value.trim().toLowerCase();
    if (key !== '') keys.add(key);
  };
  add(app);
  add(displayName);
  for (const raw of [app, displayName]) {
    if (typeof raw !== 'string') continue;
    const value = raw.trim();
    const proc = /^process:(.+)$/i.exec(value)?.[1];
    if (proc !== undefined) {
      const exe = (proc.split(/[\\/]/).pop() ?? '').trim();
      add(exe);
      add(exe.replace(/\.exe$/i, ''));
    }
    const aumid = /^([A-Za-z][A-Za-z0-9.]*)[_!]/.exec(value)?.[1];
    if (aumid !== undefined && aumid.length > 2) add(aumid);
  }
  return [...keys];
}

export function isSelfTarget(...values) {
  return values.some((value) => approvalKeys(value, undefined).some(
    (key) => SELF_EXACT.has(key)
      || key.endsWith('\\antigravity.exe')
      || key.endsWith('/antigravity.exe')
      || key.endsWith('\\deepseek harness.exe')
      || key.endsWith('/deepseek harness.exe'),
  ));
}

const WIN32_DIALOG_SCRIPT = [
  '$sh = New-Object -ComObject WScript.Shell',
  '$res = $sh.Popup($env:CU_APPROVAL_BODY, 0, $env:CU_APPROVAL_TITLE, 266276)',
  'if ($res -eq 6) { [Console]::Out.Write("ALLOW") } else { [Console]::Out.Write("DENY") }',
].join('; ');
const WIN32_DIALOG_ENCODED = Buffer.from(WIN32_DIALOG_SCRIPT, 'utf16le').toString('base64');

export function promptWin32Approval({ displayName, app, riskLevel, locale = 'zh', signal }) {
  if (process.platform !== 'win32') return Promise.resolve('denied');
  const zh = locale === 'zh' || (typeof locale === 'string' && locale.startsWith('zh'));
  const risk = riskLevel ?? 'unspecified';
  const title = zh ? 'Antigravity 电脑操作授权' : 'Antigravity Computer Use Approval';
  const body = zh
    ? `允许 Computer Use 控制 “${displayName}” 吗？\n\n标识：${app}\n风险等级：${risk}\n\n同意后，本会话内可读取该应用的窗口状态并向其发送鼠标/键盘输入。`
    : `Allow Computer Use to control "${displayName}"?\n\nApp: ${app}\nRisk level: ${risk}\n\nIf allowed, Computer Use can read this app's window state and send input to it for this session.`;

  return new Promise((resolve) => {
    if (signal?.aborted) {
      resolve('denied');
      return;
    }
    const child = spawn('powershell.exe', [
      '-NoProfile',
      '-NonInteractive',
      '-WindowStyle',
      'Hidden',
      '-EncodedCommand',
      WIN32_DIALOG_ENCODED,
    ], {
      env: { ...process.env, CU_APPROVAL_TITLE: title, CU_APPROVAL_BODY: body },
      stdio: ['ignore', 'pipe', 'ignore'],
      windowsHide: true,
    });
    let out = '';
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk) => { out += chunk; });
    const onAbort = () => {
      try { child.kill(); } catch {}
      resolve('denied');
    };
    if (signal) signal.addEventListener('abort', onAbort, { once: true });
    const done = (result) => {
      if (signal) signal.removeEventListener('abort', onAbort);
      resolve(result);
    };
    child.on('error', () => done('denied'));
    child.on('close', (code) => {
      done(code === 0 && out.trim() === 'ALLOW' ? 'allowed-once' : 'denied');
    });
  });
}

export function createApprovalBridge(ctx, config) {
  const grants = new Map();
  const autoApproved = new Set((Array.isArray(config?.autoApprove) ? config.autoApprove : [])
    .filter((x) => typeof x === 'string')
    .map((x) => x.trim().toLowerCase())
    .filter((x) => x !== ''));

  return async function decide(approval, info = {}) {
    const keys = approvalKeys(approval.app, approval.displayName);
    if (isSelfTarget(approval.app, approval.displayName)) {
      ctx.logger?.warn?.('computer-use: refusing to control the host IDE itself', approval.app);
      return false;
    }
    if (keys.some((key) => autoApproved.has(key))) return true;
    const agentKey = info.agent?.id === undefined ? 'default' : String(info.agent.id);
    let remembered = grants.get(agentKey);
    if (remembered === undefined) {
      remembered = new Set();
      grants.set(agentKey, remembered);
    }
    if (keys.some((key) => remembered.has(key))) return true;

    const approvalService = ctx.get === undefined ? undefined : ctx.get('approval');
    if (approvalService === undefined || info.agent === undefined) {
      ctx.logger?.warn?.('computer-use: no approval service or agent for app', approval.app);
      return false;
    }
    const risk = approval.riskLevel ?? 'unspecified';
    const outcome = await approvalService.request({
      agent: info.agent,
      toolName: info.toolName ?? 'computer-use',
      callId: info.callId,
      signal: info.signal,
      app: approval.app,
      displayName: approval.displayName,
      riskLevel: risk,
      reason: `Computer Use needs access to "${approval.displayName}" (risk: ${risk}). `
        + 'The helper will read this app\'s window state and send input to it.',
      displayReason: {
        en: `Computer Use wants to control "${approval.displayName}" (risk: ${risk}).`,
        zh: `电脑操作请求控制 “${approval.displayName}”（风险等级：${risk}），将读取其窗口状态并发送输入。`,
      },
    });
    if (outcome === 'allowed-once') {
      for (const key of keys) remembered.add(key);
      return true;
    }
    return false;
  };
}
