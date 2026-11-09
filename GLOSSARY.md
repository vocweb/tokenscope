# Glossary

Domain language for TokenScope. Implementation details live in code and
`docs/`; this file defines what words mean, nothing else.

## agent

A coding assistant CLI TokenScope ingests, identified by a stable source id:
`claude` (Claude Code), `codex` (OpenAI Codex CLI), `opencode`, `omp`
(Oh My Pi), `pi`. The id is what the dashboard groups by and what
`ToolStat.name` carries; the display label (`Claude Code`, `opencode`, …)
is derived from it.

Not to be confused with **provider** (below): the agent is *who logged the
usage*, the provider is *who is billed or metered*.

## provider

The upstream service behind a model or a quota window: `anthropic`,
`openai`, `opencode-go` (the plan-limit API TokenScope polls at
`/v1/usage`), and similar. A provider name may appear as a model namespace
(`openai.gpt-5.5`), a plan-limit row label, or a price-table vendor — never
as an agent id. In particular, agent `opencode` and provider `opencode-go`
are different things: the first is a log source, the second is a quota API.

## session

One conversation with the user. Each agent shards it differently on disk
(Claude Code directories, Codex rollout files, opencode `session_id`
columns, OMP per-agent files), and subagent/advisor runs are folded into
their root conversation (opencode via `session_v2.parent_id`, OMP by
session directory) so the count reflects conversations, not spawns.

## token class

The four buckets every parsed message is reduced to: `input` (uncached),
`cache-read`, `cache-write` (split by lifetime: 5-minute and 1-hour, billed
at different rates), and `output`. An agent's `reasoning` bucket has no
bucket here and folds into `output`, which shares its rate.
