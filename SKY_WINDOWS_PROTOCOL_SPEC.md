# Codex Computer Use (Sky / CUA) — Windows helper protocol spec

Target: `@oai/sky` 0.7.13, `codex-computer-use.exe` (Rust, 1,551,152 B), reverse-engineered for a from-scratch TypeScript **client** in a plain Node.js host process (no Electron, no `nodeRepl`).

Sources: beautified copies of the shipped ESM bundles under `_pretty/`, `.d.ts` under `src/types/**`, `docs/skills/oai_sky_lib/windows/SKILL.md`, and ASCII+UTF-16LE string extraction of the helper exe (`_strings_small.txt`; file offsets given as evidence anchors). Anything not directly backed by JS/exe bytes is marked **[inference]**.

---

## 0. Which code path actually runs

`src/targets/windows/create_client.js` is:

```js
function n(n){return new WindowsComputerUseClient}   // options argument is DROPPED
```

`WindowsComputerUseClient` constructor (`_pretty/targets/windows/internal/computer_use_client.js.pretty.js:21-51`) picks transport in this order:

1. `options.transport` (never supplied on Windows) →
2. `globalThis.nodeRepl?.env?.SKY_CUA_NATIVE_PIPE === "1"` → native pipe (Electron-only; note it reads `nodeRepl.env`, **not** `process.env`) →
3. shared `WindowsHelperTransport` (the stdio path).

**Conclusion: a plain Node process always lands on the stdio `WindowsHelperTransport` path.** Every `globalThis.nodeRepl` read in this code returns `undefined` outside the Codex Electron REPL, and every such site is guarded/optional.

---

## A. TRANSPORT — stdio (the path you must implement)

### A.1 Binary resolution

`_pretty/core/package_bin.js` + `computer_use_client.js:K()`:

```
OAI_SKY_WINDOWS_BIN        (env override, trimmed, wins if non-empty)
else <packageDir>/bin/windows/
     process.arch === "arm64" ? "codex-computer-use-arm64.exe" : "codex-computer-use.exe"
```

`packageDir` = nearest ancestor `package.json` dir (walk up from the module); failure → `` throw new Error(`find_package_dir start_dir=${n}`) ``; missing file → `` throw new Error(`${pkgName} bin=${path}`) ``.

On-disk facts on this machine:

| Path | Bytes | Notes |
|---|---|---|
| `bin\windows\codex-computer-use.exe` | 1,551,152 | **the operative helper** (Rust msvc, `rustc 8bab26f4f68e0e26f0bb7960be334d5b520ea452`, `windows-0.61.3`, `serde_json-1.0.149`) |
| `bin\windows\codex-computer-use-arm64.exe` | — | **not present**; x64 build only |
| `bin\windows\swift\x64\codex-computer-use-swift.exe` | 56,534,320 | Swift/WinRT build; `manifest.json` says 56,518,656 (stale). Never referenced by this JS version **[inference: unused]**. Only strings: `Usage: codex-computer-use [--parent-pid <pid>]`, `--parent-pid requires a process ID`, `x-oai-cua-approved-app`, `x-oai-cua-request-budget-ms` |
| `bin\windows\swift\x64\MSVCP140.dll` / `VCRUNTIME140_1.dll` / `VCRUNTIME140.dll` | 575,592 / 49,744 / 120,432 | shipped with sha256 in manifest |

`package.json` `publishConfig.executableFiles` lists **mac** binaries only (the Windows exe is unpacked by the runtime installer, not npm).

### A.2 argv, cwd, env

`computer_use_client.js:O()`, byte-exact:

```js
const O = (t) => [...t, "--parent-pid", String(process.pid)];
```

Applied **only** when `helperCommand`/`helperPath` were not supplied. Since Windows `create_client` supplies neither, the real argv is:

```
<packageDir>\bin\windows\codex-computer-use.exe --parent-pid <hostPid>
```

`spawn` call (`helper_transport.js:161-166`), byte-exact:

```js
const i = spawn(this.#helperCommand, this.#helperArgs, {
  env: this.#helperEnv == null ? undefined : { ...globalThis.process.env, ...this.#helperEnv },
  stdio: ["pipe","pipe","pipe"],
  windowsHide: true,
});
i.unref();
for (const s of [i.stdin, i.stdout, i.stderr]) if ("unref" in s && typeof s.unref === "function") s.unref();
```

* **No `cwd`** → helper inherits the host's cwd.
* **No `shell`** → direct `CreateProcess`; not affected by `ComSpec`.
* `child.unref()` + stream `unref()` → the helper never keeps the host event loop alive.
* `windowsHide: true` → no console window.
* `helperEnv` is always `undefined` on the Windows path (never passed), so **the helper inherits `process.env` verbatim**.
* `globalThis.process.once("exit", () => { if (child.exitCode == null && child.signalCode == null) child.kill(); })` — SIGKILL-equivalent (`TerminateProcess`) on host exit.

**Env vars the JS reads/needs for the stdio path:** `OAI_SKY_WINDOWS_BIN`, `CODEX_HOME`, `USERPROFILE`, `WSL_DISTRO_NAME` (only if `CODEX_HOME` is a POSIX path), `NODE_REPL_REQUEST_META`, `SKY_ENABLE_AUDIO`, `SKY_AUDIO_MODEL_ALLOWLIST`, `OAI_SKY_CONFIG_PATH`. Everything else (`SKY_CUA_NATIVE_PIPE*`, `ComSpec`) is irrelevant here.

### A.3 Frame format — **line-delimited JSON, not length-prefixed**

Writer (`helper_transport.js:193-216`), byte-exact:

```js
const id = (this.#nextId)++;                       // starts at 1
const meta = { ...(metaObj ?? {}), "x-oai-cua-request-budget-ms": timeoutMs };
const frame = `${JSON.stringify({ id, method, params, ...(meta == null ? {} : { meta }) })}\n`;
child.stdin.write(frame, "utf8", (err) => { if (err != null) { clearTimeout(t); pending.delete(id); reject(err); } });
```

* Encoding: UTF-8, `\n`-terminated, one JSON object per line. **No `jsonrpc` field on stdio** (the native-pipe variant adds `jsonrpc:"2.0"`; stdio does not).
* `meta` is *always* present in practice because the budget key is merged in unconditionally.
* Key order as written: `id`, `method`, `params`, `meta`.
* `x-oai-cua-request-budget-ms` is the per-request timeout in **ms** and is the value the helper uses to self-abort (`computer-use request budget exhausted`, exe `@0x1267984`).

Reader (`helper_transport.js:217-231`) — accumulates a UTF-8 string buffer, splits on the first `\n`, `.trim()`s each line, ignores empty lines, `JSON.parse`s:

```js
catch (e) { rejectAllPending(new Error(`computer-use helper emitted invalid JSON: ${line}`)); }
```

i.e. **one malformed line kills every in-flight request of that child** (it does not skip-and-continue on stdio).

### A.4 Response envelope (exe → client)

Dispatch (`helper_transport.js:232-253`), byte-exact logic:

```js
if (typeof obj.id !== "number") { for (const l of this.#eventListeners) l(obj); return; }   // notification
const p = this.#pending.get(obj.id); if (p == null) return;                                  // unknown id -> dropped
clearTimeout(p.timeout); this.#pending.delete(obj.id);
if (obj.ok) return p.resolve({ kind: "result", result: obj.result });
const ar = parseApprovalRequest(obj.approvalRequest);
if (ar != null) return p.resolve({ kind: "approval", approvalRequest: ar });
const msg = typeof obj.error === "string" ? obj.error : "computer-use helper request failed";
p.reject(new Error(msg));
```

So the three legal reply shapes are:

```jsonc
{"id":1,"ok":true,"result":{...}}
{"id":1,"ok":false,"approvalRequest":{"app":"...","displayName":"...","riskLevel":"high","allowPersistentApproval":true}}
{"id":1,"ok":false,"error":"literal human-readable string"}
```

`error` is a **bare string**, not a JSON-RPC error object. `ok` is the success discriminator; there is no `result` on failure and no `error` on success. Correlate on numeric `id`.

