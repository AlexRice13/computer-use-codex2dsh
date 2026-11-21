---
name: computer-use
description: >-
  Operate Windows desktop applications (inspect windows and UIA controls, take
  window screenshots, launch apps, click, drag, scroll, and type) through the
  codex-computer-use MCP server (`computer-use_codex-computer-use`). Activate
  this skill whenever the user asks to view, launch, automate, or interact with
  desktop GUI apps on Windows.
---

# Windows Computer Use (`codex-computer-use`)

This plugin exposes 15 `sky__*` MCP tools on server `computer-use_codex-computer-use`, backed by `codex-computer-use.exe`.

## Standard Operating Workflow

1. **Discover Windows / Apps**
   - Call `sky__list_windows` to find open top-level windows (`{ app, id, title }`).
   - If the target app isn't open, call `sky__launch_app` (accepts an `id` from `sky__list_apps`, a friendly name such as `"画图"`, `"记事本"`, `"计算器"`, or an absolute `.exe` path), then poll `sky__list_windows` until its window appears.
   - Always pass `{ app, id }` from `windows[]` (where `app` is `process:...` or an AUMID and `id` is the integer HWND) — never pass `list_apps[].id` as `window.app`.

2. **Snapshot State Before Acting**
   - Call `sky__get_window_state({ window, include_screenshot: true, include_text: true })`.
   - `element_index` values in the UIA tree and `screenshotId` in `screenshots[]` belong **only** to that snapshot and expire once input is sent or the window changes.
   - For modern/UWP apps that return an empty UIA tree (e.g., Notepad or Paint), stop requesting `include_text: true` and drive them using screenshot coordinates (`x`, `y`, `screenshotId`).

3. **Foreground Activation & Input**
   - Call `sky__activate_window({ window })` immediately before `sky__click` (coordinate mode), `sky__drag`, `sky__scroll`, `sky__type_text`, `sky__press_key`, or `sky__batch`.
   - Prefer `element_index` (`sky__click` with `element_index`, `sky__set_value`, `sky__perform_secondary_action`) whenever the UIA tree exposes the target control.
   - For multi-step coordinate sequences (such as drawing strokes in Paint or clicking several known points), use `sky__batch` with a single `screenshotId`.

4. **Verify Outcome & Exit Computer Use (`sky__close`)**
   - Re-check with `sky__get_window_state` to confirm the action succeeded.
   - **Always call `sky__close` as soon as you finish your Computer Use steps in the turn** (before sending your final reply to the user). This sends `end_turn` + `close` to `codex-computer-use.exe` so the floating status pill and agent cursor exit immediately while preserving per-session app approvals.

## Safety & Backend Rules (`codex-computer-use.exe`)

- **Do NOT send `Escape` via `sky__press_key`**: `codex-computer-use.exe`'s low-level keyboard hook treats any `VK_ESCAPE` (including synthetic `SendInput`) as a user Escape interrupt and aborts the turn. Dismiss dialogs/menus by clicking Cancel/Close buttons instead.
- **Physical Escape Interrupt**: If a tool reports that the user stopped Computer Use with the physical Escape key, stop calling `sky__*` tools immediately for the rest of the turn.
- **App Approvals & Protected Apps**: `Antigravity` and `WindowsTerminal` are blocked by `denyApps`. Other apps prompt the user once per session via a native TopMost Windows confirmation dialog unless pre-listed in `autoApprove` in `~/.gemini/config/plugins/computer-use/config.json`. Never retry an app after the user declines approval.
