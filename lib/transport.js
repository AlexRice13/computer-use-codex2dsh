// 和 codex-computer-use.exe 通信的 stdio 传输层：一行一个 JSON-RPC。
// 按观察到的 WindowsHelperTransport 契约实现（SKY_WINDOWS_PROTOCOL_SPEC
// §A、§B.1、§E、§I）：帧是 `{id,method,params,meta}\n`，回复带 `ok`/`result`/
// `approvalRequest`/`error`；有一行解析失败就废掉所有在途请求；退出码 130
// 表示用户按了物理 Esc；回合切换时自动补发 `end_turn`。
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, unlinkSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { approvalKeys, isSelfTarget } from './approvals.js';

export const ESCAPE_MSG = 'Computer Use was stopped by the user with the physical Escape key. '
  + 'Stop your work, do not call further Computer Use tools in this turn, and send a final message noting that the user stopped Computer Use.';

export const APPROVED_APP_META_KEY = 'x-oai-cua-approved-app';
export const BROWSER_POLICY_BYPASS_KEY = 'x-codex-browser-use-security-mode';
export const BROWSER_POLICY_BYPASS_VALUE = 'disabled-for-local-testing';

const DEFAULT_TIMEOUT_MS = 10000;
const LAUNCH_TIMEOUT_MS = 15000;
const CLOSE_TIMEOUT_MS = 10000;
const STDERR_TAIL = 8000;

const ENV_STRIP = [
  'NODE_OPTIONS',
  'ELECTRON_RUN_AS_NODE',
  'SKY_CUA_NATIVE_PIPE',
  'SKY_CUA_NATIVE_PIPE_DIRECTORY',
  'SKY_ENABLE_AUDIO',
  'OAI_SKY_CONFIG_PATH',
  'NODE_REPL_REQUEST_META',
];

const sanitize = (value) => String(value).replace(/[^A-Za-z0-9._-]/g, '_');

function parseApprovalRequest(value) {
  if (value === null || typeof value !== 'object') return undefined;
  const record = value;
  if (typeof record.app !== 'string' || record.app.trim() === '') return undefined;
  const displayName = typeof record.displayName === 'string' && record.displayName.trim() !== ''
    ? record.displayName.trim()
    : record.app.trim();
  const riskLevel = record.riskLevel === 'high' || record.riskLevel === 'low' ? record.riskLevel : undefined;
  return {
    app: record.app.trim(),
    displayName,
    riskLevel,
    allowPersistentApproval: record.allowPersistentApproval !== false,
  };
}

export class SkyTransport {
  #options;
  #child = null;
  #stderr = '';
  #nextId = 1;
  #pending = new Map();
  #queue = Promise.resolve();
  #exitListeners = new Set();
  #currentTurnKey = null;
  #currentTurnMeta = null;
  #lastTurnScope = null;
  #interruptedTurn = null;

  /**
   * @param options - { exePath, launcherPath, stageDir, codexHome,
   *   timeoutMs?, launchTimeoutMs?, enforceBrowserUrlPolicy?, onApproval }
   */
  constructor(options) {
    this.#options = options;
  }

  get alive() {
    return this.#child !== null && this.#child.exitCode === null && this.#child.signalCode === null;
  }

  onExit(listener) {
    this.#exitListeners.add(listener);
    return () => this.#exitListeners.delete(listener);
  }

/** 所有请求排队走：助手只有一个 stdin，同一时间只能处理一条。 */
  request(method, params, context = {}) {
    const operation = this.#queue.then(() => this.#request(method, params, context));
    this.#queue = operation.then(() => {}, () => {});
    return operation;
  }

