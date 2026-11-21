#!/usr/bin/env node
// Antigravity Stop lifecycle hook: automatically shuts down any active
// codex-computer-use.exe helper when the agent turn finishes so the status pill
// and blue agent cursor never stay stuck on the user's desktop.
import { existsSync, readdirSync, readFileSync, unlinkSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { loadMergedConfig } from './server.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_CONFIG = path.resolve(HERE, '..', 'config.json');

function drainStdin() {
  return new Promise((resolve) => {
    process.stdin.resume();
    process.stdin.on('end', resolve);
    process.stdin.on('error', resolve);
    setTimeout(resolve, 200).unref();
  });
}

function releaseOne(markerPath) {
  return new Promise((resolve) => {
    let info;
    try {
      info = JSON.parse(readFileSync(markerPath, 'utf8'));
    } catch {
      resolve();
      return;
    }
    if (!info || typeof info.pipe !== 'string') {
      try { unlinkSync(markerPath); } catch {}
      resolve();
      return;
    }
    let settled = false;
    const done = () => {
      if (settled) return;
      settled = true;
      resolve();
    };
    const timer = setTimeout(() => {
      try { sock.destroy(); } catch {}
      done();
    }, 4500);
    timer.unref();
    const sock = net.connect(info.pipe, () => {
      try { sock.write('release\n'); } catch { done(); }
    });
    sock.on('data', () => {
      clearTimeout(timer);
      try { sock.end(); } catch {}
      done();
    });
    sock.on('end', () => { clearTimeout(timer); done(); });
    sock.on('error', () => {
      clearTimeout(timer);
      // Stale marker from a dead PID
      try { unlinkSync(markerPath); } catch {}
      done();
    });
  });
}

async function run() {
  await drainStdin();
  try {
    const config = loadMergedConfig(DEFAULT_CONFIG);
    const activeDir = path.join(config.codexHome, 'active-servers');
    if (existsSync(activeDir)) {
      const files = readdirSync(activeDir)
        .filter((f) => f.endsWith('.json'))
        .map((f) => path.join(activeDir, f));
      if (files.length > 0) {
        await Promise.allSettled(files.map(releaseOne));
      }
    }
  } catch {}
  process.stdout.write('{}\n');
}

run().catch(() => {
  process.stdout.write('{}\n');
});
