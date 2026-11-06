// The model-visible tool surface. Every tool maps onto one Sky RPC using the
// exact frame contracts from SKY_WINDOWS_PROTOCOL_SPEC §B.2; validation is
// client-side first so bad calls never reach the helper. Results are projected
// through the MCP result adapter (dsh-mcp-client), which durably saves
// screenshots as image attachments when the model route declares image input.
import { createMcpToolDefinition } from '@deepseek-ai/dsh-mcp-client';
import { isSelfTarget } from './approvals.js';

const PREFIX = 'sky__';
const IMAGE_TYPES = new Set(['image/png', 'image/jpeg', 'image/webp', 'image/gif']);

/** RPCs that send input; they can never target the Harness itself. */
const INPUT_RPCS = new Set(['click', 'click_element', 'drag', 'scroll', 'type_text', 'press_key', 'set_value', 'perform_secondary_action', 'activate_window', 'batch']);
/** Coordinate RPCs: the helper does not validate snapshots or coverage, so the plugin guards them. */
const COORD_RPCS = new Set(['click', 'drag', 'scroll']);
const SECONDARY_ACTIONS = ['Raise', 'Scroll Up', 'Scroll Down', 'Scroll Left', 'Scroll Right', 'Expand', 'Collapse'];

function normalizeSecondaryAction(value) {
  const compact = value.toLowerCase().replace(/[\s_-]+/g, '');
  for (const action of SECONDARY_ACTIONS) {
    if (action.toLowerCase().replace(/\s+/g, '') === compact) return action;
  }
  return value.trim();
}

// Per-window registry of the latest get_window_state snapshot, used to refuse
// coordinate input measured from unknown or superseded screenshots and points
// that were covered by another captured window at snapshot time.
const SNAPSHOT_LIMIT = 64;
const snapshots = new Map(); // `${app}\0${id}` -> { shots: Map<shotId, {shot, occluders}>, ts, owner }
const windowKey = (w) => `${w.app}\0${w.id}`;
const ownerKey = (exec) => (exec?.agent?.id === undefined ? 'default' : String(exec.agent.id));
const isRect = (s) => s !== null && typeof s === 'object'
  && Number.isFinite(s.originX) && Number.isFinite(s.originY)
  && Number.isFinite(s.width) && Number.isFinite(s.height)
  && s.width >= 64 && s.height >= 64;

function rememberSnapshot(window, result, owner) {
  const shots = Array.isArray(result?.screenshots) ? result.screenshots : [];
  const map = new Map();
  for (const shot of shots) {
    if (typeof shot?.id !== 'string' || shot.id === '') continue;
    map.set(shot.id, { shot, occluders: [] });
  }
  if (map.size === 0) return; // text-only recheck: keep the previous screenshot snapshot valid
  for (const entry of map.values()) {
    if (typeof entry.shot.zIndex !== 'number') continue;
    const mainArea = (entry.shot.width ?? 0) * (entry.shot.height ?? 0);
    // Only window-sized siblings count as coverage; small higher-z siblings are
    // the target's own menus/combobox popups and must stay clickable.
    entry.occluders = shots.filter((other) => other !== entry.shot
      && isRect(other) && typeof other.zIndex === 'number' && other.zIndex > entry.shot.zIndex
      && other.width >= 256 && other.height >= 256
      && other.width * other.height >= 0.15 * mainArea);
  }
  snapshots.delete(windowKey(window));
  snapshots.set(windowKey(window), { shots: map, ts: Date.now(), owner });
  while (snapshots.size > SNAPSHOT_LIMIT) snapshots.delete(snapshots.keys().next().value);
}

function guardPoints(toolName, window, shotId, points, owner) {
  if (typeof shotId !== 'string' || shotId === '') {
    throw new TypeError(`${toolName} requires the screenshotId of the latest sky__get_window_state snapshot: coordinates are snapshot-relative and the helper does not validate them.`);
  }
  const snapshot = snapshots.get(windowKey(window));
  const entry = snapshot?.owner === owner ? snapshot.shots.get(shotId) : undefined;
  if (entry === undefined) {
    throw new Error(`screenshotId "${shotId}" is not from the latest snapshot of window ${window.id}; call sky__get_window_state on this window again and use the returned screenshotId.`);
  }
  // A downscaled/cropped snapshot has its own px→logical factor; the engine
  // applies the same conversion. The occlusion check has to use it too, or a
  // 0.5-scaled snapshot would test the wrong point.
  const scale = Number.isFinite(entry.shot.scale) && entry.shot.scale > 0 ? entry.shot.scale : 1;
  for (const [px, py] of points) {
    if (!Number.isFinite(px) || !Number.isFinite(py)) continue;
    const absX = entry.shot.originX + px / scale;
    const absY = entry.shot.originY + py / scale;
    for (const oc of entry.occluders) {
      if (absX >= oc.originX && absX < oc.originX + oc.width && absY >= oc.originY && absY < oc.originY + oc.height) {
        throw new Error(`point (${px},${py}) was covered by captured overlay "${oc.id}" (zIndex ${oc.zIndex}) when the snapshot was taken; call sky__activate_window on the target window first, then take a fresh sky__get_window_state snapshot before sending input.`);
      }
    }
  }
  return entry;
}