  async #request(method, params, context) {
    const {
      sessionId, turnId, meta = {}, timeoutMs, onApproval,
      agent, callId, toolName, signal,
    } = context;
    const effectiveTimeout = timeoutMs ?? (method === 'launch_app'
      ? this.#options.launchTimeoutMs ?? LAUNCH_TIMEOUT_MS
      : this.#options.timeoutMs ?? DEFAULT_TIMEOUT_MS);

    const sessionText = typeof sessionId === 'string' && sessionId.trim() !== '' ? sessionId.trim() : undefined;
    const turnText = typeof turnId === 'string' && turnId.trim() !== '' ? turnId.trim() : undefined;
    const turnScope = sessionText !== undefined && turnText !== undefined
      ? { conversationId: sessionText, turnId: turnText }
      : undefined;
    const turnKey = turnScope === undefined
      ? null
      : `${this.#options.codexHome}\0${turnScope.conversationId}\0${turnScope.turnId}`;

    const markerPath = turnScope === undefined
      ? undefined
      : path.join(this.#options.codexHome, 'cache', 'computer-use', 'interrupts',
          sanitize(turnScope.conversationId), sanitize(turnScope.turnId));

    if (markerPath !== undefined && existsSync(markerPath)) {
      this.#interruptedTurn = turnKey;
      throw new Error(ESCAPE_MSG);
    }
    if (this.#interruptedTurn !== null) {
      if (turnKey === null) throw new Error(ESCAPE_MSG);
      if (turnKey === this.#interruptedTurn && markerPath !== undefined && existsSync(markerPath)) {
        throw new Error(ESCAPE_MSG);
      }
      // 中断状态已经过去：要么回合往前走了（把落盘标记读掉），要么标记被
      // 别处清了。可以放开内存里的封锁。
      const staleKey = this.#interruptedTurn;
      this.#interruptedTurn = null;
      try {
        const [home, conv, turn] = String(staleKey).split('\0');
        if (home === this.#options.codexHome && conv !== undefined && turn !== undefined) {
          const stale = path.join(home, 'cache', 'computer-use', 'interrupts', sanitize(conv), sanitize(turn));
          if (existsSync(stale)) unlinkSync(stale);
        }
      } catch {}
    }

    if (!this.alive) this.#spawn();
    const turnMetadata = turnScope === undefined
      ? undefined
      : { session_id: turnScope.conversationId, turn_id: turnScope.turnId };

    // 回合切换时，替上一个回合自动补一发 end_turn。
    if (turnKey !== null && turnKey !== this.#currentTurnKey) {
      const previousKey = this.#currentTurnKey;
      const previousMeta = this.#currentTurnMeta;
      this.#currentTurnKey = turnKey;
      this.#currentTurnMeta = turnMetadata;
      this.#lastTurnScope = turnScope;
      if (previousKey !== null && this.alive) {
        await this.#send('end_turn', {}, previousMeta ?? {}, CLOSE_TIMEOUT_MS).catch(() => {});
      }
    }

    const baseMeta = { ...meta };
    if (turnMetadata !== undefined) Object.assign(baseMeta, turnMetadata);
    if (this.#options.enforceBrowserUrlPolicy !== true) {
      baseMeta[BROWSER_POLICY_BYPASS_KEY] ??= BROWSER_POLICY_BYPASS_VALUE;
    }

    let approvedApp;
    for (;;) {
      const frameMeta = { ...baseMeta, 'x-oai-cua-request-budget-ms': Math.max(1000, effectiveTimeout - 1000) };
      if (approvedApp !== undefined) frameMeta[APPROVED_APP_META_KEY] = approvedApp;
      const reply = await this.#send(method, params, frameMeta, effectiveTimeout, signal);
      if (reply.kind === 'result') return reply.result;
      const approval = reply.approvalRequest;
      const handler = onApproval ?? this.#options.onApproval;
      if (handler === undefined) {
        throw new Error(`Computer Use requires app approval but no approver is mounted (app "${approval.app}")`);
      }
      const approved = await handler(approval, { agent, callId, toolName, signal });
      if (approved !== true) {
        if (isSelfTarget(approval.app, approval.displayName)) {
          throw new Error('Computer Use cannot control the DeepSeek Harness itself; target another app window.');
        }
        const keys = approvalKeys(approval.app, approval.displayName);
        throw new Error(`Computer Use was not approved to use ${approval.displayName}. `
          + `To allow it without prompting, add one of [${keys.join(', ')}] to the plugin config "autoApprove".`);
      }
      approvedApp = approval.app;
    }
  }

