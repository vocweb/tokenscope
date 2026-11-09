# ADR 0001: Hidden panels may show stale data in exchange for idle CPU

Date: 2026-11-09
Status: accepted
Scope: low-cpu-idle effort, spec Decision 9

## Context

TokenScope sits in the menu bar all day. Before this effort it burned about
15% CPU while fully idle: panel hidden, no agent running. The backend pushed
a fresh dashboard to the WebView on every tick and the frontend re-rendered
it, even though nobody could see it. Laptop users felt heat and battery
drain from an app whose job is passive aggregation.

## Decision

When both the popover and the detached window are hidden, the panel is
allowed to go stale. The tray number becomes the source of truth while
hidden. Opening the panel always fetches the latest dashboard (existing
focus refetch), so the staleness is never visible in normal use.

## What stays always fresh

- Tray number and tooltip: updated on every tick where the today token
  total moved (`decide_refresh` in `src-tauri/src/parser.rs`, tray path in
  `src-tauri/src/lib.rs`).
- Panel content on open: focus refetch in `src/App.tsx` reloads the
  dashboard the instant the popover gains focus, and bumps the count-up
  animation so the open feels live.
- Panel while open: fast 30s full ticks, filesystem watcher, and event
  pushes all behave exactly as before. Nothing about the visible live
  experience changed.

## Guards against hiding real changes

A hidden tick only skips work when nothing moved. The dirty check
(`refresh_decision`) emits when any of these moved, not just tokens:

- new log data ingested (dirty flag from the watcher or scan),
- today token total,
- estimated cost (covers the known risk where a price refresh changes
  cost while token counts stay flat),
- plan limits rows.

Poll structure while hidden (`tick_kind` in `src-tauri/src/lib.rs`): fast
30s ticks keep running but go quiet, re-scanning only remembered files with
no directory listing. Every fourth tick (120s sparse deadline) runs a full
scan so brand-new files the watcher missed are still discovered within
about two minutes. The filesystem watcher remains the primary change
signal; the sparse poll is a fallback, not the driver. A new MCP server or
skill install therefore appears within minutes at worst, usually in
seconds via the watcher.

## Deliberately NOT changed

- Price table cadence: daily reload, untouched.
- Plan limits cadence: 15 minute reload, untouched.
- Manual Refresh from the tray menu: still bypasses all quiet gates
  (subject only to the pre-existing 30s anti-double-click cooldown), so
  users always have an escape hatch when they distrust the numbers.
- Quit checkpoint: `flush()` on exit still persists ingested data, so the
  next launch does not re-read everything.

## Reversibility

Reverting means: emit on every tick again, re-render on every push, and
run full scans at the fast cadence while hidden. That is a small,
localized change (the three gates named above), but it reinstates the
original 15% idle CPU, so reversal should require re-running the
acceptance measurement (5 minute idle average under 1% CPU on the same
machine and conditions) and recording the new numbers here.

## Acceptance pointer

Idle measurement (Activity Monitor, 5 minutes, panel closed, no agent
running, before/after on the same machine) is tracked in ticket 07 and is
owned by a human run, not by this file.