function guardCoordinateInput(toolName, routed, owner) {
  const params = routed.params;
  const window = params?.window;
  if (window === undefined) return;
  const points = [[params.x ?? params.from_x, params.y ?? params.from_y]];
  if (params.to_x !== undefined && params.to_y !== undefined) points.push([params.to_x, params.to_y]);
  guardPoints(toolName, window, params.screenshotId, points, owner);
}

const BATCH_MAX = 50;
const BATCH_TYPES = ['click', 'drag', 'scroll', 'key', 'text', 'wait', 'activate'];

/** Validate one batch step and turn it into (rpc, params) without a window. */
function batchStep(raw, index) {
  const where = `actions[${index}]`;
  if (raw === null || typeof raw !== 'object') throw new TypeError(`${where} must be an object`);
  const type = typeof raw.type === 'string' ? raw.type.trim().toLowerCase() : '';
  if (!BATCH_TYPES.includes(type)) {
    throw new TypeError(`${where}.type must be one of: ${BATCH_TYPES.join(', ')}`);
  }
  if (type === 'click') {
    const clickCount = raw.click_count === undefined ? 1 : asAtLeast1(raw.click_count, `${where}.click_count`);
    const mouseButton = raw.mouse_button ?? 'left';
    if (!['left', 'right', 'middle'].includes(mouseButton)) throw new TypeError(`${where}.mouse_button must be left, right or middle`);
    if (raw.element_index !== undefined) {
      return { rpc: 'click_element', params: { element_index: asElementIndex(raw.element_index, `${where}.element_index`), click_count: clickCount, mouse_button: mouseButton } };
    }
    if (raw.x !== undefined && raw.y !== undefined) {
      return { rpc: 'click', params: { x: asFinite(raw.x, `${where}.x`), y: asFinite(raw.y, `${where}.y`), click_count: clickCount, mouse_button: mouseButton } };
    }
    throw new TypeError(`${where} needs element_index or x and y`);
  }
  if (type === 'drag') {
    return { rpc: 'drag', params: {
      from_x: asFinite(raw.from_x, `${where}.from_x`), from_y: asFinite(raw.from_y, `${where}.from_y`),
      to_x: asFinite(raw.to_x, `${where}.to_x`), to_y: asFinite(raw.to_y, `${where}.to_y`),
    } };
  }
  if (type === 'scroll') {
    return { rpc: 'scroll', params: {
      x: Math.round(asFinite(raw.x, `${where}.x`)), y: Math.round(asFinite(raw.y, `${where}.y`)),
      scrollX: Math.round(asFinite(raw.scrollX ?? 0, `${where}.scrollX`)),
      scrollY: Math.round(asFinite(raw.scrollY ?? 0, `${where}.scrollY`)),
    } };
  }
  if (type === 'key') {
    const key = typeof raw.key === 'string' ? raw.key.split('+').map((x) => x.trim()).filter((x) => x !== '').join('+') : '';
    if (key === '') throw new TypeError(`${where}.key is required`);
    return { rpc: 'press_key', params: { key } };
  }
  if (type === 'text') {
    if (typeof raw.text !== 'string' || raw.text === '') throw new TypeError(`${where}.text is required`);
    return { rpc: 'type_text', params: { text: raw.text } };
  }
  if (type === 'wait') {
    const ms = raw.ms === undefined ? 100 : asFinite(raw.ms, `${where}.ms`);
    if (ms < 0 || ms > 5000) throw new TypeError(`${where}.ms must be between 0 and 5000`);
    return { rpc: '__wait', params: { ms } };
  }
  return { rpc: '__activate', params: {} };
}

function batchSteps(value) {
  if (!Array.isArray(value) || value.length === 0) throw new TypeError('actions must be a non-empty array');
  if (value.length > BATCH_MAX) throw new TypeError(`actions supports at most ${BATCH_MAX} steps`);
  return value.map(batchStep);
}