  #spawn() {
    const { exePath, launcherPath, stageDir, codexHome } = this.#options;
    const env = { ...process.env };
    for (const key of ENV_STRIP) delete env[key];
    env.CODEX_HOME = codexHome;
    env.CODEX_CLI_PATH = launcherPath;
    const child = spawn(exePath, ['--parent-pid', String(process.pid)], {
      cwd: stageDir,
      env,
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    });
    child.unref();
    for (const stream of [child.stdin, child.stdout, child.stderr]) {
      if (stream !== null && stream !== undefined && typeof stream.unref === 'function') stream.unref();
    }
    const onHostExit = () => {
      try {
        if (child.exitCode === null && child.signalCode === null) child.kill();
      } catch {}
    };
    process.once('exit', onHostExit);
    // host 退出钩子只能摘一次，'error' 和 'exit' 谁先来算谁。spawn 失败
    // 不会走到 'exit'；每次失败都漏一个 process 级监听，迟早触发
    // MaxListenersExceeded。
    const detachHostExit = () => process.off('exit', onHostExit);
    let buffer = '';
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      buffer += chunk;
      let index;
      while ((index = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, index).trim();
        buffer = buffer.slice(index + 1);
        if (line === '') continue;
        let message;
        try {
          message = JSON.parse(line);
        } catch {
          // 流已经错位了：再留着这个子进程，后面的请求会等一个永远不来的 id。
          // 直接杀掉，下次调用重新拉起。
          this.#rejectAll(new Error(`computer-use helper emitted invalid JSON: ${line}`));
          this.#cleanup(child);
          try { child.kill(); } catch {}
          return;
        }
        this.#dispatch(message);
      }
    });
    // 助手已经死了还往它的 stdin/stdout/stderr 写，会报 EPIPE 之类的错。
    // 没人接的话 Node 会抛成未捕获异常，整个 Host 跟着挂。所以三条流都挂
    // 错误处理：挂起的请求带着管道错误失败，退出处理照常收尾。
    const onPipeError = (streamName) => (error) => {
      if (this.#child !== child) return;
      this.#rejectAll(new Error(`computer-use helper ${streamName} pipe failed: ${error?.message ?? error}`));
      try { child.kill(); } catch {}
    };
    child.stdin.on('error', onPipeError('stdin'));
    child.stdout.on('error', onPipeError('stdout'));
    child.stderr.on('error', () => {});
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => {
      if (this.#child === child) this.#stderr = (this.#stderr + chunk).slice(-STDERR_TAIL);
    });
    child.on('error', (error) => {
      detachHostExit();
      this.#rejectAll(new Error(`computer-use helper failed to start: ${error.message}`));
      this.#cleanup(child);
    });
    child.on('exit', (code, signal) => {
      detachHostExit();
      // 已经被替换掉的旧子进程不能影响新子进程的在途请求；它自己的
      // pending 在 kill 时就清掉了。
      if (this.#child !== child) return;
      const reason = signal === null || signal === undefined ? `exit code ${code ?? 'unknown'}` : `signal ${signal}`;
      const tail = this.#stderr.trim();
      let error;
      if (code === 130) {
        this.#markInterrupted();
        error = new Error(ESCAPE_MSG);
      } else {
        error = new Error(tail === ''
          ? `computer-use helper exited with ${reason}`
          : `computer-use helper exited with ${reason}: ${tail}`);
      }
      this.#cleanup(child);
      this.#rejectAll(error);
      for (const listener of [...this.#exitListeners]) {
        try { listener(); } catch {}
      }
    });
    this.#child = child;
    this.#stderr = '';
    return child;
  }

  #dispatch(message) {
    if (message === null || typeof message !== 'object') return;
    if (typeof message.id !== 'number') return; // 通知类消息不回复
    const pending = this.#pending.get(message.id);
    if (pending === undefined) return;
    clearTimeout(pending.timer);
    this.#pending.delete(message.id);
    if (message.ok === true) {
      pending.resolve({ kind: 'result', result: message.result });
      return;
    }
    const approval = parseApprovalRequest(message.approvalRequest);
    if (approval !== undefined) {
      pending.resolve({ kind: 'approval', approvalRequest: approval });
      return;
    }
    pending.reject(new Error(typeof message.error === 'string' ? message.error : 'computer-use helper request failed'));
  }

  #send(method, params, meta, timeoutMs, signal) {
    const child = this.#child;
    if (child === null || child.exitCode !== null || child.signalCode !== null) {
      return Promise.reject(new Error('computer-use helper is not running'));
    }
    return new Promise((resolve, reject) => {
      const id = this.#nextId++;
      const frame = `${JSON.stringify({ id, method, params, ...(meta === undefined || Object.keys(meta).length === 0 ? {} : { meta }) })}\n`;
      const settle = () => {
        clearTimeout(timer);
        if (signal !== undefined) signal.removeEventListener('abort', onAbort);
        this.#pending.delete(id);
      };
      const timer = setTimeout(() => {
        settle();
        try { child.kill(); } catch {}
        reject(new Error(`computer-use request timed out: ${method}`));
      }, timeoutMs);
      const onAbort = () => {
        settle();
        reject(new Error(`computer-use request cancelled: ${method}`));
      };
      if (signal !== undefined) {
        if (signal.aborted) { clearTimeout(timer); reject(new Error(`computer-use request cancelled: ${method}`)); return; }
        signal.addEventListener('abort', onAbort, { once: true });
      }
      this.#pending.set(id, {
        resolve: (value) => { settle(); resolve(value); },
        reject: (error) => { settle(); reject(error); },
        timer,
      });
      try {
        child.stdin.write(frame, 'utf8', (error) => {
          if (error !== null && error !== undefined) {
            settle();
            reject(error);
          }
        });
      } catch (error) {
        // 流已经销毁时，写操作会同步抛错，而不是走回调。把它算成这次请求
        // 的失败，别让它冒成崩溃。
        settle();
        reject(new Error(`computer-use helper stdin is not writable: ${error?.message ?? error}`));
      }
    });
  }

  #markInterrupted() {
    const scope = this.#lastTurnScope;
    if (scope === null || scope === undefined) return;
    this.#interruptedTurn = `${this.#options.codexHome}\0${scope.conversationId}\0${scope.turnId}`;
    try {
      const marker = path.join(this.#options.codexHome, 'cache', 'computer-use', 'interrupts',
        sanitize(scope.conversationId), sanitize(scope.turnId));
      mkdirSync(path.dirname(marker), { recursive: true });
      writeFileSync(marker, '', 'utf8');
    } catch {}
  }

  #rejectAll(error) {
    for (const [, pending] of this.#pending) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.#pending.clear();
  }

  #cleanup(child) {
    if (this.#child === child) this.#child = null;
  }