Notifications: `id` missing or non-number → fan-out to `onEvent(cb)` subscribers (`transport.onEvent`/`transport.onExit` exist but the Windows client never subscribes to `onEvent`). The exe's app-server handshake declares `optOutNotificationMethods` (`@0x1281224`), i.e. it *can* push notifications; the JS ignores them.

### A.5 stderr

Accumulated only, never streamed (`helper_transport.js:172-174`): `this.#stderr += chunk` (only for the currently-tracked child). Surfaced solely in the exit error:

```js
const reason = signal == null ? `exit code ${exitCode ?? "unknown"}` : `signal ${signal}`;
const tail   = (this.#child === child) ? this.#stderr.trim() : "";
const err = exitCode === 130 ? new Error(ESCAPE_MSG)
        : new Error(tail === "" ? `computer-use helper exited with ${reason}`
                                : `computer-use helper exited with ${reason}: ${tail}`);
```

**Recommendation for your reimplementation:** keep a bounded stderr tail (it is the only crash diagnostic) and do not treat stderr as a protocol channel.

### A.6 Teardown

```js
close() {
  this.#interruptedTurn = null;
  const child = this.#child;
  if (child == null) return Promise.resolve();
  if (child.exitCode != null || child.signalCode != null) { this.#cleanup(child); return Promise.resolve(); }
  return this.request("close", {})                      // normal request, default 10 s timeout
    .catch(() => { child.kill(); })
    .finally(() => { this.#cleanup(child); })
    .then(() => {});
}
```

`end_turn` is **not** sent by `close()`. `#cleanup` clears the child reference, the current-turn key and the cached turn metadata, and both buffers. On exit, all pending requests are rejected and `onExit` listeners run. `spawn` errors reject all pending and drop the child.

### A.7 Named-pipe path (Electron only) — for completeness

Frame **is** length-prefixed here (`computer_use_client.js` `b`/`U`):

```js
// write
const buf = Buffer.from(JSON.stringify(msg), "utf8");
if (buf.length > 8388608) throw new Error(`Computer Use native pipe frame is too large: ${buf.length}`);   // 8 MiB send cap
const out = Buffer.alloc(4 + buf.length); out.writeUInt32LE(buf.length, 0); buf.copy(out, 4); sock.write(out);
// read  → need >=4 bytes; const len = readUInt32LE(0);
if (len > 67108864) throw new Error(`Computer Use native pipe frame is too large: ${len}`);                 // 64 MiB recv cap
```

Asymmetric caps (8 MiB out / 64 MiB in). Envelope `{id, jsonrpc:"2.0", method, params}`; replies `{id, jsonrpc:"2.0", result}` / `{id, jsonrpc:"2.0", error:{code:-32000,message}}`.

Connect requirements (`computer_use_client.js:66-74`): `nodeRepl.nativePipe.createConnection` must be a function **and** `nodeRepl.config` truthy, else `"Computer Use native pipe is unavailable"`; `nodeRepl.env.SKY_CUA_NATIVE_PIPE_DIRECTORY.trim()` must be non-empty, else `"Computer Use native pipe path is unavailable"`. Other literals: `"Computer Use native pipe is closed"`, `"Computer Use native pipe closed before response"`, `"Computer Use native pipe connection timed out"`, `` `Computer Use native pipe is unavailable: ${s}` ``, `"computer-use request timed out: "+method`.

**Can plain Node connect to `\\.\pipe\...`?** Technically yes (`net.connect('\\\\.\\pipe\\name')` + the same UInt32LE framing), **but** this build never gives you a pipe name: the directory comes from the Electron host's `nodeRepl.env`, and the Rust exe contains **zero** `\\.\pipe` / `CreateNamedPipe` / `Usage:` strings (`_strings_small.txt` negative result). **[inference]** the named pipe is served by the native `nodeRepl` host (or by the Swift build) and the helper is handed the handle, not a pathname. → **Implement stdio; treat the pipe path as unavailable.**

---

## B. APPROVAL HANDSHAKE

### B.1 stdio: approval rides in the *response body* (this is your path)

The helper does **not** send a request. It answers your `click`/`type_text`/etc. with `ok:false` + `approvalRequest`, and your client must elicit and **re-send the same request**. Loop (`helper_transport.js:51-60`), byte-exact:

```js
let result, meta = turnMetadata;
for (;;) {
  const t = await this.#send(child, method, params, meta,
                             options.timeoutMs ?? (method === "launch_app" ? this.#launchTimeout : this.#defaultTimeout));
  if (t.kind === "result") { result = t.result; break; }
  await handleApproval(t.approvalRequest, turnMetadata, options.createElicitation);
  meta = withApprovedApp(meta, t.approvalRequest.app);      // { ...meta, "x-oai-cua-approved-app": app }
}
return result;
```

`"x-oai-cua-approved-app"` is the literal key (`codex_turn_metadata.js:1`, `APPROVED_APP_META_KEY`). Same `method`, same `params`, same `id`-space (a **new** id per attempt), `meta` gains the approved-app key. Retry is unbounded (`for(;;)`) — a helper that keeps answering `approvalRequest` loops forever.

Approval-body validation (`helper_transport.js:240-248`) — the shape you must emit/accept:

```js
{ app: string /* non-blank, trimmed, REQUIRED */,
  displayName: string /* trimmed, defaults to app */,
  riskLevel: "high" | "low" | undefined /* anything else -> undefined */,
  allowPersistentApproval: boolean /* `!== false`, i.e. defaults to TRUE */ }
```

`app === "computer-audio"` is the reserved pseudo-app id for audio consent.

### B.2 The elicitation call (`helper_transport.js:74-113`)

```js
const elicit = explicitCreateElicitation ?? globalThis.nodeRepl?.createElicitation;   // guarded
if (typeof elicit !== "function") throw new Error("Computer Use requires app approval but elicitations are unavailable");
const isAudio = approvalRequest.app === "computer-audio";
const canPersist = approvalRequest.allowPersistentApproval && !isAudio;
const toolCallId = firstNonBlank([meta.call_id, meta.item_id]);
const resp = await elicit({
  message: isAudio ? "Allow Computer Use to record computer audio?"
                   : `Allow Codex to use ${approvalRequest.displayName}?`,
  meta: {
    codex_approval_kind: "mcp_tool_call",
    ...(isAudio ? { codex_request_type: "approval_request" } : {}),
    connector_id: "computer-use",
    connector_name: "Computer Use",
    persist: canPersist ? ["session","always"] : ["session"],
    riskLevel: approvalRequest.riskLevel ?? "low",
    ...(isAudio ? { ...(toolCallId ? { tool_call_id: toolCallId } : {}), tool_name: "start_audio_recording" } : {}),
    tool_params: { app: approvalRequest.app },
    tool_params_display: [{ name: "app", display_name: "App", value: approvalRequest.displayName }]
  }
});
if (resp.action !== "accept") throw new Error(`Computer Use was not approved to use ${approvalRequest.displayName}`);
if (!canPersist && isPermanent(resp)) throw new Error(`Computer Use was not approved to use ${approvalRequest.displayName}`);
```

`isPermanent(resp)` ≡ `resp._meta?.persist === "always"` **||** `resp.content?.persist === "always"` **||** (`resp.content?.source === "computer-use-persisted-state"` && `resp.content?.scope === "global"`).

**Your Node host must supply `createElicitation`** (via `transport.request(..., { createElicitation })`), because `globalThis.nodeRepl` is absent. Its return contract is `{ action?: string, content?: object, _meta?: object }` (`types/targets/windows/HelperTransport.d.ts:ElicitationResponse`) and **only `action === "accept"` approves**.

### B.3 native-pipe variant (server-initiated request)

`computer_use_client.js:W` handles a helper→client request when `obj.method === "requestComputerUseApproval" && typeof obj.id === "string"` (string id distinguishes it from responses). Missing elicitation → `"Computer Use app approval UI is unavailable outside trusted node_repl"`. Reply: `{id, jsonrpc:"2.0", result:<elicitationResponse>}` or `{id, jsonrpc:"2.0", error:{code:-32000, message}}`. Exe anchors: `AppApprovalRequest` `riskLevel` `allowPersistentApproval` (`@0x1246374`), `AppApprovalRequired` `request` (`@0x1293416`), `Computer Use requires approval to use ` (`@0x1293288`).

