# Tokenscope

**English** · [中文](README-zh.md)

<a href="https://www.producthunt.com/products/tokenscope-2?embed=true&amp;utm_source=badge-featured&amp;utm_medium=badge&amp;utm_campaign=badge-tokenscope-2" target="_blank" rel="noopener noreferrer"><img alt="Tokenscope - MacOS menu-bar dashboard for Claude CLI token usage | Product Hunt" width="250" height="54" src="https://api.producthunt.com/widgets/embed-image/v1/featured.svg?post_id=1165012&amp;theme=light&amp;t=1780816780292"></a>

A **menu-bar / system-tray app for macOS and Windows** that shows your AI coding agents' **daily token usage, estimated cost, and per-model / per-agent / MCP / Skill breakdown** — **Claude Code, Codex CLI, opencode, Oh My Pi and pi** in one place.

Stack: **Tauri 2 + React + TypeScript** (frontend) / **Rust** (data layer).

![Tokenscope panel (dark / light)](docs/screenshot.png)

## What it does

- Shows today's token count next to the menu-bar icon (e.g. `⬡ 14.00M`)
- Click to open the panel: **5H / Day / Week / Month** toggle — `5H` is the *rolling* last-5-hours window subscription plans meter their session limits in (Day/Week/Month stay calendar-based)
- Metrics: total tokens (input/output), estimated cost, requests / sessions
- Four breakdowns: **by agent** (Claude Code / Codex / opencode / Oh My Pi / pi) / **by model** / **by MCP call** / **by Skill call**
- **Plan limits**: the 5-hour / weekly / monthly windows the *provider* meters (opencode Go/Zen, Claude), with % used and reset countdowns — fetched live, not inferred from logs
- Cost donut (hover for a single model), year-long activity heatmap
- **Popover + real window, at the same time** (tray menu → *Open in Window*): left-clicking the menu-bar icon always opens the quick-look popover, while *Open in Window* opens a second, ordinary decorated window with a Dock icon (macOS) / taskbar entry (Windows) that reopens where you left it. Closing that window hides it — the app keeps living in the menu bar — and no restart is needed to open it again
- For Claude Code: **counts only the MCP servers / Skills you installed yourself** — all Claude built-in tools and Anthropic's bundled MCP servers are filtered out

## Data sources (zero-intrusion, read-only)