const WINDOW_SCHEMA = {
  type: 'object',
  properties: {
    app: { type: 'string', description: 'Process address, e.g. "process:C:\\Windows\\System32\\notepad.exe" (windows[].app from a listing, not the list_apps id).' },
    id: { type: 'integer', minimum: 0, description: 'Raw HWND window id.' },
    title: { type: 'string', description: 'Optional title hint.' },
  },
  required: ['app', 'id'],
  additionalProperties: false,
};

function asWindow(value, label = 'window') {
  if (value === null || typeof value !== 'object') {
    throw new TypeError(`${label}.app must be a non-empty string and ${label}.id must be an integer >= 0`);
  }
  const record = value;
  if (typeof record.app !== 'string' || record.app.trim() === '') {
    throw new TypeError(`${label}.app must be a non-empty string and ${label}.id must be an integer >= 0`);
  }
  let id = record.id;
  if (typeof id === 'string' && /^\d+$/.test(id.trim())) id = Number(id.trim());
  if (!Number.isInteger(id) || id < 0) {
    throw new TypeError(`${label}.app must be a non-empty string and ${label}.id must be an integer >= 0`);
  }
  const out = { app: record.app, id };
  if (typeof record.title === 'string') out.title = record.title;
  return out;
}

function asElementIndex(value, label = 'element_index') {
  if (typeof value === 'string' && /^\d+$/.test(value.trim())) value = Number(value.trim());
  if (!Number.isInteger(value) || value < 0) throw new TypeError(`${label} must be an integer >= 0`);
  return value;
}

function asFinite(value, label) {
  if (typeof value !== 'number' || !Number.isFinite(value)) throw new TypeError(`${label} must be a finite number`);
  return value;
}

function asAtLeast1(value, label) {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 1) throw new TypeError(`${label} must be >= 1`);
  return value;
}

function dataUrlToImage(value) {
  const match = /^data:([^;,]+);base64,([\s\S]*)$/.exec(String(value ?? ''));
  if (match === null || !IMAGE_TYPES.has(match[1])) return undefined;
  return { type: 'image', mimeType: match[1], data: match[2] };
}

function textResult(text, structured) {
  const out = { content: [{ type: 'text', text }] };
  if (structured !== undefined) out.structuredContent = structured;
  return out;
}

function windowStateResult(result, args) {
  const lines = [];
  const window = result?.window ?? {};
  lines.push(`window: app=${window.app ?? '?'} id=${window.id ?? '?'} title=${JSON.stringify(window.title ?? '')}`);
  const screenshots = Array.isArray(result?.screenshots) ? result.screenshots : [];
  lines.push(`screenshots: ${screenshots.length} (pass the main screenshotId to coordinate tools)`);
  const content = [];
  const attachImage = (shot) => {
    const image = dataUrlToImage(shot.url);
    if (image === undefined) { content.push({ type: 'text', text: `[screenshot ${shot.id}: unrecognized image payload, omitted]` }); return false; }
    content.push(image);
    return true;
  };
  // Attach the primary (lowest zIndex, largest) frame and any other real window
  // frames; skip cursor/decorative overlay fragments that would burn image tokens.
  const ranked = screenshots
    .map((shot, index) => ({ shot, index }))
    .filter(({ shot }) => isRect(shot) && shot.hasContent !== false)
    .sort((a, b) => (a.shot.zIndex ?? 0) - (b.shot.zIndex ?? 0) || (b.shot.width * b.shot.height) - (a.shot.width * a.shot.height));
  const seenPayloads = new Set();
  let attached = 0;
  for (const { shot } of ranked) {
    if (attached >= 4) break;
    const fingerprint = `${shot.width}x${shot.height}:${String(shot.url).length}`;
    if (seenPayloads.has(fingerprint)) continue;
    seenPayloads.add(fingerprint);
    if (attachImage(shot)) attached += 1;
  }
  if (attached === 0 && screenshots.length > 0) attachImage(screenshots[0]);
  for (const shot of screenshots) {
    const scale = Number.isFinite(shot.scale) && shot.scale !== 1 ? ` scale=${shot.scale}` : '';
    lines.push(`  ${shot.id}: origin=(${shot.originX},${shot.originY}) size=${shot.width}x${shot.height} zIndex=${shot.zIndex}${scale} hasContent=${shot.hasContent ?? (isRect(shot) && shot.url ? true : false)}${isRect(shot) ? '' : ' (overlay fragment, not attached)'}`);
  }
  const accessibility = result?.accessibility ?? {};
  if (typeof accessibility.tree === 'string' && accessibility.tree !== '') {
    lines.push('accessibility tree (element indexes are valid only for this snapshot):');
    lines.push(accessibility.tree);
  } else if (args?.include_text === true) {
    lines.push('accessibility tree: none returned. Many UWP/modern apps (e.g. Notepad) expose no usable UIA tree; drive them with the screenshot + coordinate tools after activating the window, and do not keep re-requesting the tree.');
  }
  if (accessibility.focused !== undefined) lines.push(`focused: ${JSON.stringify(accessibility.focused)}`);
  if (typeof accessibility.selected_text === 'string' && accessibility.selected_text !== '') {
    lines.push(`selected_text: ${accessibility.selected_text}`);
  }
  if (accessibility.selected_elements !== undefined) lines.push(`selected_elements: ${JSON.stringify(accessibility.selected_elements)}`);
  if (typeof accessibility.document_text === 'string' && accessibility.document_text !== '') {
    lines.push(`document_text: ${accessibility.document_text}`);
  }
  content.unshift({ type: 'text', text: lines.join('\n') });
  const out = { content };
  out.structuredContent = {
    window,
    screenshots: screenshots.map(({ url, ...rest }) => ({
      ...rest,
      hasContent: rest.hasContent ?? (isRect(rest) && typeof url === 'string' && url !== ''),
    })),
    accessibility,
  };
  return out;
}

