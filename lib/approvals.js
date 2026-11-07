// 审批桥：把助手发来的 approvalRequest 接到 Harness 的审批服务上。
// 同一个 agent 会话里批过的应用会记住；插件配置也可以提前放行一批应用。
// 默认拒绝：没有 agent、没有审批服务，或者用户没点允许，都算拒绝。
// Harness 自己永远不能出现在审批目标里，autoApprove 也不行（防止模型操作自己）。

const SELF_EXACT = new Set(['com.deepseek.dsh', 'deepseek harness', 'deepseek harness.exe']);

/**
 * 一条审批请求可能带的全部身份写法。助手在不同 RPC 里报的 app 身份不稳定
 * （裸名字、process:<path>、AUMID、本地化显示名），所以配置里写哪一种都能对上。
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

/** 目标或审批身份指向 Harness 自己时返回 true。 */
export function isSelfTarget(...values) {
  return values.some((value) => approvalKeys(value, undefined).some(
    (key) => SELF_EXACT.has(key) || key.endsWith('\\deepseek harness.exe') || key.endsWith('/deepseek harness.exe'),
  ));
}

export function createApprovalBridge(ctx, config) {
  const grants = new Map(); // agent key -> 小写 app key 的集合
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