| Purpose | Path |
|---------|------|
| **Claude Code** session logs (tokens / model / tool calls) | `~/.claude/projects/**/*.jsonl` |
| **Codex CLI** session rollouts (tokens / model) | `$CODEX_HOME/sessions/**/rollout-*.jsonl` (default `~/.codex/...`) |
| **opencode** sessions, new SQLite store (tokens / model) | `~/.local/share/opencode/opencode.db` → `message.data` |
| **opencode** sessions, legacy JSON store (tokens / model) | `~/.local/share/opencode/storage/message/**/*.json` |
| **Oh My Pi** session logs (tokens / model) | `~/.omp/agent/sessions/**/*.jsonl` + `~/.omp/profiles/*/agent/sessions/**/*.jsonl` |
| User MCP whitelist (Claude Code) | `~/.claude.json` → `mcpServers` + `projects[*].mcpServers` |
| User Skill whitelist (Claude Code) | `~/.claude/skills/` directory |
| Plan-window usage, opencode Go/Zen | `GET https://opencode.ai/zen/go/v1/usage` (Bearer API key, read-only) |
| Plan-window usage, Claude Code | `GET https://api.anthropic.com/api/oauth/usage` (Bearer OAuth token + `anthropic-beta: oauth-2025-04-20`) |
| Plan-window usage, any other provider | `~/.omp/agent/agent.db` → `usage_history` (Oh My Pi's own poller snapshot) |
| opencode-go API key (never logged) | `OPENCODE_GO_API_KEY` / `OPENCODE_API_KEY` env → `~/.omp/**/agent.db` → `auth_credentials` → `~/.local/share/opencode/auth.json` |
| Claude OAuth token (never logged) | `CLAUDE_CODE_OAUTH_TOKEN` env → macOS keychain `Claude Code-credentials*` → `~/.claude/.credentials.json` → `~/.omp/**/agent.db` → `auth_credentials`; every source is read and the first token the endpoint accepts wins, because the copies do not expire together |
| Model prices | **Primary**: [models.dev](https://models.dev/api.json) (bare model names, matching Claude CLI logs) → **Fallback**: [LiteLLM](https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json) → bundled snapshot → built-in flagship table. Cached in `~/Library/Caches/tokenscope/`, refreshed every 24h (ETag-conditional), with offline fallback |

Sources that aren't installed are simply skipped — no directory, no data, no error.

### Key processing
- Deduplicated by `message.id` (streaming/retries repeat the same usage); when one message spans multiple lines, its tool calls are merged and the token usage is counted once
- **Claude Code re-appends the whole assistant line as the response streams**, so one `message.id` repeats in a file with `output_tokens` climbing on each copy (measured: 355 of 647 ids in a 6 h window ended above their first count, while `input`/`cache_*` never changed). Tool calls merge *and* the token reading is taken from the newest line that carries one — keeping only the first copy froze every streamed response at its opening chunk and lost ~69% of Claude Code's output tokens. A copy with no usage at all (the response had not started streaming) never overwrites a stored reading
- Per-source token mapping: Claude Code reports the four classes directly; **Codex** counts cached input *inside* `input_tokens` (so uncached input = the difference) and has no cache-write figure; **opencode**'s separate `reasoning` bucket folds into output (same rate); **Oh My Pi** reports the same four classes as Claude Code; **pi** reports `usage{input, output, cacheRead, cacheWrite, reasoning}` per assistant message, with `reasoning` folding into output and `cacheWrite` counting as 5-minute cache creation (same convention as opencode)
- **Codex** rollouts carry a per-turn `last_token_usage` plus a cumulative `total_token_usage`; the per-turn figure is used, so a session's tokens land on the hour/day they happened. One rollout file = one session
- **Oh My Pi** writes per-agent logs (`__advisor.*.jsonl`, subagent files) under one session directory; they are grouped into a single session, and every agent's tokens count
- Tool classification (MCP / Skill breakdowns) reads **Claude Code** logs only — the MCP whitelist comes from Claude's own config, so other agents' tool calls count toward their agent total but not those two lists
- **Plan limits** answer "how much of my quota is gone", which is not derivable from logs: the provider meters a rolling window and only it knows the fill level. opencode Go/Zen is queried at `/v1/usage` (`usage.rolling|weekly|monthly.{percent,status,resetsAt}`) and Claude at `/api/oauth/usage` (`five_hour`/`seven_day`/`seven_day_*` `utilization`, plus scoped windows in `limits[]` — that is where `7 Day (Fable)` comes from); anything else falls back to Oh My Pi's own poller snapshot. Newest observation wins per window; a failed fetch keeps the last known numbers but marks the row `stale · read HH:MM` (or `window reset` once its reset time has passed) instead of presenting them as current, and credentials are only ever read from env/disk, sent in an `Authorization` header, and never logged or persisted
- Persisted cache folds each *finished* day into one row per (day, agent, session, model): only today needs per-message resolution (the Day chart's 24 hourly buckets), so a harness logging tens of thousands of requests a day doesn't grow the cache into the hundreds of MB. The store is held in memory while the app runs and **checkpointed every 15 minutes** (plus on the first change of a run and on exit), not rewritten per refresh: refreshes are driven by log writes, so writing the whole cache per refresh pushed gigabytes a day through the disk and tripped macOS's per-process disk-writes limit, which stalls the refresh loop inside `write()` and freezes the panel. Deleted log files are dropped from the manifest at checkpoint time so it can't grow without bound
- Token split: `input` (uncached) / `cache` (creation+read) / `output`; the UI folds cache into "In" by default and shows a separate "cached %"
- Price matching: exact id → normalized id (strip vendor prefix + `.`↔`p`, e.g. `glm-5.1`⇄`glm-5p1`) → provider-namespace strip (`anthropic.claude-opus-5` → `claude-opus-5`); models.dev's official bare-name price wins
- Models are **grouped by bare name across agents**: Oh My Pi logs `openai.gpt-5.5` / `global.openai.gpt-5.6-sol` where Codex logs `gpt-5.5`, and one model must not split into several rows (a version dot like `glm-5.1` is not a namespace and is left alone)
- Cost is priced per the four token types; each model carries a `priced` flag — **models not found in either source still count tokens but are labelled "no price" in the UI**
- **Cache writes are priced by TTL**: Anthropic charges 1.25x base input for a 5-minute cache write and 2x for a 1-hour one, and Claude Code holds its prompt cache for an hour by default. Claude Code (`cache_creation.ephemeral_*`) and Oh My Pi (`cttl.ephemeral1h`) both report the split, so the 1-hour share is billed at the 1-hour rate; models.dev publishes only the 5-minute rate, so the 1-hour rate for Claude models is derived (2x input) unless LiteLLM's `cache_creation_input_token_cost_above_1hr` says otherwise. Models with no 1-hour cache keep the single published rate
- Logs contain only the bare model name (no vendor) → third-party models default to the official vendor price (an estimate)
- Tool classification: `mcp__<server>__*` where the server is in your config → MCP; a Skill call (the `Skill` tool's `input.skill`, or a `/skill` slash command) whose name is in your skills directory → Skill; everything else is ignored

> Cost is an **estimate** based on public prices; subscription users should read it as "equivalent spend value".

### Token types & cost formula

Every assistant message's `usage` reports four **mutually exclusive** token counts (they never double-count the same token):

| Stage | `usage` field | What it is | Price (relative to input) |
|-------|---------------|------------|---------------------------|
| **Input** (uncached) | `input_tokens` | New prompt tokens sent this turn | 1× |
| **Cache write** | `cache_creation_input_tokens` | Context written into the prompt cache | ~1.25× |
| **Cache read** (hit) | `cache_read_input_tokens` | Context replayed from the cache | ~0.1× (much cheaper) |
| **Output** | `output_tokens` | Tokens the model generated | ~5× |

**Tokens** (per period, summed over messages):

```
total  = input + cache_creation + cache_read + output
# the UI shows:  In = input + cache_creation + cache_read,  Out = output,  cached % = cache_read / total
```

**Cost** (each stage priced at its own per-token rate from the price table):

```
cost = input            × price.input
     + cache_creation   × price.cache_creation
     + cache_read       × price.cache_read     # cache hits billed at the discounted read rate
     + output           × price.output
```

So a cache hit is **not** billed as normal input — it uses the dedicated (cheaper) `cache_read` rate, which is why heavily-cached usage shows a huge token count but a modest cost. The UI folds cache into "In" for display only; billing always uses the four separate rates above.

## Install

### Option 1: Homebrew (recommended)

```bash
brew install --cask hdusy/tokenscope/tokenscope
```

The cask's `postflight` strips the quarantine attribute (`xattr -cr`) automatically, so **it opens on first launch without the "Apple cannot verify" prompt**.

After you open it once it registers as a login item, then **launches in the menu bar automatically on every boot**.

Upgrade:

```bash
brew update && brew upgrade --cask tokenscope
```

### Option 2: Download the .dmg

1. Download the latest `Tokenscope_*_universal.dmg` from [Releases](https://github.com/HduSy/tokenscope/releases) (works on both Apple Silicon and Intel)
2. Drag it into Applications
3. Because the build is **unsigned / unnotarized**, Gatekeeper blocks the first launch — pick one:
   - Right-click the app → **Open** → confirm **Open** again, or
   - Run once in the terminal:
     ```bash
     xattr -cr /Applications/Tokenscope.app && open /Applications/Tokenscope.app
     ```

> Unsigned is a current known limitation. A true "double-click to open" experience requires Apple Developer ID signing + notarization — see `PRD.md` §6.4.

### Option 3: Install on Windows

1. Download the latest `Tokenscope_*_x64-setup.exe` from [Releases](https://github.com/HduSy/tokenscope/releases)
2. Double-click to install. Because the build is **unsigned**, Windows SmartScreen will warn on first run — click **More info → Run anyway**
3. The app installs per-user (no admin required) and registers itself for **launch at login** automatically
4. Requirements: **Windows 10 1803+ / Windows 11** with the WebView2 runtime (preinstalled on Windows 11; Windows 10 users without it will be prompted by the installer)

### After first launch

- **macOS**: an icon plus today's token count appears in the menu bar (e.g. `⬡ 12.40M`)
- **Windows**: the tray icon appears in the notification area. The Windows tray API doesn't show a label beside the icon — **hover the tray icon** to see today's token count in the tooltip (e.g. `Tokenscope · today 12.40M`)
- Left-click the icon to toggle the popover; right-click for the menu (Open / Refresh / **Open in Window** / Launch at Login / Quit)
- **Launch-at-login is set up automatically** — no manual configuration needed

## Develop

```bash
pnpm install
pnpm tauri dev         # launch the desktop app (requires the Rust toolchain)
```

Frontend-only preview (using the real-data snapshot `public/dev-dashboard.json`):

```bash
pnpm dev               # http://localhost:1420
# refresh the snapshot (the example loads the real price table first, so
# costs match the app; `cargo run --example dump` alone only has the built-in
# snapshot and marks everything else "no price"):
cd src-tauri && cargo run --example dump > ../public/dev-dashboard.json
```

## Build

```bash
pnpm tauri build       # outputs .app / .dmg on macOS, .exe (NSIS) on Windows to src-tauri/target/release/bundle/
```

For distribution see `PRD.md` §6.3 (Homebrew Cask recommended on macOS; direct `.dmg` / `.exe` downloads benefit from code signing + notarization).

## Structure

```
src/                  React frontend
  data.ts             types + Tauri bridge + theme + formatting
  charts.tsx          chart primitives (bars / donut / sparkline / heatmap / segmented control)
  App.tsx             main panel
src-tauri/src/
  store.rs            incremental multi-agent ingest — Claude Code / Codex CLI / opencode (JSON + SQLite) / Oh My Pi (dedup by message id) / pi (session JSONL, `~/.pi/agent/sessions`)
  parser.rs           aggregation (Day/Week/Month + heatmap)
  pricing.rs          models.dev / LiteLLM price loading and costing
  config.rs           user MCP / Skill whitelist
  model.rs            data structures returned to the frontend
  lib.rs              Tauri commands + menu-bar tray
```

## Bug log

Notable bugs found during development — symptom, root cause, and fix — are
collected in [docs/BUGFIXES.md](docs/BUGFIXES.md).
