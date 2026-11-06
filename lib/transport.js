// Line-delimited JSON-RPC stdio transport for codex-computer-use.exe.
// Faithful reimplementation of the observed WindowsHelperTransport contract
// (SKY_WINDOWS_PROTOCOL_SPEC §A, §B.1, §E, §I): frames are
// `{id,method,params,meta}\n`, replies carry `ok`/`result`/`approvalRequest`/
// `error`, one malformed line aborts every in-flight request, exit code 130 is
// the physical-Escape signal, and turn transitions auto-send `end_turn`.
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

  /** Serialize every call: the helper is single-threaded on one stdin. */
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
      // The interruption no longer holds: the turn advanced (consume the durable
      // marker) or the marker was cleared elsewhere. Release the memory block.
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

    // Automatic end_turn for the previous turn on a turn transition.
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
    // Remove the host-exit hook exactly once, on either terminal event: a
    // failed spawn ('error') never reaches 'exit', and leaking one
    // process-level listener per failed attempt trips MaxListenersExceeded.
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
          // The stream is desynchronized; keep the tainted child alive would hang
          // every later request on a never-matching id. Kill and respawn next call.
          this.#rejectAll(new Error(`computer-use helper emitted invalid JSON: ${line}`));
          this.#cleanup(child);
          try { child.kill(); } catch {}
          return;
        }
        this.#dispatch(message);
      }
    });
    // A pipe to a helper that has already died raises EPIPE (and friends) on
    // the stdin/stdout/stderr streams. Without a listener Node turns that into
    // an uncaught exception that would take the whole Host down, so every
    // stream gets one: pending requests fail with the pipe error and the exit
    // handler performs the normal cleanup.
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
      // A stale child (already replaced by a respawn) must not poison the new
      // child's in-flight requests; its own pending set was rejected at kill time.
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
    if (typeof message.id !== 'number') return; // notifications are ignored
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
        // A destroyed stream throws synchronously instead of reporting through
        // the callback; surface it as this request's failure, not a crash.
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

  /** Normal teardown: request `close`, fall back to kill. Does not send end_turn. */
  async close() {
    const child = this.#child;
    if (child === null || child.exitCode !== null || child.signalCode !== null) {
      this.#cleanup(child);
      return;
    }
    // `close` is serialized behind whatever is in flight. Bound the whole
    // graceful phase, not just the frame's own timer, so unloading the plugin
    // cannot stall on a wedged request; kill whatever is left afterwards.
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

  /** Hard teardown for plugin unload. */
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