### B.4 Browser URL policy gate (a second gate, after approval)

A target whose process is a browser — the exe matches `msedge` / `chrome` / `brave` / `opera` / `iexplore` / `firefox` (`@0x137e04`, `@0x1280xxx`) — passes the normal approval and then hits a second gate the helper runs itself. It reads the current URL and checks it against a site-status policy it fetches over WinHTTP from ChatGPT's backend: `GET https://chatgpt.com/backend-api/aura/site_status?site_url=<url>&url_request_source=codex_browser_use`, with `Authorization: Bearer <token>`, `originator:`, `ChatGPT-Account-ID:` headers (strings at `@0x139f60`, `@0x1280353`, `@0x1280400`). With the bundled stub there is no token and no account, so the fetch cannot resolve and the call ends the turn (not an `approvalRequest` — a thrown error). The four outcomes, byte-exact (`@0x13472f`…):

- `…it is not allowed on the current browser URL…`
- `…it could not verify whether the current browser URL is allowed…`
- `…it could not determine the current browser URL on Windows with enough confidence to enforce policy…` ← the one the stub hits
- `…browser URL policy enforcement is not yet supported for the current Windows browser…`

**The escape hatch is a request-meta key**, read in `func@0x7737b`: if `meta["x-codex-browser-use-security-mode"] == "disabled-for-local-testing"` the function returns early (sets the URL-allowed sentinel and skips the fetch). Measured live through the plugin's own transport: on a Chrome window, an approved `get_window_state` **without** the key throws the "could not determine" stop, and **with** the key returns the full UIA tree. A sibling meta key `ComputerUseAllowForbiddenTargets` is checked nearby but did **not** bypass the gate on its own. The key must ride the *approved* re-send, since the URL gate runs after approval; the plugin puts it on the base meta so every frame carries it (`lib/computer-use-client.js`, gated by `enforceBrowserUrlPolicy`, default off). This is the same mode name OpenAI uses for its own local testing, i.e. the helper's intended path for running outside ChatGPT.

---

## C. METHOD CATALOG

Full method list embedded in the exe, one contiguous blob at `@0x1253170` (byte-exact, this order):

```
start_audio_recording stop_audio_recording get_window_state list_apps launch_app list_windows get_window diagnostic_state activate_window perform_secondary_action set_value click_element scroll_element click drag scroll type_text press_key close end_turn
```

Unknown method → exe error string `unsupported method: ` (`@0x1253420`).

`create_client.js` (Windows) exposes **window2 only** (`src/types/window2/*`), and `WindowsComputerUseClientBase implements T.Window2.Client`. `create_tinysky_alt` is **not** in `@oai/sky` — it lives in the sibling `@oai/cua` package (`.../node_modules/@oai/cua/dist/lib/js/oai_js_cua/src/tinysky_alt/create_tinysky_alt.js`), is gated on `nodeRepl.env.CUA_REPL_ENABLED_SURFACES` (`"browser"`,`"computer"`; `throw new Error("CUA_REPL_ENABLED_SURFACES is required")`) and drives the full-desktop-ish `get_state`/`get_apps` surface. **Not used by the Windows Sky path** — negative finding.

`params` below are exactly what the JS writes after its own validation. `Window` = `{ app: string /* non-blank */, id: integer >= 0, title?: string }` — the `app`+`id` pair is the window handle. `p(window)` throws ``TypeError("window.app must be a non-empty string and window.id must be an integer >= 0")``; `id` may arrive as a numeric string (`v()` coerces it).

| JS method | RPC method | params (exact) | result |
|---|---|---|---|
| `list_apps()` | `list_apps` | `{}` | `Array<{id:string, windows:Window[], displayName?, lastUsedDate?, useCount?, isRunning?}>`; non-object items and items with a blank `id` are **silently dropped**; non-array → `[]` |
| `list_windows()` | `list_windows` | `{}` | `Window[]` (items failing `y()` coercion dropped) |
| `get_window({app?, id})` | `get_window` | `{app?, id}` (`WindowLookupParams`, 2 fields) | `Window`; `null` → `Error("codex-computer-use.exe did not return a window")`; input must be an object else `TypeError("get_window input must be an object")`, bad id → `TypeError("id must be an integer >= 0")` |
| `launch_app({app})` | `launch_app` | `{app}` (non-empty string else `TypeError("app is required")`) | `void`. **Timeout 15 s** (vs 10 s) |
| `get_window_state({window, include_screenshot=true, include_text=false})` | `get_window_state` | `{window, include_screenshot, include_text}` | see §D |
| `activate_window({window})` | `activate_window` | `{window}` | `void` |
| `click({window, element_index\|element\|elementIndex, click_count=1, mouse_button="left", x, y, screenshotId})` | **`click_element`** when an index is present | `{window, element_index, click_count, mouse_button}` (`ElementClickParams`, 4 fields) | `void` |
| | **`click`** when index absent | `{window, x, y, screenshotId?, click_count, mouse_button}` (`ClickParams`) | `void`; neither index nor finite `x`+`y` → ``TypeError(`${label} requires either element_index or finite x and y coordinates`)`` |
| `scroll({window,x,y,scrollX,scrollY,screenshotId})` | `scroll` | `{window,x,y,screenshotId?,scrollX,scrollY}` (`ScrollParams`, 6 fields) | `void`; coords rounded via `Math.round`, non-finite → ``TypeError(`${label} must be a finite number`)`` |
| `drag({window,from_x,from_y,to_x,to_y,screenshotId})` | `drag` | `{window,from_x,from_y,to_x,to_y,screenshotId?}` (`DragParams`, 6 fields) | `void` |
| `press_key({window,key})` | `press_key` | `{window, key}` (`PressKeyParams`, 2 fields) | `void`; falsy key → `TypeError("key is required")` |
| `type_text({window,text})` | `type_text` | `{window, text}` (`TypeTextParams`, 2 fields) | `void`; non-string → `TypeError("text is required")` |
| `set_value({window,element_index,value})` | `set_value` | `{window, element_index, value}` (`SetValueParams`, 3 fields) | `void`; non-string `value` → `TypeError("value is required")` |
| `perform_secondary_action({window,element_index,action})` | `perform_secondary_action` | `{window, element_index, action}` (`PerformSecondaryActionParams`, 3 fields) | `void`; blank `action` → `TypeError("action is required")` |
| `start_audio_recording({max_duration_ms}?)` | `start_audio_recording` | `{max_duration_ms}` (may be `undefined`) (`StartAudioRecordingParams`, 1 field) | `void` |
| `stop_audio_recording()` | `stop_audio_recording` | `{}` | `{filepath:string}` → client hydrates `{filepath, bytes:Uint8Array, data_url:"data:audio/wav;base64,…"}` |
| `closeTransport()` / `close()` | `close` | `{}` (`EmptyParams`) | `void` |
| *(internal)* | `end_turn` | `{}` | `void` |

**Exposed by the exe but never called by this JS:** `diagnostic_state`, `scroll_element`, `press_key`'s/`click`'s richer variants. `paste` is **mac-only** (`targets/mac/paste.js`, `MacComputerUseClient`); the Windows client has no `paste` and the exe has no `paste` method — the SKILL.md `sky.paste(...)` section is mac-centric **[inference]**.

Validation helpers worth reproducing: `R(element_index)` accepts an integer ≥0 or a numeric string, else ``TypeError("element_index must be an integer >= 0")``; `b()` ≡ finite & ≥1 (`` `${t} must be >= 1` ``).