const SPEC = [
  {
    rpc: 'list_apps',
    tool: 'list_apps',
    description: 'List installed and running apps on this Windows host, with their app ids, running state, window counts and usage. Returns immediately; does not wait for app state. Use the id to launch apps; use windows[].app from the result (or from sky__list_windows) for actions.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
    params: () => ({}),
    project: (result) => textResult(
      (Array.isArray(result) ? result : []).map((app) => {
        const running = app.isRunning ?? app.running ?? false;
        const windows = Array.isArray(app.windows) ? app.windows.length : app.windowCount;
        return `${app.id}${running ? ' (running)' : ''} | ${app.displayName} | windows=${windows ?? '?'} | useCount=${app.useCount ?? '-'} | lastUsed=${app.lastUsedDate ?? '-'}`;
      }).join('\n') || '(no apps)',
      { apps: result },
    ),
  },
  {
    rpc: 'list_windows',
    tool: 'list_windows',
    description: 'List all top-level windows: {app, id, title}. The app value (process:...) plus the integer id (HWND) is the handle every other tool takes as window.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
    params: () => ({}),
    project: (result) => textResult(
      (Array.isArray(result) ? result : []).map((w) => `${w.id} | app=${w.app} | title=${JSON.stringify(w.title ?? '')}`).join('\n') || '(no windows)',
      { windows: result },
    ),
  },
  {
    rpc: 'get_window',
    tool: 'get_window',
    description: 'Return one window handle {app, id, title}. id is the raw HWND (never a list index).',
    inputSchema: {
      type: 'object',
      properties: { app: { type: 'string' }, id: { type: 'integer', minimum: 0 } },
      required: ['id'],
      additionalProperties: false,
    },
    params: (args) => {
      if (args === null || typeof args !== 'object') throw new TypeError('get_window input must be an object');
      const idRaw = args.id;
      const id = typeof idRaw === 'string' && /^\d+$/.test(idRaw.trim()) ? Number(idRaw.trim()) : idRaw;
      if (!Number.isInteger(id) || id < 0) throw new TypeError('id must be an integer >= 0');
      const out = { id };
      if (typeof args.app === 'string' && args.app !== '') out.app = args.app;
      return out;
    },
    project: (result) => textResult(JSON.stringify(result), { window: result }),
  },
  {
    rpc: 'launch_app',
    tool: 'launch_app',
    description: 'Launch an app by id from sky__list_apps (or by path). Does not wait for a window; poll sky__list_windows. If the app is already running this is an idempotent no-op.',
    inputSchema: {
      type: 'object',
      properties: { app: { type: 'string', description: 'App id from sky__list_apps.' } },
      required: ['app'],
      additionalProperties: false,
    },
    params: (args) => {
      const app = typeof args?.app === 'string' ? args.app.trim() : '';
      if (app === '') throw new TypeError('app is required');
      return { app };
    },
    project: (result, args) => textResult(`launched ${args.app}; poll sky__list_windows until its window appears`, { result: result ?? null }),
  },
  {
    rpc: 'get_window_state',
    tool: 'get_window_state',
    description: 'Capture one window in a single call: JPEG screenshot(s) (screenshotId, origin, size, zIndex, hasContent, scale) and the window-relative UIA accessibility tree with element indexes and window-relative logical bounds. Both come from the same moment, so coordinates and element indexes are consistent. Pass include_screenshot=false for a text-only recheck after an action (cheaper). Pass scale (0.1-1) to capture a smaller JPEG, and region {x,y,width,height} in logical window px to capture just one panel; coordinates measured on that smaller/cropped image are still accepted by the input tools. Element indexes and cached bounds are valid only for this snapshot; recapture before acting again.',
    inputSchema: {
      type: 'object',
      properties: {
        window: WINDOW_SCHEMA,
        include_screenshot: { type: 'boolean', default: true },
        include_text: { type: 'boolean', default: false, description: 'Include the UIA accessibility tree (element indexes).' },
        scale: { type: 'number', minimum: 0.1, maximum: 1, description: 'JPEG px per logical window px (default 1). 0.5 quarters the image tokens.' },
        region: {
          type: 'object',
          description: 'Crop in logical window-relative px. Omit for the full window.',
          properties: {
            x: { type: 'number' }, y: { type: 'number' },
            width: { type: 'number' }, height: { type: 'number' },
          },
          required: ['x', 'y', 'width', 'height'],
          additionalProperties: false,
        },
      },
      required: ['window'],
      additionalProperties: false,
    },
    params: (args) => {
      const window = asWindow(args?.window);
      const includeScreenshot = args?.include_screenshot !== false;
      const includeText = args?.include_text === true;
      if (!includeScreenshot && !includeText) throw new TypeError('get_window_state requires include_screenshot or include_text');
      const out = { window, include_screenshot: includeScreenshot, include_text: includeText };
      if (args?.scale !== undefined) {
        const scale = asFinite(args.scale, 'scale');
        if (scale < 0.1 || scale > 1) throw new TypeError('scale must be between 0.1 and 1');
        out.scale = scale;
      }
      if (args?.region !== undefined) {
        const r = args.region;
        if (r === null || typeof r !== 'object') throw new TypeError('region must be an object with x, y, width and height');
        out.region = {
          x: Math.round(asFinite(r.x, 'region.x')),
          y: Math.round(asFinite(r.y, 'region.y')),
          width: Math.round(asFinite(r.width, 'region.width')),
          height: Math.round(asFinite(r.height, 'region.height')),
        };
        if (out.region.width < 8 || out.region.height < 8) throw new TypeError('region.width and region.height must be >= 8');
      }
      return out;
    },
    project: (result, args) => windowStateResult(result, args),
  },
  {
    rpc: 'activate_window',
    tool: 'activate_window',
    description: 'Bring a window to the foreground. Call this immediately before any coordinate click, drag, scroll, type_text, or press_key on that window: input is delivered to the foreground window, and coverage detected at snapshot time is refused until you re-activate and re-snapshot.',
    inputSchema: { type: 'object', properties: { window: WINDOW_SCHEMA }, required: ['window'], additionalProperties: false },
    params: (args) => ({ window: asWindow(args?.window) }),
    project: () => textResult('ok'),
  },
  {
    rpc: 'click', // routed to click_element or click below
    tool: 'click',
    description: 'Click inside a window. Preferred: element_index from the latest sky__get_window_state tree (the helper resolves its cached center and waits for the UI to settle). For coordinates pass the screenshotId of the screenshot you measured from; points covered by a higher-zIndex captured window are refused. click_count >= 2 multi-clicks; mouse_button: left|right|middle.',
    inputSchema: {
      type: 'object',
      properties: {
        window: WINDOW_SCHEMA,
        element_index: { description: 'Index from the latest accessibility tree (integer >= 0).' },
        click_count: { type: 'number', minimum: 1, default: 1 },
        mouse_button: { type: 'string', enum: ['left', 'right', 'middle'], default: 'left' },
        x: { type: 'number', description: 'Snapshot-relative x; requires y and screenshotId.' },
        y: { type: 'number', description: 'Snapshot-relative y; requires x and screenshotId.' },
        screenshotId: { type: 'string', description: 'Id of the screenshot the coordinates were measured from (from the latest sky__get_window_state). Required for coordinate clicks.' },
      },
      required: ['window'],
      additionalProperties: false,
    },
    route: (args) => {
      const window = asWindow(args?.window);
      const clickCount = asAtLeast1(args?.click_count ?? 1, 'click_count');
      const mouseButton = args?.mouse_button ?? 'left';
      const hasElement = args?.element_index !== undefined && args.element_index !== null;
      if (hasElement) {
        return {
          rpc: 'click_element',
          params: { window, element_index: asElementIndex(args.element_index), click_count: clickCount, mouse_button: mouseButton },
        };
      }
      if (args?.x !== undefined && args?.y !== undefined) {
        const params = { window, x: asFinite(args.x, 'x'), y: asFinite(args.y, 'y'), click_count: clickCount, mouse_button: mouseButton };
        if (typeof args.screenshotId === 'string' && args.screenshotId !== '') params.screenshotId = args.screenshotId;
        return { rpc: 'click', params };
      }
      throw new TypeError('click requires either element_index or finite x and y coordinates');
    },
    project: () => textResult('ok'),
  },
  {
    rpc: 'drag',
    tool: 'drag',
    description: 'Drag between two snapshot-relative locations (from_x, from_y → to_x, to_y) with the left button.',
    inputSchema: {
      type: 'object',
      properties: {
        window: WINDOW_SCHEMA,
        from_x: { type: 'number' }, from_y: { type: 'number' },
        to_x: { type: 'number' }, to_y: { type: 'number' },
        screenshotId: { type: 'string', description: 'Id of the screenshot both points were measured from (from the latest sky__get_window_state).' },
      },
      required: ['window', 'from_x', 'from_y', 'to_x', 'to_y', 'screenshotId'],
      additionalProperties: false,
    },
    params: (args) => {
      const params = {
        window: asWindow(args?.window),
        from_x: asFinite(args?.from_x, 'from_x'),
        from_y: asFinite(args?.from_y, 'from_y'),
        to_x: asFinite(args?.to_x, 'to_x'),
        to_y: asFinite(args?.to_y, 'to_y'),
        screenshotId: typeof args?.screenshotId === 'string' ? args.screenshotId : '',
      };
      return params;
    },
    project: () => textResult('ok'),
  },
  {
    rpc: 'scroll',
    tool: 'scroll',
    description: 'Scroll at a snapshot-relative point. scrollX/scrollY are deltas (rounded to integers).',
    inputSchema: {
      type: 'object',
      properties: {
        window: WINDOW_SCHEMA,
        x: { type: 'number' }, y: { type: 'number' },
        scrollX: { type: 'number' }, scrollY: { type: 'number' },
        screenshotId: { type: 'string', description: 'Id of the screenshot the point was measured from (from the latest sky__get_window_state).' },
      },
      required: ['window', 'x', 'y', 'scrollX', 'scrollY', 'screenshotId'],
      additionalProperties: false,
    },
    params: (args) => {
      const params = {
        window: asWindow(args?.window),
        x: Math.round(asFinite(args?.x, 'x')),
        y: Math.round(asFinite(args?.y, 'y')),
        scrollX: Math.round(asFinite(args?.scrollX, 'scrollX')),
        scrollY: Math.round(asFinite(args?.scrollY, 'scrollY')),
        screenshotId: typeof args?.screenshotId === 'string' ? args.screenshotId : '',
      };
      return params;
    },
    project: () => textResult('ok'),
  },
  {
    rpc: 'type_text',
    tool: 'type_text',
    description: 'Type literal text (character-by-character via SendInput) into the focused control of the foreground window. Call sky__activate_window on the target window first or the text lands on whatever app is currently active. Do not use it for key combinations — use sky__press_key for those.',
    inputSchema: {
      type: 'object',
      properties: { window: WINDOW_SCHEMA, text: { type: 'string' } },
      required: ['window', 'text'],
      additionalProperties: false,
    },
    params: (args) => {
      const text = typeof args?.text === 'string' ? args.text : undefined;
      if (text === undefined) throw new TypeError('text is required');
      return { window: asWindow(args?.window), text };
    },
    project: () => textResult('ok'),
  },
  {
    rpc: 'press_key',
    tool: 'press_key',
    description: 'Press a key or combination into the foreground window; call sky__activate_window on the target window first. Keys: Enter, Escape, Tab, Backspace, Delete, Up, Down, Left, Right, F1–F24, space, or combinations joined with "+", e.g. "ctrl+s", "alt+Tab", "ctrl+shift+Escape".',
    inputSchema: {
      type: 'object',
      properties: { window: WINDOW_SCHEMA, key: { type: 'string' } },
      required: ['window', 'key'],
      additionalProperties: false,
    },
    params: (args) => {
      const key = typeof args?.key === 'string' ? args.key.split('+').map((x) => x.trim()).filter((x) => x !== '').join('+') : '';
      if (key === '') throw new TypeError('key is required');
      return { window: asWindow(args?.window), key };
    },
    project: () => textResult('ok'),
  },
  {
    rpc: 'batch',
    tool: 'batch',
    description: 'Run a sequence of input actions on one window in a single call. Steps are sent in order and each is the same operation as the matching single-action tool: {type:"click", x, y | element_index, click_count?, mouse_button?}, {type:"drag", from_x, from_y, to_x, to_y}, {type:"scroll", x, y, scrollX?, scrollY?}, {type:"key", key}, {type:"text", text}, {type:"wait", ms}, {type:"activate"}. Coordinate steps are all measured on the same screenshotId; the guard validates every point against that snapshot once, then the whole sequence runs without a fresh snapshot between steps. Use this for paint strokes, repeated clicks or a short measured choreography (at most 50 steps).',
    inputSchema: {
      type: 'object',
      properties: {
        window: WINDOW_SCHEMA,
        screenshotId: { type: 'string', description: 'Id of the screenshot every coordinate in this batch was measured from (required when any step uses x/y).' },
        actions: {
          type: 'array',
          minItems: 1,
          maxItems: BATCH_MAX,
          description: 'Ordered steps; see the tool description for the shape of each type.',
          items: { type: 'object', additionalProperties: true },
        },
      },
      required: ['window', 'actions'],
      additionalProperties: false,
    },
    project: (result) => textResult(`batch: ${result?.executed ?? 0} action(s) sent`, result ?? null),
  },
  {
    rpc: 'set_value',
    tool: 'set_value',
    description: 'Set the value of an editable element by element_index from the latest sky__get_window_state tree (UIA value pattern). Preferred over typing when the tree exposes the field.',
    inputSchema: {
      type: 'object',
      properties: {
        window: WINDOW_SCHEMA,
        element_index: { description: 'Integer >= 0 from the latest tree.' },
        value: { type: 'string' },
      },
      required: ['window', 'element_index', 'value'],
      additionalProperties: false,
    },
    params: (args) => ({
      window: asWindow(args?.window),
      element_index: asElementIndex(args?.element_index),
      value: typeof args?.value === 'string' ? args.value : String(args?.value ?? ''),
    }),
    project: () => textResult('ok'),
  },
  {
    rpc: 'perform_secondary_action',
    tool: 'perform_secondary_action',
    description: 'Invoke a secondary UIA action on an element by element_index from the latest tree. Supported actions are exactly: Raise, Scroll Up, Scroll Down, Scroll Left, Scroll Right, Expand, Collapse.',
    inputSchema: {
      type: 'object',
      properties: {
        window: WINDOW_SCHEMA,
        element_index: { description: 'Integer >= 0 from the latest tree.' },
        action: { type: 'string', enum: SECONDARY_ACTIONS, description: 'One of: Raise, Scroll Up, Scroll Down, Scroll Left, Scroll Right, Expand, Collapse. camelCase like "ScrollUp" is accepted.' },
      },
      required: ['window', 'element_index', 'action'],
      additionalProperties: false,
    },
    params: (args) => {
      const action = typeof args?.action === 'string' ? args.action.trim() : '';
      if (action === '') throw new TypeError('action is required');
      const normalized = normalizeSecondaryAction(action);
      if (!SECONDARY_ACTIONS.includes(normalized)) {
        throw new TypeError(`unsupported action "${action}"; expected one of: ${SECONDARY_ACTIONS.join(', ')}`);
      }
      return { window: asWindow(args?.window), element_index: asElementIndex(args?.element_index), action: normalized };
    },
    project: () => textResult('ok'),
  },
];

