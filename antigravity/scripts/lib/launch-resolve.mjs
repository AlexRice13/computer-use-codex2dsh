// Resolves friendly/bare app names ("画图", "Paint", "mspaint", "记事本", "calc")
// and packaged WindowsApps exe paths into canonical Start-Menu AUMIDs so that
// codex-computer-use.exe's strict launch_app accepts them cleanly.
import { spawnSync } from 'node:child_process';

let cachedStartApps = null;
let cachedAt = 0;
const CACHE_TTL_MS = 60_000;

const ALIASES = [
  ['mspaint', 'microsoft.paint_'],
  ['paint', 'microsoft.paint_'],
  ['画图', 'microsoft.paint_'],
  ['notepad', 'microsoft.windowsnotepad_'],
  ['记事本', 'microsoft.windowsnotepad_'],
  ['calc', 'microsoft.windowscalculator_'],
  ['calculator', 'microsoft.windowscalculator_'],
  ['计算器', 'microsoft.windowscalculator_'],
];

const squash = (s) => String(s).replace(/\s+/g, '').toLowerCase();

function looksLikeAumid(s) {
  return (s.includes('_') && s.includes('!')) || s.toLowerCase().startsWith('windows.');
}

function hasDrivePrefix(s) {
  return /^[A-Za-z]:[\\/]/.test(s);
}

function loadStartApps() {
  const now = Date.now();
  if (cachedStartApps !== null && now - cachedAt < CACHE_TTL_MS) return cachedStartApps;
  if (process.platform !== 'win32') return [];
  try {
    const res = spawnSync('powershell.exe', [
      '-NoProfile',
      '-NonInteractive',
      '-WindowStyle',
      'Hidden',
      '-Command',
      '[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-StartApps | Select-Object Name, AppID | ConvertTo-Json -Compress',
    ], { encoding: 'utf8', windowsHide: true, timeout: 5000 });
    if (res.status === 0 && res.stdout.trim() !== '') {
      const parsed = JSON.parse(res.stdout.trim());
      const arr = Array.isArray(parsed) ? parsed : [parsed];
      cachedStartApps = arr
        .filter((x) => x && typeof x.Name === 'string' && typeof x.AppID === 'string')
        .map((x) => ({ display: x.Name.trim(), id: x.AppID.trim() }));
      cachedAt = now;
      return cachedStartApps;
    }
  } catch {}
  return cachedStartApps ?? [];
}

function packageFamily(dir) {
  const idx = dir.indexOf('_');
  const pubIdx = dir.lastIndexOf('__');
  if (idx <= 0 || pubIdx < 0) return undefined;
  const name = dir.slice(0, idx);
  const publisher = dir.slice(pubIdx + 2);
  if (!name || !publisher) return undefined;
  return `${name}_${publisher}`.toLowerCase();
}

function aumidForPackagedExe(exePath) {
  const parts = exePath.split(/[\\/]/);
  const waIdx = parts.findIndex((p) => p.toLowerCase() === 'windowsapps');
  if (waIdx < 0 || waIdx + 1 >= parts.length) return undefined;
  const family = packageFamily(parts[waIdx + 1]);
  if (!family) return undefined;
  for (const { id } of loadStartApps()) {
    if (id.toLowerCase().startsWith(family) && id.includes('!')) return id;
  }
  return undefined;
}

export function resolveLaunchTarget(rawApp) {
  const app = typeof rawApp === 'string' ? rawApp.trim() : '';
  if (app === '' || /^\d+$/.test(app)) return app;
  const lower = app.toLowerCase();
  if (lower.startsWith('shell:') || lower.startsWith('app-user-model-id:')) return app;

  if (hasDrivePrefix(app) || app.startsWith('\\\\')) {
    return aumidForPackagedExe(app) ?? app;
  }
  if (looksLikeAumid(app) || app.includes('\\') || app.includes('/')) {
    return app;
  }

  // Bare name ("画图", "Paint", "mspaint", "记事本", "calc", etc.)
  const token = lower.endsWith('.exe') ? lower.slice(0, -4) : lower;
  const compact = squash(token);
  const aliasEntry = ALIASES.find(([name]) => squash(name) === compact);
  const aliasPrefix = aliasEntry?.[1];

  const entries = loadStartApps();
  if (aliasPrefix) {
    const byAlias = entries.find(({ id }) => id.toLowerCase().startsWith(aliasPrefix) && id.includes('!'));
    if (byAlias) return byAlias.id;
  }
  for (const { display, id } of entries) {
    const dl = display.toLowerCase();
    if (dl === token || squash(dl) === compact) return id;
  }
  for (const { id } of entries) {
    if (!id.includes('!')) continue;
    const idl = id.toLowerCase();
    const family = idl.split('!')[0] ?? '';
    const pkgName = family.split('_')[0] ?? '';
    const tail = pkgName.split('.').pop() ?? '';
    if (tail && (tail === token || (token.length >= 4 && tail.endsWith(token)))) {
      return id;
    }
  }
  return app;
}
