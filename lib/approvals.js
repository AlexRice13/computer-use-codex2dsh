// Approval bridge: maps the helper's approvalRequest onto the Harness approval
// service. Decisions are remembered per agent session; the plugin config can
// pre-approve apps. Fail-closed: no agent, no approval service, or any
// non-allowed outcome denies the app. The Harness itself can never be
// approved, even via autoApprove (self-control guard).

const SELF_EXACT = new Set(['com.deepseek.dsh', 'deepseek harness', 'deepseek harness.exe']);

/**
 * Every identifier an approval request can be matched by. The helper reports
 * unstable app identities across RPCs (bare name, `process:<path>`, AUMID,
 * localized display names), so a config entry matches any of these forms.
 */
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

/** True when a target or approval identity points at the Harness itself. */
export function isSelfTarget(...values) {
  return values.some((value) => approvalKeys(value, undefined).some(
    (key) => SELF_EXACT.has(key) || key.endsWith('\\deepseek harness.exe') || key.endsWith('/deepseek harness.exe'),
  ));
}

export function createApprovalBridge(ctx, config) {
  const grants = new Map(); // agent key -> Set of lowercased app keys
  const autoApproved = new Set((Array.isArray(config?.autoApprove) ? config.autoApprove : [])
    .filter((x) => typeof x === 'string')
    .map((x) => x.trim().toLowerCase())
    .filter((x) => x !== ''));

  return async function decide(approval, info = {}) {
    const keys = approvalKeys(approval.app, approval.displayName);
    if (keys.some((key) => SELF_EXACT.has(key) || key.endsWith('deepseek harness.exe'))) {
      ctx.logger?.warn?.('computer-use: refusing to control the Harness itself', approval.app);
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