export function createToolDefinitions(inner, engine) {
  /**
   * Execute a measured sequence without a snapshot round-trip between steps.
   * The coordinate guard runs once, against the snapshot the caller measured
   * on; every later step is part of the same measured gesture.
   */
  async function runBatch(args, exec, publicName) {
    if (args === null || typeof args !== 'object') throw new TypeError('batch input must be an object');
    const window = asWindow(args.window);
    if (isSelfTarget(window.app, window.title)) {
      throw new TypeError('Computer Use cannot drive the DeepSeek Harness itself; target another application window.');
    }
    const steps = batchSteps(args.actions);
    const owner = ownerKey(exec);
    const shotId = typeof args.screenshotId === 'string' ? args.screenshotId : '';
    const points = [];
    for (const step of steps) {
      if (step.rpc === 'click') points.push([step.params.x, step.params.y]);
      else if (step.rpc === 'drag') points.push([step.params.from_x, step.params.from_y], [step.params.to_x, step.params.to_y]);
      else if (step.rpc === 'scroll') points.push([step.params.x, step.params.y]);
    }
    if (points.length > 0) guardPoints(publicName, window, shotId, points, owner);

    const sessionId = exec?.agent?.id === undefined ? undefined : String(exec.agent.id);
    const turnId = sessionId === undefined ? undefined : engine.turnProvider(sessionId);
    const transport = engine.ensureTransport();
    let executed = 0;
    for (let i = 0; i < steps.length; i += 1) {
      const step = steps[i];
      if (step.rpc === '__wait') {
        await new Promise((resolve) => setTimeout(resolve, step.params.ms));
        executed += 1;
        continue;
      }
      const meta = {
        sessionId,
        turnId,
        agent: exec?.agent,
        callId: exec?.callId,
        toolName: publicName,
        signal: exec?.signal,
      };
      try {
        const rpc = step.rpc === '__activate' ? 'activate_window' : step.rpc;
        const params = { window, ...step.params };
        // Coordinate steps must carry the same snapshot id so the helper maps
        // their pixels through the shot's crop/scale geometry.
        if (shotId !== '' && (step.rpc === 'click' || step.rpc === 'drag' || step.rpc === 'scroll')) {
          params.screenshotId = shotId;
        }
        await transport.request(rpc, params, meta);
      } catch (error) {
        const message = String(error?.message ?? error);
        throw new Error(`batch action ${i + 1}/${steps.length} (${step.rpc}) failed after ${executed} completed action(s): ${message}`);
      }
      executed += 1;
    }
    if (steps.some((step) => step.rpc !== '__wait')) snapshots.delete(windowKey(window));
    return { executed, total: steps.length };
  }

  const definitions = [];
  for (const spec of SPEC) {
    const publicName = `${PREFIX}${spec.tool}`;
    definitions.push(createMcpToolDefinition(inner, {
      name: publicName,
      rawName: spec.rpc,
      description: spec.description,
      inputSchema: spec.inputSchema,
      async call(args, exec) {
        if (spec.tool === 'batch') {
          const result = await runBatch(args, exec, publicName);
          return spec.project(result, args ?? {});
        }
        const routed = spec.route !== undefined ? spec.route(args) : { rpc: spec.rpc, params: spec.params(args) };
        const owner = ownerKey(exec);
        const inputWindow = routed.params?.window;
        if (INPUT_RPCS.has(routed.rpc) && inputWindow !== undefined && isSelfTarget(inputWindow.app, inputWindow.title)) {
          throw new TypeError('Computer Use cannot drive the DeepSeek Harness itself; target another application window.');
        }
        if (COORD_RPCS.has(routed.rpc)) guardCoordinateInput(publicName, routed, owner);
        const sessionId = exec?.agent?.id === undefined ? undefined : String(exec.agent.id);
        const turnId = sessionId === undefined ? undefined : engine.turnProvider(sessionId);
        const transport = engine.ensureTransport();
        let result;
        try {
          result = await transport.request(routed.rpc, routed.params, {
            sessionId,
            turnId,
            agent: exec?.agent,
            callId: exec?.callId,
            toolName: publicName,
            signal: exec?.signal,
          });
        } catch (error) {
          const message = String(error?.message ?? error);
          if (routed.rpc === 'launch_app' && /did not expose a targetable window/i.test(message)) {
            throw new Error(`${message} UWP/MSIX apps relaunch under a different identity (a package family name, not the exe path you launched): poll sky__list_windows for the new window instead of retrying the launch.`);
          }
          if (/multiple signatures/i.test(message)) {
            throw new Error(`${message} Windows owned by shared system processes (such as explorer.exe) cannot be addressed by the helper; operate through a normal top-level window from sky__list_windows instead.`);
          }
          throw error;
        }
        if (routed.rpc === 'get_window_state') {
          if (inputWindow !== undefined) rememberSnapshot(inputWindow, result, owner);
        } else if (INPUT_RPCS.has(routed.rpc) && inputWindow !== undefined) {
          // Any delivered input makes previously measured screenshot coordinates stale.
          snapshots.delete(windowKey(inputWindow));
        }
        return spec.project(result ?? null, args ?? {});
      },
    }));
  }
  return definitions;
}