/** 正常收尾：先发 `close`，不行再 kill。不发 end_turn。 */
  async close() {
    const child = this.#child;
    if (child === null || child.exitCode !== null || child.signalCode !== null) {
      this.#cleanup(child);
      return;
    }
    // `close` 会排在在途请求后面。超时要盖住整个优雅关闭阶段，不能只算
    // 这一帧自己的定时器，否则卸载插件会被一个卡死的请求拖住；剩下的直接 kill。
    const graceful = this.request('close', {}, { timeoutMs: CLOSE_TIMEOUT_MS }).catch(() => {});
    let deadline;
    await Promise.race([
      graceful,
      new Promise((resolve) => {
        deadline = setTimeout(resolve, CLOSE_TIMEOUT_MS);
        deadline.unref?.();
      }),
    ]);
    clearTimeout(deadline);
    if (child.exitCode === null && child.signalCode === null) {
      try { child.kill(); } catch {}
    }
    this.#cleanup(child);
  }

/** 卸载插件时的硬收尾。 */
  kill() {
    const child = this.#child;
    if (child !== null) {
      try {
        if (child.exitCode === null && child.signalCode === null) child.kill();
      } catch {}
      this.#cleanup(child);
    }
    this.#rejectAll(new Error('computer-use helper was shut down by the harness'));
  }
}