`press_key` normalizer `E()`: `key.split("+").map(trim).filter(Boolean).join("+")`; empty → `TypeError("key is required")`. `j()` is the identity, so **aliasing (`Control`→`Control_L`, `Ctrl`, `Numpad_0`→`KP_0`, `period`, …) happens inside the exe**, whose canonical name table is the huge blob at `@0x1276172` (`CTRL CTRL_L … SHIFT_L … KP_0..KP_9 F1..F20`, plus `ENTER RETURN LINEFEED`, `ESCAPE`, `BACKSPACE`, `DELETE DEL`, `PAUSE`, `CAPS_LOCK`, `KP_MULTIPLY`, `BRACKETLEFT`, `BRACELEFT`, `BAR PIPE`, `~`, `;`, `` ` ``, …).

Scroll direction / mouse-button parsing (exe-side): `up/down/left/right` + `u/d/l/r` (`@0x1276012`), `unsupported scroll direction: `, `pages must be a finite number > 0`, `middle`/`m`, `unsupported mouse_button: `, `right double click is not supported`, `click_count must be >= 1`.

Serialization note: the exe's *wire* names are camelCase per the JS readers (`displayName`, `lastUsedDate`, `useCount`, `isRunning`, `screenshotId`, `zIndex`, `originX`, …), while `last_used`/`use_count`/`install_state`/`item_count`/`modified_unix_seconds`/`cached_at_unix_seconds` (`@0x1246718`) belong to the on-disk `shell-apps.json` cache, and `allow_browser_and_computer_use`/`feature_requirements`/`default_app_access` belong to policy TOML. **[inference from string clustering]**

### C.1 The `app` identifier namespace (measured against a live helper)

Everything below was read off a Host-spawned `codex-computer-use.exe` on this machine, not inferred from strings.

Three shapes appear in a `Window.app` and in `list_apps[].id`:

| shape | example | where it comes from |
|---|---|---|
| Start-Menu record id | `{6D809377-6AF0-444B-8957-A3773F02200E}\7-Zip\7zFM.exe` | a process the helper joined to its own shell index entry |
| packaged-app id (AUMID) | `{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\charmap.exe` | a packaged app; also the form `launch_app` quotes back in its errors |
| synthesized process id | `process:C:\Windows\System32\msinfo32.exe` | `process:` + the full image path, for a process the helper could not join to any record |

The `process:` literal sits in `.rdata` at RVA `0x133800` and has nine code references (`func@0x25eff` ×3, `0x2ae3f`, `0x2af01`, `0x2b533`, `0x89791`, `0x974db`, `0x97d0e` — the last being the dispatch hub that also serialises the window list), so the form is built in several places rather than being a one-off error decoration. The catalog record's `process_keys` field is the plausible join key behind all three **[inference from the serde field blob alone]** — a join decides *which* of the three shapes a process is reported under, never *whether* it is listed at all (that is the helper's integrity level, below).

**`launch_app` normalises before it reports.** Hand it an exe path and the failure names the *catalog identifier*, not the string you sent:

```
launch_app({app: "C:\\WINDOWS\\system32\\charmap.exe"})
  → launched app did not expose a targetable window: {1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\charmap.exe
```

So an id attempt and a path attempt produce textually identical refusals. A client that retries a launch through the index cannot tell "the retry never ran" from "the retry failed the same way" by reading the message alone — this cost a full debugging round.

**What `launch_app` accepts** (measured at Medium integrity): a helper app id, a packaged app's AUMID (`Microsoft.WindowsCalculator_8wekyb3d8bbwe!App`, started in 743 ms), or an exe path it can start (`C:\Windows\System32\msinfo32.exe`, 467 ms). `shell:AppsFolder\<AUMID>!App` → `launch_app requires an installed app id from list_apps or a launchable executable identifier`. `launch_app` answers `{}` before the window exists, so poll `list_windows` afterwards.

**Which windows are listed: every visible top-level window whose process the helper can open.** An earlier version of this section concluded the opposite — "a window is listed only if the helper itself started its process" — from a same-executable A/B, a kill test, and a census across helper deaths. The measurements were real; the conclusion was not. Every helper involved, Host-spawned and hand-spawned alike, was started from `plugins/dsh-computer-use/vendor/windows/`, inside a checkout that had been a Codex sandbox writable root and so carries an inheritable `Mandatory Label\Low Mandatory Level:(OI)(CI)(NW)` entry. Windows starts a process at the lower of its parent's integrity level and its image's label, so every one of those helpers ran at **Low**. The A/B that settles it is one binary started from two places by the same Node parent:

| helper image | integrity | `list_windows` | picture | `activate_window`, `click_element`, `click(x,y)`, `press_key` | `launch_app` of an AUMID |
|---|---|---|---|---|---|
| the labelled checkout | Low | 2 windows — the two Low `msinfo32` it had launched | `CreateForMonitor … (0x80070005)` | `failed to activate captured window` | `拒绝访问。 (0x80070005)` |
| a copy under `%TEMP%` | Medium | 11 windows — Chrome, VS Code, Explorer, Notepad, … | 1809×927 JPEG | all succeed | launched |

Read through that lens, the old findings explain themselves. Apps the helper launched inherited its Low token, so any later Low helper could open them — the "membership survives the helper's death" census. A second instance started from `pwsh` was Medium and therefore invisible — "per process, not per executable". The tracked and the untracked window looked identical from outside because the difference is the owning process's token, not anything on the window. The enumeration analysis below fits as well: a window is skipped when its identity cannot be resolved (`func@0xae479` → `func@0x25c3b`), and resolving it needs the owning process, which a Low token cannot open when that process runs at Medium **[inference: consistent with every measurement above; the failing call inside `func@0x25c3b` was not read]**. The plugin now runs the helper from a Medium-labelled copy (`plugins/dsh-computer-use/lib/helper-stage.js`).

**What the enumeration itself checks** (REA, instruction level). `func@0xab00d` drives `EnumWindows` with callback `func@0xaecab`, which skips a window that fails the style/visibility filter (`func@0x383c3`, reading `IsWindowVisible` and `GetWindowLongPtrW`), a cloaked window (`func@0x38477` → `DwmGetWindowAttribute`, attribute 14), a window whose `GetWindowThreadProcessId` yields pid 0, and a window whose identity cannot be resolved (`func@0xae479` → `func@0x25c3b`, which is also where the `process:` identifier is built). `IsIconic` does *not* skip: an iconic window is routed to the usability gate `func@0xabbeb`, which reads attribute 9 over 16 bytes (`DWMWA_EXTENDED_FRAME_BOUNDS`, by size and fallback-to-`GetWindowRect` behaviour **[inference]**) and rejects with `window has invalid bounds or is not visible` when the rect is degenerate or off-screen past −29999. So **pid is only ever tested against zero here** — "is this one of mine" is not an enumeration filter, it lives in the identity resolution. Separately, a `CreateToolhelp32Snapshot` pass (`func@0x36f0e`, `PROCESSENTRY32W` at `rsp+0x168`, `dwSize` 0x238) walks live processes and probes a fixed-stride record array of `0x70` = 112 bytes whose leading dword is a pid (`cmp dword ptr [rdi + r8 - 0x70], r9d` at `0x37259`). None of the three snapshot loops reads `th32ParentProcessID` — which is offset `+0x20`, not `+8`; `+8` is `th32ProcessID` and `szExeFile` starts at `+44` — so parent-chain attribution is a **strong but not exhaustive negative** **[open: only a full line-by-line disassembly of the three loops could still surface a parent-pid read passed straight to a call]**. The helper also takes one explicit lineage parameter from its caller, `--parent-pid <node pid>` (`lib/helper-pool.js:26`), used as a die-with-parent watchdog.

**`list_apps` result shape (measured, raw).** A bare JSON array, not an object; the plugin's `shapeApps` maps it:

```jsonc
[ { "displayName": "系统信息",
    "id": "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\msinfo32.exe",
    "isRunning": true,
    "windows": [ { "app": "process:C:\\Windows\\System32\\msinfo32.exe", "id": 134582, "title": "系统信息" } ] },
  { "displayName": "TRYP FPV", "id": "steam://rungameid/1881200",
    "isRunning": false, "lastUsedDate": "2026-10-26", "useCount": 0, "windows": [] } ]
```

Note the asymmetry inside one record: `id` is the shell/AUMID identifier while `windows[].app` is the *runtime* identifier the other tools take — they differ for the same app, which is why every action tool needs the object the listing returned rather than a reconstructed id. `lastUsedDate`/`useCount` appear only on the not-running shell entries.

**Window ids are hwnds and do survive a helper restart** — the same live `msinfo32` came back as hwnd `134582` before and after its helper was replaced. An earlier version of this section said the opposite, having compared two *different* instances of the app (`13699836` then `6686800`); the id changes when the **app** is restarted, not when the helper is. Re-derive `{app, id}` from `list_windows` after relaunching an app, and keep the defensive rebind in the plugin's `lib/computer-use-client.js` for the case where the helper died and a tool call still holds an old transport.

---

## D. `get_window_state` RESULT SHAPE

Validator (`computer_use_client_base.js:24-58`), reproduces exactly what the exe must return:

```jsonc
{
  "window":        { "app": "…", "id": 12345, "title": "…" },   // falls back to the request's window if absent
  "screenshots":   [ { "url": "data:image/jpeg;base64,…",        // REQUIRED string, non-empty
                       "id": "screenshot-0",                      // default when absent
                       "zIndex": 0,                               // default 0 when absent/non-finite
                       "originX"?: number, "originY"?: number,    // screen origin of the captured region
                       "width"?: number, "height"?: number } ],   // logical px
  "accessibility": null | {
      "tree": "…",                        // REQUIRED string when accessibility present
      "focused_element"?: string,         // formatted line for the focused element
      "selected_text"?: string,
      "selected_elements"?: string[],     // all entries must be strings
      "document_text"?: string }
}
```

Hard errors (byte-exact): `"codex-computer-use.exe did not return window state"`, `"codex-computer-use.exe did not return accessibility state"`, `"codex-computer-use.exe did not return accessibility tree"`, `` `codex-computer-use.exe did not return ${field}` `` (used for `selected_elements`), `"codex-computer-use.exe did not return screenshots"`, `"codex-computer-use.exe did not return a screenshot"`, `"codex-computer-use.exe did not return a screenshot URL"`, `"codex-computer-use.exe did not return computer audio"`, `"codex-computer-use.exe did not return a computer audio filepath"`.

Semantics:
* `include_screenshot` and `include_text` — at least one must be true client-side: `TypeError("get_window_state must request include_text, include_screenshot, or both")` (mirrored inside the exe, `@0x1264336`). `screenshots` may be `null`/absent when `include_text` was the requested mode; it is `[]` then. If `include_text` was requested and `accessibility` is `null`, that is legal (→ `accessibility: null`).
* `screenshots[]` is more than the target window: SKILL.md — *"Bounded screenshots captured for the window **and related transient UI**"* (tooltips, context menus, combos). The exe also captures an `another window` fallback (`@0x1253170`) and errors `no screenshots captured for ` / `no screenshot targets found for ` / `no desktop window`.
* `screenshotId` is **mandatory-in-practice for coordinate input**: pass back the `id` of the screenshot whose `originX/originY/width/height` your `x`/`y` were read from. Server-side guards: `point (<x>,<y>) is outside viewport { originX: N, originY: N, width: N, height: N }`, `point (…) is over <process>, not target window <id>; activate the target or take a fresh screenshot before retrying`, `coordinate input geometry is unavailable`, `lastWindowIdentity`.
* `accessibility.tree` is a **pre-formatted, model-facing text tree** ("including element indexes and tab hierarchy"). Element indices are the numbers embedded in that text and are only valid against the snapshot that produced them (SKILL.md: *"Element indexes are valid only for the accessibility state that produced them"*). The server keys its cache on the window and rejects stale use: `element <n> has no cached bounds`, `element <n> no longer exists in <…>`, `element <n> is not available in cached app state for <…>`, `no cached app state is available for <…>`, `window bounds changed; call get_window_state before using this window`, `window changed; call get_window_state before using this window`, `call get_window_state before using this window`, `window is minimized; call activate_window, refresh with get_window, then retry get_window_state`, `user input was detected in this window; call get_window_state before continuing` (`@0x1264336`, `@0x1253017`, `@0x1254640`).
* `element_index` therefore = index into the exe's cached UI Automation snapshot for that window (`snapshotRevision`, `accessibilityRevision`, `accessibilitySnapshotCount`, `captureCachedSessionCount`, `lastCaptureInvalidationReason` are the diagnostic fields behind `diagnostic_state`, `@0x1245830`).
* Screenshots are emitted as a side effect after a successful call: `nodeRepl.emitImage(url)` else `codex.emitImage({type:"input_image", image_url:url, detail:"original"})`, both optional — **no-op in plain Node**, so you must surface `result.screenshots[].url` yourself.
* Images are JPEG: `image/jpeg`, `BitmapEncoder.CreateAsync`, `ImageQuality` (`@0x1282179`).

### D.1 Golden — live A/B measurements against the original helper (2026-11)

* **Engine architecture (PE analysis).** `bin\windows\codex-computer-use.exe` (1.5 MB) is a launcher stub with an **empty import table**; the real engine is `bin\windows\swift\x64\codex-computer-use-swift.exe` (~55 MB, MSVC toolchain — pulls `MSVCP140.dll`, `VCRUNTIME140*.dll`, sha256-pinned in `manifest.json`). The stub relays the stdio JSON-RPC contract to the swift engine.
* **Result top level** (measured, text-only mode): `{ "window": {app,id,title}, "screenshots": [], "accessibility": {…}, "cacheDiagnostics": {…} }`. `window` carries **no** `displayName`. `screenshots` is always present (`[]` when `include_screenshot:false` or capture failed — capture failure is silent in text mode). `accessibility` is omitted when `include_text:false`.
* **Check order (measured on a minimized window):** serde parse → window resolution (pid/app/owner) → **approval** → **both-false include check** → **minimized refusal**. A first-touch unapproved minimized window yields `approvalRequest`, not the minimized error.
* **Tree format (277-line golden vs a 278-line original capture, byte-exact on shared nodes):**
  `Window: "{title}", App: {exe-file-name}.` header, then one line per node: `"\t".repeat(depth+1) + "{index} {LocalizedControlType}"` plus optional suffixes **in this order**: `" (disabled)"`, `" (settable, string)"`, `" {name}"`, `" Description: {helpText}"`, `" Value: {value}"`, `" ID: {automationId}"`, and `" Secondary Actions: Raise"` **only on the window root**.
* `focused_element` is **the focused node's own tree line** (same index), not a separately formatted record.
* `document_text` = the `ValuePattern` value of the **last** `Document` node in the walk.
* `cacheDiagnostics`: `{accessibilityRevision, accessibilitySnapshotCount, captureCachedSessionCount}`; a freshly built tree bumps revision **+1** and snapshot count **+3**; a second call on an unchanged window (same title/origin/size) replays the cache with no bump.
* `diagnostic_state` result: `{ "cacheDiagnostics": {…}, "lastWindowIdentity": null | {appId, bounds:{originX,originY,width,height}, inputHwnd:"985200" (STRING), processId:47444 (int), processName:"App.exe", rootHwnd:"985200" (STRING), snapshotRevision, title} }`. `bounds` are **DPI-physical** pixels — the helper process is per-monitor-v2 DPI aware.
* **Suspended providers:** plain UIA walker calls see only the top 3–4 nodes of a provider suspended by the lock screen, while the original renders the full 277-node tree from the same spawn context. Live UIA event subscriptions (`AddStructureChangedEventHandler` on the window root, `TreeScope_Subtree`) make the UIA core bring provider connections up; our engine does exactly that before walking (`wake_provider`).
* **Wire deviation accepted:** `approvalRequest` JSON field **order** differs (original emits struct-definition order `app,displayName,riskLevel,allowPersistentApproval`; ours is alphabetical). JSON-semantically identical; every JSON.parse consumer is unaffected.

---

## E. INTERRUPT / ESC / TURN LIFECYCLE

Three independent mechanisms.

### E.1 Escape marker files (JS ⇄ exe handshake, filesystem)

Directory component, byte-exact: `` a.join("cache","computer-use","interrupts") `` (JS `@helper_transport:16`; exe literal `cache/computer-use/interrupts` `@0x1275158`).

Path builder (`helper_transport.js:115-116`):

```js
F(dir, scope) = path.join(scope.codexHome, dir, sanitize(scope.conversationId), sanitize(scope.turnId))
// sanitize(t) = t.replace(/[^A-Za-z0-9._-]/g, "_")
```

so the full path is

```
<CODEX_HOME>\cache\computer-use\interrupts\<session_id>\<turn_id>
```

(an **empty** file, `""`, written `utf8` after `mkdirSync(dirname, {recursive:true})`).

**Who writes it:** the helper (on physical ESC — `wait for computer-use turn key`, `create computer-use interrupt marker directory`, `write computer-use interrupt marker`, `computer-use-escape-stop`, log `computer-use turn interrupted by physical Escape key`, `@0x1275158`/`@0x1274841`), **and** the JS client itself in two cases (§E.2).

**When it is checked:** at the top of *every* `request()` (`helper_transport.js:45-48`), before spawning/sending, for the current turn scope only:

```js
if (turnScope != null && existsSync(F(INTERRUPT_DIR, turnScope))) {
  this.#interruptedTurn = turnKey;                       // sticky for this turn
  return Promise.reject(new Error(ESCAPE_MSG));
}
if (this.#interruptedTurn != null) {
  if (turnKey === this.#interruptedTurn) return Promise.reject(new Error(ESCAPE_MSG));
  if (turnKey != null) this.#interruptedTurn = null;     // cleared on turn change
}
```

Error text, byte-exact (JS constant `I` ≡ exe literal, identical):

```
Computer Use was stopped by the user with the physical Escape key. Stop your work, do not call further Computer Use tools in this turn, and send a final message noting that the user stopped Computer Use.
```

Once tripped, every further call **in that turn** rejects with that exact string; a new turn clears it. The keyboard hook that detects ESC lives in `src/input/interruption.rs` (`failed to install keyboard hook`, `hook state mutex poisoned`, `failed to install mouse hook`, `@0x1275650`).

### E.2 Exit code 130

`helper_transport.js:174-177`:

```js
child.on("exit", (code, signal) => {
  globalThis.process.off("exit", onHostExit);
  if (code === 130) this.#markInterrupted(this.#lastTurnScope);   // sets #interruptedTurn + writes the marker file
  …
  const err = code === 130 ? new Error(ESCAPE_MSG) : new Error(/* helper exited … */);
  this.#cleanup(child); this.#rejectAll(err, child); for (const cb of this.#exitListeners) cb();
});
```

**Confirmed: exit code 130 is the ESC-pressed signal.** It (a) writes the interrupt marker for the last-known turn scope, (b) rejects all pending with the exact ESCAPE message instead of the "helper exited" message. **[inference]** the exe self-terminates with 130 on ESC so that a supervising CLI propagates the interrupt.

### E.3 Turn transitions, `end_turn`, and `Local\CodexComputerUseTurnEnded`

Turn metadata source (`codex_turn_metadata.js`, byte-exact): merge JSON-parsed `process.env.NODE_REPL_REQUEST_META` → merge `globalThis.nodeRepl?.requestMeta` → capture then **delete** the `x-codex-turn-metadata` key and splice in its JSON-parsed contents → re-add `x-oai-cua-approved-app` only if it is a non-blank string.

Turn scope (`helper_transport.js:149-156`): requires **both** `meta.session_id` and `meta.turn_id` to be non-blank strings, else `null` (→ no turn tracking, no interrupt checks, no `end_turn`). Scope key = `` `${codexHome}\0${conversationId}\0${turnId}` `` with `codexHome = X()` (see §F.1). The exe accepts the aliases `thread-id | session_id | sessionId | conversation_id | conversationId` and `turn-id | turn_id | turnId` (`@0x1274894`, `@0x1275032`).

Automatic `end_turn` (`helper_transport.js:184-190`, `R`): before dispatching any request other than `end_turn`, if the incoming turn key differs from the tracked one, the transport sends `end_turn` `{}` with the **previous** turn's metadata and swallows any error, then records the new turn key/metadata.

`end_turn` short-circuit (`:41`):

```js
if (method === "end_turn" && (turnKey == null || turnKey !== this.#currentTurnKey
    || this.#child == null || child.exitCode != null || child.signalCode != null)) return null;
```

and after a successful `end_turn` the tracked turn key/metadata are cleared (`:60`).

Base-client hook (`computer_use_client_base.js:183-198`), Electron-only: `globalThis.nodeRepl?.addTurnEndedHandler({ timeoutMs: 4000, run })` → fires `transport.request("end_turn", {}, {codexTurnMetadata})` when the REPL reports the turn ended, guarded so it fires once per turn. In plain Node this is `undefined` → you must drive `end_turn` yourself at end-of-turn (or just `close()`).

Helper side: `signal computer-use turn-ended event` + `Local\CodexComputerUseTurnEnded` (manual-reset event, `create computer-use turn-ended event`, `wait for computer-use turn-ended event failed`, `computer-use turn-ended watcher unavailable: `, `@0x1274536`/`@0x1274751`/`@0x1275104`), whose payload carries the turn key; when it fires the helper refuses further calls with

```
Computer Use is no longer available in this turn because the turn has ended. Do not call further Computer Use tools in this turn.
```

(`@0x1274536`). Also: `computer-use previous notify hook failed: `, `… skipped: missing command`, `… skipped: invalid arguments`, `missing value for --previous-notify`, `missing turn-ended payload` (`@0x1274150`) — the `--previous-notify <json>` + `turn-ended` argv hook used by the Codex CLI `notify` integration; not needed by a Node host.

---

## F. POLICY / CONFIG — every path and env var

### F.1 Codex home resolution (`helper_transport.js:117-148`, `X()`)

```
1. process.env.CODEX_HOME, .trim()  — if non-empty:
     /^[A-Za-z]:[\\/]/ | startsWith("\\\\") | startsWith("//")   → normalize "/" → "\"
     ^/mnt/<single-letter>/…                                      → "<DRIVE>:\…"   (WSL form)
     startsWith("/")                                              → spawnSync("wsl.exe",
                                                    WSL_DISTRO_NAME ? ["-d",distro,"--","wslpath","-w",p]
                                                                    : ["wslpath","-w",p], {encoding:"utf8"})
                                                                    use trimmed stdout if status 0, else the raw string
     otherwise                                                    → as-is
2. else if process.platform === "win32" && process.env.USERPROFILE?.trim() → path.win32.join(USERPROFILE, ".codex")
3. else                                                          → path.join(os.homedir(), ".codex")
```

The exe implements the same resolver independently (`CODEX_HOME`, `wsl.exe`, `WSL_DISTRO_NAME`, `-d`, `--mnt`, `wslpath`, `-w`, `.codex`, `USERPROFILE` — `@0x1275589`, `@0x1251613`, `@0x1254968`). **Set `CODEX_HOME` explicitly in your host process** so JS and the exe agree; otherwise the interrupt-marker path can diverge and ESC stops working.

### F.2 Filesystem paths

| Path | Written by | Required? |
|---|---|---|
| `<CODEX_HOME>\cache\computer-use\interrupts\<conv>\<turn>` | exe (ESC) + JS (exit 130) | optional; absence = not interrupted |
| `<CODEX_HOME>\cache\computer-use\shell-apps.json` | exe (`computer-use-app-catalog` cache, `@0x1278384`) | optional (self-healing cache) |
| `<CODEX_HOME>\cache\computer-use\user-assist.json` | exe (`@0x1278800`) | optional |
| `<CODEX_HOME>\cache\computer-use` (root) | exe | optional |
| `<CODEX_HOME>\config.toml` | exe writes the Codex **notify** hook (`resolve Windows Computer Use executable path`, `create Codex config directory`, `write Codex notify config`, `notify notify=`, `@0x1274382`, module `src/codex/notify_config.rs`) | **not required to pre-exist** — the exe creates the dir/config as needed **[inference]** |
| `<CODEX_HOME>\computer-use\config.json` | exe (status-pill config; same string cluster as above, `@0x1274382`) | optional — see §G |
| `bin\windows\codex-computer-use.exe` | packaged | **REQUIRED** (or `OAI_SKY_WINDOWS_BIN`) |
| Registry `Software\Microsoft\Windows\CurrentVersion\App Paths`, `shell:AppsFolder`, `%APPDATA%\Microsoft\Windows\Start Menu\Programs`, `%ProgramData%\Microsoft\Windows\Start Menu\Programs`, `%LOCALAPPDATA%\Microsoft\WindowsApps`, `\windowsapps\__`, `.exe`, `app-user-model-id:` | exe app catalog (`src/shell/app_catalog.rs`, `src/shell/registry.rs`, `src/shell/process.rs` `@0x1255029`–`@0x1280184`) | n/a (helper-internal) |

### F.3 Env vars

| Var | Read by | Effect / default |
|---|---|---|
| `OAI_SKY_CONFIG_PATH` | `load_options.js` | path to a JSON file → `Options`; else platform-derived `{target:"windows"}`; else `` `${var} missing, unhandled process.platform=${platform}` `` |
| `OAI_SKY_WINDOWS_BIN` | `package_bin.js` | absolute helper exe path, trimmed; overrides packaged path |
| `CODEX_HOME` | JS + exe | see §F.1 |
| `USERPROFILE` | JS + exe | `<USERPROFILE>\.codex` default |
| `WSL_DISTRO_NAME` | JS (+ exe) | `-d <distro> -- wslpath -w` |
| `NODE_REPL_REQUEST_META` | JS | JSON (string or object) merged into per-request `meta`; **must carry `session_id` and `turn_id`** to enable turn tracking/interrupts/`end_turn` |
| `SKY_ENABLE_AUDIO` | JS (`computer_use_client_base.js:198`) **and** exe (`@0x1265259`) | must be exactly `"1"`; otherwise `start_audio_recording`/`stop_audio_recording` are `Reflect.deleteProperty`'d off the client, and the exe answers `computer audio recording is not enabled` |
| `SKY_AUDIO_MODEL_ALLOWLIST` | JS (`require_audio_model_support.js`) | JSON array of model names; else `"SKY_AUDIO_MODEL_ALLOWLIST must be a JSON array of model names"`; model read from `requestMeta["x-codex-turn-metadata"]` (JSON string) `.model`; mismatch → `"Computer audio recording requires a model that supports audio input"`. Only enforced on the `sky.js`/`service.js` REPL layer |
| `CODEX_CLI_PATH` | **exe** (`@0x1281124`) | locates the `codex` CLI used to spawn `codex app-server` for auth + policy |
| `SKY_CUA_NATIVE_PIPE` / `SKY_CUA_NATIVE_PIPE_DIRECTORY` | JS via `nodeRepl.env` | Electron only |
| `ComSpec`/`COMSPEC` | `core/cli.js` only (`/d /s /c <cmd>.cmd`) | **not used** by the Windows helper transport |

### F.4 Policy / auth the helper enforces itself (relevant to "why does my call fail")

The exe is a **client of `codex app-server`** (`src/policy/auth.rs`, `@0x1244104`–`@0x1281224`): it spawns it, does `initialize` with `clientInfo { name:"codex-computer-use", title:"Windows Computer Use Helper", version:"0.1.0" }`, `capabilities { experimentalApi }`, `optOutNotificationMethods`, then `getAuthStatus` with `includeToken` / `refreshToken` / `authToken` / `authMethod` (`chatgpt`, `chatgptAuthTokens`), `accountId`, `chatgpt_account_id`. Failure strings: `failed to launch codex app-server`, `failed waiting for codex app-server`, `codex app-server exited without a result`, `codex app-server exited before returning response`, `failed to read/parse/encode/write app-server request…`, `failed to open codex app-server stdin/stdout`, `codex app-server did not return an auth token`, `unsupported Codex auth method: `, `Codex auth method is unavailable`, `codex app-server rejected `.

Policy RPCs `configRequirements/read` and `config/read` (`@0x1258232`, `@0x1251806`, plus `includeLayers`), errors: `Computer Use is disabled by policy`, `configRequirements/read did not return requirements`, `config/read did not return config`, `Computer Use is not allowed on this app by policy`, `policy cache mutex poisoned`. Policy schema keys (`@0x1246718`, `@0x1246374`): `allowBrowserAndComputerUse`/`allow_browser_and_computer_use`, `featureRequirements`/`features`, `computerUse`/`computer_use`, `allow_persistent_approval`, `defaultAppAccess`/`default_app_access`, `aumids`, `exes`, `allow`, `deny`; structs `PolicySource(3) AppRules(3) WindowsRules(2) ExeRule(4) PolicyFeatures(1)` with `ExeRule` fields `exe`/`access`/`publisher_name`/`product_name`/`binary_name`/`display_name`/`identifier`/`target_path`/`process_keys`. Also `ComputerUseAllow`, `ForbiddenTargets`, `x-codex-browser-use-security-mode`, `disabled-for-local-testing` (`@0x1255328`) — the latter is the local-testing escape hatch **[inference]** — and a WinHTTP `GET /backend-api/aura/site_status?site_url=&url_request_source=` to `chatgpt.com` with `Authorization: Bearer `, `originator: `, `ChatGPT-Account-ID: ` (`@0x1280352`–`@0x1280831`).

**The handshake actually happens, and the helper blocks on it (measured live).** With `CUA_APPSERVER_LOG` forced inside the generated launcher, every trial logged the same three requests within milliseconds of the helper's start and nothing else for the life of the helper:

```jsonc
// → initialize            clientInfo.name "codex-computer-use"
// ← { userAgent: "deepseek-harness-computer-use/0.1.0-dsh (win32; x64) codex-computer-use",
//     codexHome: "…\\AppData\\Local\\dsh-computer-use", platformFamily: "windows", os: "windows" }
// → configRequirements/read  ← { supportsIndependentSpeedModes: true, requirements: null }
// → config/read              ← { config: { computer_use: { allow_browser_and_computer_use: true,
//                                   default_app_access: "allow", allow_persistent_approval: true,
//                                   apps: [] }, browser_use: null } }
```

Two consequences for the port. A `config/read` that never answers is fatal, not degraded: a recorder that accepted the stdio pipe but never replied turned `launch_app` into `HelperTimeout … "launch_app" within 15000ms`, which is why the stub must stay a synchronous replier. And `default_app_access:"allow"` with an empty `apps` list gates nothing, so the policy answer was never the reason a window failed to appear — the helper's integrity level (§C.1) was. Note also that **`getAuthStatus` never arrived** on this path even though the strings above imply it; the stub answers it only in case a future build asks. An earlier draft of this file inferred from a missing `app-server.log` that the helper "never consults" the stub — wrong, and the file is absent only because `plugins/dsh-computer-use/index.js:276` hands the stub a log path when `verboseLogging` is on.

---

## G. STATUS PILL / CURSOR

The status pill is drawn **by the exe** with Windows.UI.Composition (module `src/overlay/*`, huge `create display overlay …` / `create pill …` string runs at `@0x1284166`), overlay window/element name `codex-computer-use-status-pill` (`@0x1284166`), plus a separate **system cursor manager** child process.

Config object (`ComputerUseConfig`, 4 fields, `@0x1246464`):

```
accentColor : string   // default literal "#339cff"  (@0x1251792)
locale      : string
strings     : ComputerUseConfig_OverlayStrings { usingComputer: string, escToCancel: string }
<4th field> : unresolved from strings  [inference: a direction/visibility switch]
direction   : "ltr" | "rtl"      // present in the same rodata cluster
```

Written/read under `src/codex/notify_config.rs`, same string cluster as `config.toml` / `create Codex config directory` / `write Codex notify config` / `computer-use` / `config.json` (`@0x1274382`) → **[inference]** `<CODEX_HOME>\computer-use\config.json`, written by the **exe**, not by `@oai/sky` JS.

**Verified negative:** `grep` over the whole shipped `sky_js` tree for `accentColor|config.json|statusPill|status_pill|usingComputer|escToCancel` → **zero matches**. The JS client writes nothing before starting the helper. **The exe does not require it** (it has defaults, incl. `#339cff`) — so a Node host can omit it entirely; supply it only to localize the pill text or change the accent.

Kernel objects (for cleanup/debugging, all `Local\` namespace — per-session, non-elevated):

* `Local\CodexComputerUseTurnEnded` — turn-ended event (§E.3)
* `Local\CodexComputerUseSystemCursorManager` — cursor-manager mutex, with suffixed events `-suppress`, `-restore`, `-shutdown`, `-ready`, `-ack` (`@0x1258896`–`@0x1258987`); the manager is a re-exec of the exe with `--system-cursor-manager` (`@0x1259050`), with `parse system cursor manager parent pid` and errors `missing system cursor manager parent pid|restore event|shutdown event|ready event|ack event|suppress event`, `system cursor manager did not ack `, `system cursor manager exited with `, `poll system cursor manager after operation timeout`
* `Local\CodexComputerUseCursorOverlaySingleton` — single-instance mutex for the overlay (`create cursor overlay singleton mutex`, `@0x1268112` / `@0x1268020`)

Initial cursor placement: `SetCursorPos initial overlay cursor position` (`src/input/initial_cursor.rs`, `@0x1275920`).

---

## H. GOTCHAS

1. **`--parent-pid` is appended only for the packaged exe.** Any custom `helperCommand` (e.g. your own wrapper) must add `--parent-pid <pid>` itself, else `parse parent pid` / `missing parent pid` (`@0x1269510`, `@0x1254136`). Unknown argv → `unknown argument: `; there are no other required args on the RPC path.
2. **`--parent-pid` is the host PID** and the exe uses it for supervision + the cursor-manager parent handle; killing the parent kills the helper (`missing parent pid`), and the JS additionally `kill()`s the child on host `exit`.
3. **`nodeRepl` assumptions — all optional.** Every use is `globalThis.nodeRepl?.…` guarded: `env` (pipe opt-in), `nativePipe.createConnection`, `config`, `createElicitation`, `requestMeta`, `emitImage`, `addTurnEndedHandler`, `withSuspendedTimeout`. `withSuspendedTimeout(fn)` merely extends the REPL idle timeout; absent → direct call. **No hard `nodeRepl` dependency on the stdio path.** `sky.js`/`service.js` *do* hard-require it (`"sky requires node_repl; configure NODE_REPL_TRUSTED_SERVICES"`, `"Computer Use requires a trusted Node REPL Sky service"`) — so **import `targets/windows/create_client.js` (or construct `WindowsComputerUseClient` directly), not `sky`**.
4. **Approvals will hang/fail without an elicitation provider.** Exact throw: `"Computer Use requires app approval but elicitations are unavailable"`. In a headless host, pre-seed the policy `allow` list / `allow_persistent_approval` or supply `createElicitation`. The retry loop is unbounded → a helper stuck on approval spins forever.
5. **`session_id`+`turn_id` are effectively load-bearing.** Without both non-blank: no turn key → no ESC-interrupt checks, no `end_turn`, no per-turn cleanup, and stale helper state can leak across turns.
6. **One malformed stdout line aborts every in-flight request** (`` computer-use helper emitted invalid JSON: ${line} ``). Do not let anything else write to the helper's stdout.
7. **Timeouts:** default `10000 ms`, `launch_app` `15000 ms`; per-request `timeoutMs` and constructor `timeoutMs` override both (note the constructor value sets **both** fields — a single `timeoutMs` overrides the `launch_app` special case too). Message: `` `computer-use request timed out: ${method}` ``. On timeout the child is **killed** unless `preserveHelperOnTimeout: true` (default `false`). The budget is also forwarded to the exe as `x-oai-cua-request-budget-ms`, and the exe independently fails with `computer-use request budget exhausted` — set it generously (SKILL.md documents the helper itself waiting ~1 s, up to ~5 s, after an action before capturing state).
8. **Serialized request queue.** All client calls are funnelled through a single promise chain (`w()`), and all requests share one stdin — the helper is single-threaded with respect to the RPC channel. Do not parallelize from your host.
9. **Shared transport refcounting.** Module-level `Map` keyed on `` JSON.stringify({helperArgs, helperCommand, timeoutMs}) `` with `refCount`; `closeTransport()` decrements and only truly closes at `≤0`. Two clients with identical options share one exe process; `Object.freeze(WindowsComputerUseClient)`.
10. **Coordinate inputs are snapshot-guarded server-side**, and the guards are the failure mode you will hit most: `window bounds changed; call get_window_state before using this window`, `window changed; call get_window_state before using this window`, `call get_window_state before using this window`, `read current window bounds before input`, `window id is required`, `coordinate input target is unavailable`, `window capture worker is not running`, `point (…) is over <proc>, not target window <id>`. Treat `get_window_state` as a cheap-enough re-anchor and always pass a fresh `state.window` + matching `screenshotId`.
11. **`click` splits into two RPC methods** (`click` vs `click_element`) and the `element_index` alias set is `{element_index, element, elementIndex}` — easy to break by passing both.
12. **Audio is double-gated** (`SKY_ENABLE_AUDIO=1` in the host process *and* in the helper's inherited env — one env, so setting it once suffices) plus model allowlisting on the REPL layer; otherwise the methods literally do not exist on the client object.
13. **Hard-coded paths / App-execution-alias assumptions (exe side):** `%LOCALAPPDATA%\Microsoft\WindowsApps`, `\windowsapps\__`, `app-user-model-id:`, `shell:AppsFolder`, Start-Menu scanning, `Software\Microsoft\Windows\CurrentVersion\App Paths`. `launch_app` rejects pids and ambiguous ids: `launch_app requires an installed app id from list_apps or a launchable executable identifier`, `launch_app does not support pid app identifiers`, `` `${id} is ambiguous; use the canonical app id from list_apps` `` (`@0x1265680`, `@0x1278714`). Always pass `list_apps()`-returned `id`s.
14. **Code-signature / parent checks:** the exe's only parent-related logic found is `--parent-pid` + `parse system cursor manager parent pid`. No `WinVerifyTrust`, `CryptQueryObject`, Authenticode, or `OpenProcess`-token *policy* strings were found beyond `open target process ` / `open target process token ` (used for the target app's token, `@0x1254545`). **[inference]** no signature enforcement — but the **policy/app-server** layer is the real gate (§F.4), so a machine without a logged-in Codex CLI (`CODEX_CLI_PATH`, `codex app-server`) will fail with `Computer Use is disabled by policy` / `codex app-server did not return an auth token` rather than a transport error.
15. **Single-instance:** the only singleton found is `Local\CodexComputerUseCursorOverlaySingleton` (overlay). Multiple helper processes are therefore allowed by design (the JS caches one per option-key, not globally), but they will contend for the cursor manager and the physical mouse/keyboard.
16. **`child.unref()` everywhere** → your host must hold its own handles open; otherwise Node exits while requests are outstanding (the `process.once("exit")` killer then SIGKILLs the helper).
17. **`Object.freeze`** on `WindowsComputerUseClient` and `Reflect.deleteProperty` on the audio methods — don't monkey-patch the client.
18. **`emitImage` is a no-op in Node** — screenshots arrive only inside `result.screenshots[].url` as `data:image/jpeg;base64,…` and can be multi-MB; the stdio path has no frame-size cap (unlike the pipe's 8 MiB send cap).
19. **The exe is a `windowsHide` GUI-ish process, not a console app** — never assume an interactive console, and never write to its stdin after `close()`.
20. **`end_turn` returns `null`, not a result object**, when short-circuited — treat `null` as success.

---

## I. Minimal faithful reimplementation (pseudo-contract)

```
spawn(exe, ["--parent-pid", String(process.pid)],
      { stdio:["pipe","pipe","pipe"], windowsHide:true })     // env inherits process.env; set CODEX_HOME + NODE_REPL_REQUEST_META
loop:  write(JSON.stringify({ id, method, params,
                             meta: { ...requestMeta, "x-oai-cua-approved-app"?: app,
                                     "x-oai-cua-request-budget-ms": timeoutMs } }) + "\n")
       read lines split on "\n"; match numeric id; ok→result | approvalRequest→elicit+retry | error→throw string
before each request: existsSync(join(CODEX_HOME,"cache","computer-use","interrupts",san(session_id),san(turn_id)))
                     → throw ESC_MSG, sticky per turn key
on turn change: send end_turn {} with the PREVIOUS turn's meta (ignore errors)
on close: request("close",{}) w/ 10 s, fallback child.kill()
treat child exit 130 as the ESC condition (write marker, reject pending with ESC_MSG)
```
