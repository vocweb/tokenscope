// Incremental event store.
//
// Ingestion (this file) is the only place that touches the agent logs. It
// parses each assistant message into a provider/config/price-independent
// RawEvent (just the facts), reads only newly-appended bytes of changed files
// (tracked by a per-file size/mtime/offset manifest), dedupes by message id,
// and persists everything to the cache dir. Aggregation (parser.rs) then works
// purely on these in-memory events — cheap, and recomputed per request because
// the Day/Week/Month windows are relative to "now".
//
// Four coding-agent CLIs are ingested, all read-only:
//
//   claude   ~/.claude/projects/**/*.jsonl            (Claude Code)
//   codex    $CODEX_HOME/sessions/**/rollout-*.jsonl  (OpenAI Codex CLI)
//   opencode ~/.local/share/opencode/storage/message/**/*.json + opencode.db
//   omp      ~/.omp/**/agent/sessions/**/*.jsonl      (Oh My Pi)
//
// Each source writes a different shape, so each has its own line/file parser
// below; everything after that (dedupe, manifest, prune, persist) is shared.
use chrono::{DateTime, Local};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

// Stable source ids. Also the sort order the UI renders them in.
pub const TOOL_CLAUDE: &str = "claude";
pub const TOOL_CODEX: &str = "codex";
pub const TOOL_OPENCODE: &str = "opencode";
pub const TOOL_OMP: &str = "omp";

#[derive(Serialize, Deserialize, Clone)]
pub struct RawEvent {
    pub ts_ms: i64,
    pub session: String,
    pub model: String, // raw model id (price lookup), normalized later for grouping
    pub in_tok: f64,
    pub cc: f64, // cache creation, 5-minute lifetime share
    /// Cache creation written with a 1-hour lifetime. Anthropic bills those at
    /// twice the base input price, so they can't be lumped in with `cc`.
    #[serde(default)]
    pub cc_1h: f64,
    pub cr: f64, // cache read
    pub out_tok: f64,
    /// API requests folded into this row. Always 1 for a raw message; a
    /// compacted day row carries however many it summed.
    #[serde(default = "one")]
    pub n: u64,
    pub mcp: Vec<String>,    // all mcp__<server> names called (unfiltered)
    pub skills: Vec<String>, // all Skill input.skill ids called (unfiltered)
    pub id: String,          // message id (dedup)
    // Source log file (manifest key). Lets a truncated/rewritten file purge its
    // own stale events before being re-read, so re-ingestion stays idempotent.
    #[serde(default)]
    pub source: String,
    // Which CLI this event came from (TOOL_*). Empty only for pre-v5 caches,
    // which the STORE_VERSION bump discards anyway.
    #[serde(default)]
    pub tool: String,
}

/// Split a cache-write total into its 5-minute and 1-hour shares, returning
/// `(5m, 1h)`. Both agents that report cache writes say how much of the total was
/// written with a 1-hour lifetime — Claude Code as
/// `cache_creation.ephemeral_1h_input_tokens`, Oh My Pi as `cttl.ephemeral1h` —
/// and Anthropic bills those at 2x base input against 1.25x for 5-minute ones.
/// Anything the breakdown doesn't account for stays on the 5-minute rate, and a
/// log with no breakdown (an older build) keeps its whole total there, which is
/// how these tokens were priced before the split existed.
fn split_cache_write(total: f64, one_hour: f64) -> (f64, f64) {
    let one_hour = one_hour.max(0.0).min(total.max(0.0));
    (total - one_hour, one_hour)
}

fn one() -> u64 {
    1
}

/// Add `src`'s totals into a compacted day row (`dst`), keeping the MCP / Skill
/// call lists so their period counts survive compaction.
fn fold_into(dst: &mut RawEvent, src: &RawEvent) {
    dst.in_tok += src.in_tok;
    dst.cc += src.cc;
    dst.cc_1h += src.cc_1h;
    dst.cr += src.cr;
    dst.out_tok += src.out_tok;
    dst.n += src.n;
    dst.mcp.extend(src.mcp.iter().cloned());
    dst.skills.extend(src.skills.iter().cloned());
}

/// Id prefix of a compacted day row (see Store::compact).
const AGG_PREFIX: &str = "agg:";

/// Per-file scratch state for sources whose lines are only meaningful in
/// context: Codex emits a running model in `turn_context` and per-turn usage in
/// `token_count`, so a resumed incremental scan needs the model (and the last
/// cumulative total, for logs that carry no per-turn delta) from the bytes it
/// already read.
#[derive(Serialize, Deserialize, Default, Clone)]
struct FileState {
    model: String,
    seq: u64,       // emitted-event counter, used to build stable ids
    tot: [u64; 4],  // last cumulative (input, cached, output, reasoning)
}

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    // path -> (size, mtime_ms, byte offset already ingested)
    files: HashMap<String, (u64, i64, u64)>,
    // path -> codex/omp per-file parser state (see FileState)
    #[serde(default)]
    state: HashMap<String, FileState>,
    // sqlite db path -> ingest watermark (opencode.db: max time_updated)
    #[serde(default)]
    dbs: HashMap<String, i64>,
}

pub struct Store {
    pub events: Vec<RawEvent>,
    // message id -> index in `events`. A single assistant message can be split
    // across several JSONL lines (e.g. thinking on one line, tool_use on the
    // next) that all share its id; we merge their tool calls into one event and
    // count its token usage only once.
    index: HashMap<String, usize>,
    manifest: Manifest,
}

// Bump when the parsing/extraction logic changes in a way that requires
// re-reading logs from scratch (the incremental manifest would otherwise skip
// already-seen bytes and miss newly-extracted facts).
//   v2: count slash-command skill invocations (`/skill`), not just Skill tool_use.
//   v3: merge tool_use across lines sharing a message id (a thinking line + a
//       tool_use line were deduped, dropping the tool call).
//   v4: track a per-event source file (idempotent re-read of truncated logs).
//   v5: ingest Codex / opencode / Oh My Pi alongside Claude Code; RawEvent
//       gains `tool`, so v4 caches (claude-only, tool-less) are discarded.
//   v6: events carry a request count (`n`) and finished days are compacted; the
//       v5 cache also mis-keyed Codex sessions, so it must be rebuilt.
//   v7: cache writes carry their 5-minute / 1-hour split (`cc_1h`), which
//       Anthropic bills at 1.25x and 2x base input. A v6 cache has every write
//       on the 5-minute rate, so it must be rebuilt or the undercount persists
//       for all history that predates this build.
//   v8: codex turns that report only `total_tokens` (breakdown zeroed) are
//       counted instead of recorded as zero-token requests; a v7 cache holds
//       those as 0, so it must be rebuilt for that history to be right.
const STORE_VERSION: u32 = 8;

/// Atomically replace `path`'s contents: write a sibling temp file, then rename
/// over the target (same-volume rename is atomic on Windows and Unix). Avoids
/// the half-written/truncated JSON that a crash mid-`fs::write` would leave.
fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn claude_dir() -> Option<PathBuf> {
    Some(home()?.join(".claude").join("projects"))
}

/// Codex CLI roots its state at $CODEX_HOME (default ~/.codex); sessions are
/// sharded by date under sessions/YYYY/MM/DD/rollout-<iso>-<uuid>.jsonl.
fn codex_sessions_dir() -> Option<PathBuf> {
    let root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))?;
    Some(root.join("sessions"))
}

/// opencode follows the XDG data dir: $XDG_DATA_HOME/opencode, default
/// ~/.local/share/opencode.
pub fn opencode_data_dir() -> Option<PathBuf> {
    match std::env::var_os("OPENCODE_DATA") {
        Some(v) => Some(PathBuf::from(v)),
        None => match std::env::var_os("XDG_DATA_HOME") {
            Some(v) => Some(PathBuf::from(v).join("opencode")),
            None => Some(home()?.join(".local").join("share").join("opencode")),
        },
    }
}

/// (legacy JSON message dir, sqlite db path) for opencode.
fn opencode_paths() -> Option<(PathBuf, PathBuf)> {
    let root = opencode_data_dir()?;
    Some((
        root.join("storage").join("message"),
        root.join("opencode.db"),
    ))
}

/// Oh My Pi keeps one agent dir per profile: ~/.omp/agent plus
/// ~/.omp/profiles/<name>/agent. Sessions live under <agent>/sessions/<project>/
/// as either <iso>_<uuid>.jsonl (single agent) or <iso>_<uuid>/<agent>.jsonl.
fn omp_session_roots() -> Vec<PathBuf> {
    let Some(home) = home() else { return Vec::new() };
    let base = home.join(".omp");
    let mut roots = vec![base.join("agent").join("sessions")];
    if let Ok(entries) = fs::read_dir(base.join("profiles")) {
        for e in entries.flatten() {
            roots.push(e.path().join("agent").join("sessions"));
        }
    }
    roots
}

fn cache_dir() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("tokenscope");
    let _ = fs::create_dir_all(&d);
    Some(d)
}

/// Directories the filesystem watcher registers, so a log write reaches the
/// panel in ~1s instead of waiting for the 30s poll. Roots that don't exist are
/// skipped (a CLI the user doesn't have installed needs no watcher) — except
/// Claude Code's, which is created so a fresh machine still gets live updates.
pub fn watch_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(claude) = claude_dir() {
        let _ = fs::create_dir_all(&claude);
        roots.push(claude);
    }
    if let Some(codex) = codex_sessions_dir() {
        if codex.is_dir() {
            roots.push(codex);
        }
    }
    if let Some(opencode) = opencode_data_dir() {
        if opencode.is_dir() {
            roots.push(opencode);
        }
    }
    for omp in omp_session_roots() {
        if omp.is_dir() {
            roots.push(omp);
        }
    }
    roots
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Oh My Pi spreads one conversation over several files: `<iso>_<uuid>.jsonl`
/// for the main agent plus `<iso>_<uuid>/<agent>.jsonl` for advisors and
/// subagents. The shared `<iso>_<uuid>` directory component is the session, so
/// those all fold into one — otherwise every advisor would look like its own
/// session. Codex rolls one file per session, so it just uses the file stem.
fn omp_session_from_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(idx) = s.find("/sessions/") {
        let rest = &s[idx + "/sessions/".len()..];
        let mut parts = rest.split('/');
        let _project = parts.next();
        if let Some(second) = parts.next() {
            return match second.strip_suffix(".jsonl") {
                Some(session) => session.to_string(),
                None => second.to_string(),
            };
        }
    }
    stem(path)
}

fn now_ms_from_rfc3339(ts: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp_millis())
}

impl Store {
    /// Load persisted events + offset manifest (empty on first run).
    pub fn load() -> Self {
        let mut events: Vec<RawEvent> = Vec::new();
        let mut manifest = Manifest::default();
        if let Some(dir) = cache_dir() {
            // If the cache was written by an older parser, discard it so ingest
            // does a full rescan and picks up newly-extracted facts.
            let version_ok = fs::read_to_string(dir.join("version"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                == Some(STORE_VERSION);
            if version_ok {
                // events.json and offsets.json are ONE consistent unit: the
                // manifest's per-file byte offsets are only meaningful relative
                // to the events we actually loaded. If either is missing or fails
                // to parse (e.g. a crash left events.json half-written), discard
                // BOTH and fall back to a full rescan — otherwise a good manifest
                // paired with empty/corrupt events would make ingest() skip every
                // already-recorded file and silently lose all history.
                let loaded_events = fs::read_to_string(dir.join("events.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Vec<RawEvent>>(&t).ok());
                let loaded_manifest = fs::read_to_string(dir.join("offsets.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Manifest>(&t).ok());
                if let (Some(e), Some(m)) = (loaded_events, loaded_manifest) {
                    events = e;
                    manifest = m;
                }
            }
        }
        let index = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Store {
            events,
            index,
            manifest,
        }
    }

    /// Persist events + manifest.
    ///
    /// Deliberately *not* called on every refresh: the whole cache is
    /// re-serialized here (several MB), and refreshes are driven by log writes,
    /// so writing per refresh is what made the app dirty gigabytes per hour.
    /// The caller checkpoints on a schedule instead (see parser.rs).
    pub fn save(&mut self) {
        self.prune_missing_files();
        if let Some(dir) = cache_dir() {
            // Atomic writes so a crash/kill mid-save can't leave a half-written
            // events.json (load() would then discard the pair and lose history).
            // Write events before offsets: if we crash between them, the manifest
            // is merely stale (points at fewer bytes → re-reads a little) rather
            // than ahead of the events on disk.
            if let Ok(t) = serde_json::to_string(&self.events) {
                let _ = write_atomic(&dir.join("events.json"), t.as_bytes());
            }
            if let Ok(t) = serde_json::to_string(&self.manifest) {
                let _ = write_atomic(&dir.join("offsets.json"), t.as_bytes());
            }
            let _ = write_atomic(&dir.join("version"), STORE_VERSION.to_string().as_bytes());
        }
    }

    /// Drop manifest entries whose log file no longer exists.
    ///
    /// Entries were only ever inserted, so every session file ever seen kept a
    /// full path (~150 bytes) in the manifest — which is re-serialized on every
    /// checkpoint — even after Claude Code cleaned the file up. The events those
    /// files produced are kept: they are still history, and a file that comes
    /// back is re-read from the start and deduplicated by message id.
    fn prune_missing_files(&mut self) {
        let Manifest { files, state, dbs } = &mut self.manifest;
        let before = files.len();
        files.retain(|k, _| std::path::Path::new(k).exists());
        if files.len() == before {
            return;
        }
        // A file's parser state is only meaningful alongside its offset.
        state.retain(|k, _| files.contains_key(k));
        dbs.retain(|k, _| std::path::Path::new(k).exists());
    }

    /// Rebuild the id→index map after the `events` vector is mutated wholesale
    /// (purge/prune shift positions, so partial updates aren't enough).
    fn rebuild_index(&mut self) {
        self.index = self
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
    }

    /// Drop every event that came from `key`, then rebuild the index. Used before
    /// re-reading a truncated/rewritten file so re-ingestion is idempotent
    /// (otherwise the cross-line tool_use merge re-appends calls and id-less
    /// events get pushed twice, inflating MCP/Skill counts and token totals).
    fn purge_source(&mut self, key: &str) {
        self.events.retain(|e| e.source != key);
        self.rebuild_index();
    }

    /// Record one parsed event.
    ///
    /// Dedup semantics differ by source, because the logs differ: Claude Code
    /// splits one message across several lines that all share its id, so a
    /// repeat means "same message, more tool calls" (merge, never re-count
    /// tokens). Codex/opencode/Oh My Pi rewrite a message as it streams, so a
    /// repeat means "same message, newer numbers" (replace).
    fn push_event(&mut self, ev: RawEvent) {
        if !ev.id.is_empty() {
            if let Some(&i) = self.index.get(&ev.id) {
                if ev.tool == TOOL_CLAUDE {
                    let prev = &mut self.events[i];
                    prev.mcp.extend(ev.mcp);
                    prev.skills.extend(ev.skills);
                } else {
                    self.events[i] = ev;
                }
                return;
            }
            self.index.insert(ev.id.clone(), self.events.len());
        }
        self.events.push(ev);
    }

    /// Drop events older than `cutoff_ms`. The reports/heatmap only span the last
    /// ~26 weeks, so anything older is dead weight that grows events.json without
    /// bound. Returns whether anything was removed. Old logs already at EOF are
    /// never re-read, so their pruned events don't reappear.
    pub fn prune_before(&mut self, cutoff_ms: i64) -> bool {
        let before = self.events.len();
        self.events.retain(|e| e.ts_ms >= cutoff_ms);
        let removed = self.events.len() != before;
        if removed {
            self.rebuild_index();
        }
        removed
    }

    /// Fold every event from a *finished* day into one row per
    /// (day, tool, session, model) and return whether anything was folded.
    ///
    /// Only today needs sub-day resolution (the Day report's 24 hourly buckets);
    /// Week, Month and the heatmap all sum day-level totals, and requests /
    /// sessions stay exact because a row carries its request count. Without this
    /// the cache grows with every API call — Oh My Pi runs advisors on nearly
    /// every step (tens of thousands of requests a day), which took events.json
    /// to 144 MB and made each refresh rewrite all of it.
    ///
    /// Rows get a deterministic id and no source file: re-folding a day is a
    /// no-op, and a later truncation-purge of the files they came from (already
    /// read to EOF) can't drop them.
    pub fn compact(&mut self, today: chrono::NaiveDate) -> bool {
        use chrono::TimeZone;
        let day_start = |d: chrono::NaiveDate| -> i64 {
            Local
                .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                .earliest()
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0)
        };
        let cutoff_ms = day_start(today);

        let mut folded: HashMap<String, RawEvent> = HashMap::new();
        let mut kept: Vec<RawEvent> = Vec::with_capacity(self.events.len());
        for ev in std::mem::take(&mut self.events) {
            if ev.id.starts_with(AGG_PREFIX) || ev.ts_ms >= cutoff_ms {
                kept.push(ev);
                continue;
            }
            let day = DateTime::from_timestamp_millis(ev.ts_ms)
                .unwrap_or_default()
                .with_timezone(&Local)
                .date_naive();
            let id = format!(
                "{}{}:{}:{}:{}",
                AGG_PREFIX, day, ev.tool, ev.session, ev.model
            );
            let row = folded.entry(id.clone()).or_insert_with(|| RawEvent {
                ts_ms: day_start(day),
                session: ev.session.clone(),
                model: ev.model.clone(),
                in_tok: 0.0,
                cc: 0.0,
                cc_1h: 0.0,
                cr: 0.0,
                out_tok: 0.0,
                n: 0,
                mcp: Vec::new(),
                skills: Vec::new(),
                id,
                source: String::new(),
                tool: ev.tool.clone(),
            });
            fold_into(row, &ev);
        }
        self.events = kept;
        if folded.is_empty() {
            return false;
        }
        // Positions shifted when the folded events left the vector, so the index
        // must be rebuilt *before* the merge loop below looks rows up in it.
        self.rebuild_index();
        for row in folded.into_values() {
            // An existing row for the same day (folded by an earlier run) must
            // accumulate, not be replaced.
            match self.index.get(&row.id).copied() {
                Some(i) => fold_into(&mut self.events[i], &row),
                None => {
                    self.index.insert(row.id.clone(), self.events.len());
                    self.events.push(row);
                }
            }
        }
        true
    }

    /// Incrementally ingest every supported agent log.
    /// Returns whether anything changed (new events or an updated file offset),
    /// so the caller can skip a full cache rewrite when nothing moved.
    pub fn ingest(&mut self) -> bool {
        let mut dirty = false;
        dirty |= self.ingest_claude();
        dirty |= self.ingest_codex();
        dirty |= self.ingest_omp();
        dirty |= self.ingest_opencode_json();
        dirty |= self.ingest_opencode_db();
        dirty
    }

    // ── shared incremental JSONL scan ─────────────────────────────────
    /// Read only the new bytes of one JSONL file and map each line to at most
    /// one event. `map` may mutate the file's persistent `FileState` (codex/omp
    /// need the running model and counters). Returns whether the file changed.
    fn scan_jsonl(
        &mut self,
        path: &Path,
        tool: &str,
        map: &mut dyn FnMut(&serde_json::Value, &mut FileState) -> Option<RawEvent>,
    ) -> bool {
        let key = path.to_string_lossy().to_string();
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        let size = meta.len();
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let mut offset = match self.manifest.files.get(&key).copied() {
            Some((psize, pmtime, poff)) => {
                if psize == size && pmtime == mtime_ms {
                    return false; // unchanged → skip
                }
                if size < poff {
                    // truncated / rewritten (e.g. log compaction): the bytes
                    // we already ingested are gone, so purge this file's
                    // events and re-read from the start, idempotently.
                    self.purge_source(&key);
                    self.manifest.state.remove(&key);
                    0
                } else {
                    poff
                }
            }
            None => 0,
        };

        let Ok(mut f) = fs::File::open(path) else {
            return false;
        };
        if f.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return false;
        }
        // only process up to the last newline; leave a partial trailing line
        // (file still being written) for the next pass
        let process_until = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => 0,
        };
        let mut state = self.manifest.state.get(&key).cloned().unwrap_or_default();
        for line in buf[..process_until].split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(s) = std::str::from_utf8(line) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else {
                continue;
            };
            if let Some(mut ev) = map(&v, &mut state) {
                ev.source = key.clone();
                ev.tool = tool.to_string();
                self.push_event(ev);
            }
        }
        self.manifest.state.insert(key.clone(), state);
        offset += process_until as u64;
        self.manifest.files.insert(key, (size, mtime_ms, offset));
        true
    }

    // ── Claude Code ───────────────────────────────────────────────────
    fn ingest_claude(&mut self) -> bool {
        let Some(root) = claude_dir() else {
            return false;
        };
        let mut dirty = false;
        let paths: Vec<PathBuf> = WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
            .map(|e| e.path().to_path_buf())
            .collect();
        for path in paths {
            dirty |= self.scan_jsonl(&path, TOOL_CLAUDE, &mut |v, _st| parse_claude_line(v));
        }
        dirty
    }

    // ── Codex CLI ─────────────────────────────────────────────────────
    fn ingest_codex(&mut self) -> bool {
        let Some(root) = codex_sessions_dir() else {
            return false;
        };
        if !root.is_dir() {
            return false;
        }
        let mut dirty = false;
        let paths: Vec<PathBuf> = WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
            .map(|e| e.path().to_path_buf())
            .collect();
        for path in paths {
            // one rollout file per session: the stem carries the session uuid
            let session = stem(&path);
            dirty |= self.scan_jsonl(&path, TOOL_CODEX, &mut |v, st| {
                parse_codex_line(v, st, &session)
            });
        }
        dirty
    }

    // ── Oh My Pi ──────────────────────────────────────────────────────
    fn ingest_omp(&mut self) -> bool {
        let mut dirty = false;
        for root in omp_session_roots() {
            if !root.is_dir() {
                continue;
            }
            let paths: Vec<PathBuf> = WalkDir::new(&root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
                .map(|e| e.path().to_path_buf())
                .collect();
            for path in paths {
                let session = omp_session_from_path(&path);
                dirty |= self.scan_jsonl(&path, TOOL_OMP, &mut |v, _st| {
                    parse_omp_line(v, &session)
                });
            }
        }
        dirty
    }

    // ── opencode: legacy JSON store ───────────────────────────────────
    /// opencode < ~1.1 wrote one JSON document per message
    /// (`storage/message/<session>/<msg>.json`), which cannot be read
    /// incrementally by byte offset — a whole file is either new or rewritten,
    /// so it is re-parsed in full whenever size/mtime moved and deduped by
    /// message id.
    fn ingest_opencode_json(&mut self) -> bool {
        let Some((dir, _)) = opencode_paths() else {
            return false;
        };
        if !dir.is_dir() {
            return false;
        }
        let mut dirty = false;
        let paths: Vec<PathBuf> = WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .collect();
        for path in paths {
            let key = path.to_string_lossy().to_string();
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            let size = meta.len();
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if let Some((psize, pmtime, _)) = self.manifest.files.get(&key).copied() {
                if psize == size && pmtime == mtime_ms {
                    continue;
                }
            }
            if self.manifest.files.get(&key).map(|f| f.0 > size).unwrap_or(false) {
                // shrunk/rewritten → drop what we had for this file first
                self.purge_source(&key);
            }
            self.manifest.files.insert(key.clone(), (size, mtime_ms, size));
            dirty = true;
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let mut state = FileState::default();
            if let Some(mut ev) = parse_opencode_message(&v, &mut state) {
                ev.source = key;
                ev.tool = TOOL_OPENCODE.to_string();
                self.push_event(ev);
            }
        }
        dirty
    }

    // ── opencode: sqlite store (≥ 1.1) ────────────────────────────────
    /// `opencode.db` keeps the same message documents in `message.data` (JSON)
    /// with id/session_id/time_created lifted into columns. Messages are
    /// rewritten while streaming, so the watermark is `time_updated` (not rowid)
    /// and a re-read of the boundary row is harmless: `push_event` replaces.
    fn ingest_opencode_db(&mut self) -> bool {
        let Some((_, db)) = opencode_paths() else {
            return false;
        };
        if !db.is_file() {
            return false;
        }
        self.ingest_opencode_db_at(&db)
    }

    /// Split from path discovery so the SQL + json join can be exercised against
    /// a throwaway db.
    fn ingest_opencode_db_at(&mut self, db: &Path) -> bool {
        let key = db.to_string_lossy().to_string();
        let mark = self.manifest.dbs.get(&key).copied().unwrap_or(0);
        let Ok(conn) = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return false;
        };
        let mut parsed = false;
        let Ok(mut stmt) = conn.prepare(
            // The watermark is the `time_updated` *column*, so it has to be
            // selected — the message JSON has no such field (and its `time.created`
            // is the message's birth, not its last rewrite).
            //
            // Strict `>`: with `>=` the boundary row is re-read on every pass,
            // which reports "dirty" (and rewrites the cache) forever. The cost is
            // that an update landing in the *same* millisecond as the watermark
            // is missed — a row rewritten later than the message that set the
            // watermark still has time_updated > mark.
            "SELECT id, session_id, time_created, time_updated, data FROM message WHERE time_updated > ?1",
        ) else {
            return false;
        };
        let mut max_seen = mark;
        {
            let Ok(rows) = stmt.query_map([mark], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            }) else {
                return false;
            };
            for row in rows.flatten() {
                let (id, session_id, time_created, time_updated, data) = row;
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
                    continue;
                };
                parsed = true;
                max_seen = max_seen.max(time_updated);
                // `data` is the message document minus id/sessionID, which the
                // table carries in its own columns (see opencode's `info(row)`).
                let mut v = v;
                if let Some(obj) = v.as_object_mut() {
                    obj.entry("id").or_insert(serde_json::Value::String(id));
                    obj.entry("sessionID")
                        .or_insert(serde_json::Value::String(session_id));
                    obj.entry("time_created")
                        .or_insert(serde_json::json!(time_created));
                }
                let mut state = FileState::default();
                if let Some(mut ev) = parse_opencode_message(&v, &mut state) {
                    ev.source = key.clone();
                    ev.tool = TOOL_OPENCODE.to_string();
                    self.push_event(ev);
                }
            }
        }
        // The watermark is the newest `time_updated` we have ingested.
        if max_seen > mark {
            self.manifest.dbs.insert(key, max_seen);
        }
        parsed || max_seen > mark
    }
}

// ── Claude Code line parser ────────────────────────────────────────
/// Parse one Claude Code JSONL line into a RawEvent (assistant messages only).
fn parse_claude_line(v: &serde_json::Value) -> Option<RawEvent> {
    match v.get("type")?.as_str()? {
        "assistant" => parse_assistant(v),
        // Skills invoked via slash command (e.g. `/find-skills`) are logged as a
        // user message with a <command-name> tag, NOT as a Skill tool_use, so
        // they need a separate path or they'd never be counted.
        "user" => parse_user_command(v),
        _ => None,
    }
}

/// Extract the inner text of `<tag>...</tag>` from `s`, if present.
fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let rest = &s[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].to_string())
}

/// A user message that is a slash-command invocation of a skill, e.g.
/// `<command-name>/find-skills</command-name>`. The skill name is left
/// unfiltered here; compute_event drops non-user skills via the whitelist.
fn parse_user_command(v: &serde_json::Value) -> Option<RawEvent> {
    let text = v.get("message")?.get("content")?.as_str()?;
    let raw = extract_tag(text, "command-name")?;
    let skill = raw.trim().trim_start_matches('/').trim().to_string();
    if skill.is_empty() {
        return None;
    }
    let ts = v.get("timestamp")?.as_str()?;
    let ts_ms = now_ms_from_rfc3339(ts)?;
    let session = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    // dedup key: the line's own uuid (command messages have no message.id)
    let id = v.get("uuid").and_then(|i| i.as_str())?.to_string();
    if id.is_empty() {
        return None;
    }
    Some(RawEvent {
        ts_ms,
        session,
        model: String::new(), // not an LLM request → no model/tokens/cost
        in_tok: 0.0,
        cc: 0.0,
        cc_1h: 0.0,
        cr: 0.0,
        out_tok: 0.0,
        n: 1,
        mcp: Vec::new(),
        skills: vec![skill],
        id,
        source: String::new(),
        tool: String::new(), // filled in by the caller
    })
}

fn parse_assistant(v: &serde_json::Value) -> Option<RawEvent> {
    let msg = v.get("message")?;
    let model = msg.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
    if model == "<synthetic>" {
        return None;
    }
    let ts = v.get("timestamp")?.as_str()?;
    let ts_ms = now_ms_from_rfc3339(ts)?;
    let session = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();

    let usage = msg.get("usage");
    let g = |k: &str| -> f64 {
        usage
            .and_then(|u| u.get(k))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };
    let (cc_5m, cc_1h) = split_cache_write(
        g("cache_creation_input_tokens"),
        usage
            .and_then(|u| u.pointer("/cache_creation/ephemeral_1h_input_tokens"))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0),
    );

    let mut mcp = Vec::new();
    let mut skills = Vec::new();
    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if let Some(rest) = name.strip_prefix("mcp__") {
                mcp.push(rest.split("__").next().unwrap_or("").to_string());
            } else if name == "Skill" {
                if let Some(sk) = block
                    .get("input")
                    .and_then(|i| i.get("skill"))
                    .and_then(|s| s.as_str())
                {
                    if !sk.is_empty() {
                        skills.push(sk.to_string());
                    }
                }
            }
        }
    }

    Some(RawEvent {
        ts_ms,
        session,
        model: model.to_string(),
        in_tok: g("input_tokens"),
        cc: cc_5m,
        cc_1h,
        cr: g("cache_read_input_tokens"),
        out_tok: g("output_tokens"),
        n: 1,
        mcp,
        skills,
        id,
        source: String::new(),
        tool: String::new(), // filled in by the caller
    })
}

// ── Codex CLI line parser ──────────────────────────────────────────
/// One rollout line. Two shapes matter:
///   `turn_context` → the model in effect for the turn that follows
///   `event_msg`/`token_count` → usage for that turn (`last_token_usage`, the
///     per-request delta; `total_token_usage` is cumulative for the session)
/// Codex counts cached input *inside* input_tokens, so the uncached share is
/// the difference — mapping it the other way would double-count the cache.
fn parse_codex_line(
    v: &serde_json::Value,
    st: &mut FileState,
    session: &str,
) -> Option<RawEvent> {
    let kind = v.get("type")?.as_str()?;
    if kind == "turn_context" {
        if let Some(m) = v.pointer("/payload/model").and_then(|m| m.as_str()) {
            st.model = m.to_string();
        }
        return None;
    }
    if kind != "event_msg" || v.pointer("/payload/type")?.as_str()? != "token_count" {
        return None;
    }
    let ts_ms = now_ms_from_rfc3339(v.get("timestamp")?.as_str()?)?;
    let info = v.pointer("/payload/info")?;
    let u64_at = |u: &serde_json::Value, k: &str| -> Option<u64> { u.get(k)?.as_u64() };

    // Prefer the per-request delta; fall back to differencing the cumulative
    // total for rollout logs that only carry the running number.
    let (input, cached, output) = match info.get("last_token_usage") {
        Some(last) => {
            let input = u64_at(last, "input_tokens").unwrap_or(0);
            let cached = u64_at(last, "cached_input_tokens").unwrap_or(0);
            let output = u64_at(last, "output_tokens").unwrap_or(0);
            // Older rollout logs report a turn as `total_tokens` only, leaving
            // the breakdown fields zeroed (both `last_token_usage` and
            // `total_token_usage` carry the same number). Those sessions did
            // real work, so counting the total — as uncached input, since codex
            // does not say how much of it was cached — is much closer than the
            // zero-token request the breakdown alone would record.
            if input == 0 && cached == 0 && output == 0 {
                match u64_at(last, "total_tokens") {
                    Some(t) if t > 0 => (t, 0, 0),
                    _ => (input, cached, output),
                }
            } else {
                (input, cached, output)
            }
        }
        None => {
            let total = info.get("total_token_usage")?;
            let cur = [
                u64_at(total, "input_tokens").unwrap_or(0),
                u64_at(total, "cached_input_tokens").unwrap_or(0),
                u64_at(total, "output_tokens").unwrap_or(0),
                u64_at(total, "reasoning_output_tokens").unwrap_or(0),
            ];
            let mut delta = [0u64; 4];
            for i in 0..4 {
                delta[i] = cur[i].saturating_sub(st.tot[i]);
            }
            st.tot = cur;
            (delta[0], delta[1], delta[2])
        }
    };

    st.seq += 1;
    Some(RawEvent {
        ts_ms,
        session: session.to_string(),
        model: if st.model.is_empty() {
            "unknown".to_string()
        } else {
            st.model.clone()
        },
        // cached input is a subset of input, not an addition
        in_tok: input.saturating_sub(cached) as f64,
        cc: 0.0, // codex reports no cache-write figure (writes bill as input)
        cc_1h: 0.0,
        cr: cached as f64,
        out_tok: output as f64,
        n: 1,
        mcp: Vec::new(), // MCP/Skill breakdown is Claude Code-only (needs its config)
        skills: Vec::new(),
        // Ids are scoped to the *session*, not the file: the same conversation
        // can be mirrored into several profile dirs, and it must still count once.
        id: format!("dx:{}:{}", session, st.seq),
        source: String::new(),
        tool: String::new(), // filled in by the caller
    })
}

// ── Oh My Pi line parser ───────────────────────────────────────────
/// Oh My Pi writes one JSONL entry per message; assistant entries carry
/// `model`, `provider` and a `usage` block with the four mutually exclusive
/// token classes (same split Claude Code reports).
fn parse_omp_line(v: &serde_json::Value, session: &str) -> Option<RawEvent> {
    if v.get("type")?.as_str()? != "message" {
        return None;
    }
    let m = v.get("message")?;
    if m.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let usage = m.get("usage")?;
    let num = |keys: &[&str]| -> f64 {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(|x| x.as_f64()))
            .unwrap_or(0.0)
    };
    let ts_ms = m
        .get("timestamp")
        .and_then(|t| t.as_i64())
        .or_else(|| v.get("timestamp").and_then(|t| t.as_i64()))
        .or_else(|| v.get("timestamp").and_then(|t| t.as_str()).and_then(now_ms_from_rfc3339))?;
    // Oh My Pi reports the 1-hour share of its cache writes as
    // `usage.cttl.ephemeral1h` (its own `cost.cacheWrite` already bills it at the
    // 1-hour rate).
    let (cc_5m, cc_1h) = split_cache_write(
        num(&["cacheWrite", "cacheWriteTokens"]),
        usage
            .pointer("/cttl/ephemeral1h")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0),
    );
    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Some(RawEvent {
        ts_ms,
        session: session.to_string(),
        model: m
            .get("model")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string(),
        in_tok: num(&["input", "inputTokens"]),
        cc: cc_5m,
        cc_1h,
        cr: num(&["cacheRead", "cacheReadTokens"]),
        out_tok: num(&["output", "outputTokens"]),
        n: 1,
        mcp: Vec::new(),
        skills: Vec::new(),
        id: if id.is_empty() {
            String::new()
        } else {
            // session-scoped: a conversation mirrored into another profile dir
            // still counts once
            format!("omp:{}:{}", session, id)
        },
        source: String::new(),
        tool: String::new(), // filled in by the caller
    })
}

// ── opencode message parser (legacy JSON store + sqlite `data`) ────
/// Both opencode stores hold the same message document:
/// `{ id, sessionID, role, time: { created }, modelID, providerID,
///    tokens: { input, output, reasoning, cache: { read, write } } }`.
/// `reasoning` is a separate bucket there but has no bucket here (and shares
/// the output rate), so it is folded into output rather than dropped.
fn parse_opencode_message(v: &serde_json::Value, _st: &mut FileState) -> Option<RawEvent> {
    if v.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let tokens = v.get("tokens")?;
    let ts_ms = v
        .pointer("/time/created")
        .and_then(|t| t.as_i64())
        .or_else(|| v.get("time_created").and_then(|t| t.as_i64()))?;
    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let num = |x: Option<&serde_json::Value>| x.and_then(|v| v.as_f64()).unwrap_or(0.0);
    Some(RawEvent {
        ts_ms,
        session: v
            .get("sessionID")
            .or_else(|| v.get("session_id"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        model: v
            .get("modelID")
            .or_else(|| v.get("model_id"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string(),
        in_tok: num(tokens.get("input")),
        cc: num(tokens.pointer("/cache/write")),
        cc_1h: 0.0,
        cr: num(tokens.pointer("/cache/read")),
        out_tok: num(tokens.get("output")) + num(tokens.get("reasoning")),
        n: 1,
        mcp: Vec::new(),
        skills: Vec::new(),
        id: if id.is_empty() {
            String::new()
        } else {
            format!("oc:{}", id)
        },
        source: String::new(),
        tool: String::new(), // filled in by the caller
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn codex_splits_cached_input_out_of_input() {
        let mut st = FileState::default();
        st.model = "gpt-5.5".into();
        // input_tokens includes the cached portion; the uncached share is the
        // difference, and cache writes are never reported by codex.
        let ev = parse_codex_line(
            &line(
                r#"{"timestamp":"2026-09-03T15:29:47.868Z","type":"event_msg","payload":{"type":"token_count",
                    "info":{"last_token_usage":{"input_tokens":16527,"cached_input_tokens":15744,
                    "output_tokens":406,"reasoning_output_tokens":25,"total_tokens":16933}}}}"#,
            ),
            &mut st,
            "sess-1",
        )
        .unwrap();
        assert_eq!(ev.in_tok, 783.0);
        assert_eq!(ev.cr, 15744.0);
        assert_eq!(ev.cc, 0.0);
        assert_eq!(ev.out_tok, 406.0);
        assert_eq!(ev.model, "gpt-5.5");
        assert_eq!(ev.session, "sess-1");
    }

    #[test]
    fn codex_counts_a_total_only_turn_instead_of_recording_zero() {
        // Older rollout logs zero the breakdown and report the turn as
        // `total_tokens` alone; those sessions still did real work.
        let mut st = FileState::default();
        st.model = "gpt-5.5".into();
        let ev = parse_codex_line(
            &line(
                r#"{"timestamp":"2026-05-03T14:59:52.501Z","type":"event_msg","payload":{"type":"token_count",
                    "info":{"total_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,
                    "reasoning_output_tokens":0,"total_tokens":12266},
                    "last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,
                    "reasoning_output_tokens":0,"total_tokens":12266},"model_context_window":null}}}"#,
            ),
            &mut st,
            "sess-old",
        )
        .unwrap();
        assert_eq!(ev.in_tok, 12266.0);
        assert_eq!(ev.cr, 0.0);
        assert_eq!(ev.out_tok, 0.0);
    }

    #[test]
    fn codex_deltas_cumulative_totals_when_no_per_turn_usage() {
        let mut st = FileState::default();
        let ev = |v: &serde_json::Value, st: &mut FileState| {
            parse_codex_line(v, st, "s").unwrap()
        };
        let mk = |i: u64, c: u64, o: u64| {
            serde_json::json!({
                "timestamp": "2026-09-03T15:29:47.868Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": { "total_token_usage": {
                        "input_tokens": i, "cached_input_tokens": c, "output_tokens": o
                    } }
                }
            })
        };
        let a = ev(&mk(100, 60, 10), &mut st);
        let b = ev(&mk(250, 100, 40), &mut st);
        // first sighting of the running total: nothing has been counted yet
        assert_eq!((a.in_tok, a.cr, a.out_tok), (40.0, 60.0, 10.0));
        // second: only the delta since the previous line
        assert_eq!((b.in_tok, b.cr, b.out_tok), (110.0, 40.0, 30.0));
        // ids stay distinct so the dedupe index doesn't collapse the two turns
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn turn_context_sets_the_model_for_following_usage() {
        let mut st = FileState::default();
        assert!(parse_codex_line(
            &line(r#"{"type":"turn_context","payload":{"model":"gpt-5.6"}}"#),
            &mut st,
            "s"
        )
        .is_none());
        let ev = parse_codex_line(
            &line(
                r#"{"timestamp":"2026-09-03T15:29:47.868Z","type":"event_msg","payload":{"type":"token_count",
                   "info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":1}}}}"#,
            ),
            &mut st,
            "s",
        )
        .unwrap();
        assert_eq!(ev.model, "gpt-5.6");
    }

    // Claude Code reports how much of its cache write went into a 1-hour cache,
    // and Anthropic bills those at 2x base input against 1.25x for 5-minute ones.
    // Lumping them together prices every write at the 5-minute rate.
    #[test]
    fn claude_splits_cache_writes_by_ttl() {
        let ev = parse_claude_line(&line(
            r#"{"type":"assistant","uuid":"u1","timestamp":"2026-09-24T01:00:00.000Z","sessionId":"s1",
                "message":{"id":"m1","model":"claude-opus-5",
                           "usage":{"input_tokens":2,"output_tokens":317,
                                    "cache_creation_input_tokens":1001,
                                    "cache_read_input_tokens":332079,
                                    "cache_creation":{"ephemeral_5m_input_tokens":0,
                                                      "ephemeral_1h_input_tokens":1001}}}}"#,
        ))
        .unwrap();
        assert_eq!((ev.cc, ev.cc_1h), (0.0, 1001.0));
    }

    #[test]
    fn claude_cache_write_split_is_optional_and_bounded_by_the_total() {
        // No breakdown (an older build): the whole total stays on the 5-minute
        // rate, exactly as it was priced before the split existed.
        let plain = parse_claude_line(&line(
            r#"{"type":"assistant","uuid":"u1","timestamp":"2026-09-24T01:00:00.000Z","sessionId":"s1",
                "message":{"id":"m1","model":"claude-opus-5",
                           "usage":{"input_tokens":1,"output_tokens":1,
                                    "cache_creation_input_tokens":500,
                                    "cache_read_input_tokens":10}}}"#,
        ))
        .unwrap();
        assert_eq!((plain.cc, plain.cc_1h), (500.0, 0.0));
        // A breakdown can't bill more 1-hour tokens than the total it is a share of.
        assert_eq!(split_cache_write(500.0, 900.0), (0.0, 500.0));
        assert_eq!(split_cache_write(0.0, 0.0), (0.0, 0.0));
    }

    // Oh My Pi reports the same split as `usage.cttl.ephemeral1h`.
    #[test]
    fn omp_splits_cache_writes_by_ttl() {
        let ev = parse_omp_line(
            &line(
                r#"{"type":"message","id":"m1","timestamp":1769389687056,
                    "message":{"role":"assistant","model":"claude-sonnet-5",
                               "usage":{"input":2,"output":478,"cacheRead":0,
                                        "cacheWrite":74546,"cttl":{"ephemeral1h":74546}}}}"#,
            ),
            "s1",
        )
        .unwrap();
        assert_eq!((ev.cc, ev.cc_1h), (0.0, 74546.0));
    }

    #[test]
    fn opencode_message_maps_all_four_token_classes() {
        let mut st = FileState::default();
        let ev = parse_opencode_message(
            &line(
                r#"{"id":"msg_bf7d80110001u0SbKmQxwDwOKt","sessionID":"ses_408","role":"assistant",
                    "time":{"created":1769389687056},"modelID":"claude-sonnet-4-5","providerID":"anthropic",
                    "cost":0,"tokens":{"input":6,"output":217,"reasoning":3,"cache":{"read":20365,"write":2909}}}"#,
            ),
            &mut st,
        )
        .unwrap();
        assert_eq!(ev.in_tok, 6.0);
        assert_eq!(ev.out_tok, 220.0); // reasoning folds into output
        assert_eq!(ev.cr, 20365.0);
        assert_eq!(ev.cc, 2909.0);
        assert_eq!(ev.model, "claude-sonnet-4-5");
        assert_eq!(ev.session, "ses_408");
        assert_eq!(ev.ts_ms, 1769389687056);
        // user messages carry no usage and must not become events
        assert!(parse_opencode_message(
            &line(r#"{"id":"msg_x","sessionID":"ses_408","role":"user"}"#),
            &mut st
        )
        .is_none());
    }

    #[test]
    fn omp_message_reads_usage_and_survives_iso_timestamps() {
        let ev = parse_omp_line(
            &line(
                r#"{"type":"message","id":"92eab999","timestamp":"2026-09-23T10:38:10.617Z",
                    "message":{"role":"assistant","model":"claude-sonnet-5","provider":"anthropic",
                    "timestamp":1790159886476,
                    "usage":{"input":2,"output":364,"cacheRead":0,"cacheWrite":8333,"totalTokens":8699}}}"#,
            ),
            "2026-09-23T10-36-49-393Z_01a0cdd6",
        )
        .unwrap();
        assert_eq!(ev.in_tok, 2.0);
        assert_eq!(ev.out_tok, 364.0);
        assert_eq!(ev.cr, 0.0);
        assert_eq!(ev.cc, 8333.0);
        assert_eq!(ev.ts_ms, 1790159886476); // message.timestamp wins over the entry's string
        assert_eq!(ev.model, "claude-sonnet-5");
        assert!(parse_omp_line(&line(r#"{"type":"title","title":"x"}"#), "s").is_none());
    }

    #[test]
    fn opencode_sqlite_rows_are_ingested_and_replace_on_update() {
        use rusqlite::params;
        let dir = std::env::temp_dir().join(format!("tokenscope-oc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
               id text PRIMARY KEY, session_id text NOT NULL,
               time_created integer NOT NULL, time_updated integer NOT NULL,
               data text NOT NULL)",
        )
        .unwrap();
        // `data` holds the message document without id/sessionID — those live in
        // columns (opencode's `info(row) = { ...row.data, id, sessionID }`).
        let data = |input: u64, output: u64| {
            format!(
                r#"{{"role":"assistant","time":{{"created":1769389687056}},
                     "modelID":"claude-sonnet-5","providerID":"anthropic",
                     "tokens":{{"input":{input},"output":{output},"reasoning":3,
                                "cache":{{"read":20365,"write":2909}}}}}}"#
            )
        };
        let put = |id: &str, updated: i64, d: &str| {
            conn.execute(
                "INSERT OR REPLACE INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, "ses_db", 1769389687056i64, updated, d],
            )
            .unwrap();
        };
        put("msg_a", 1, &data(6, 217));
        put("msg_b", 2, &data(1, 5));

        let mut s = Store {
            events: Vec::new(),
            index: HashMap::new(),
            manifest: Manifest::default(),
        };
        assert!(s.ingest_opencode_db_at(&db));
        assert_eq!(s.events.len(), 2);
        let a = s.events.iter().find(|e| e.id == "oc:msg_a").unwrap();
        assert_eq!(a.session, "ses_db"); // lifted from the column
        assert_eq!(a.tool, TOOL_OPENCODE);
        assert_eq!(a.in_tok, 6.0);
        assert_eq!(a.out_tok, 220.0); // 217 + reasoning 3
        assert_eq!(a.cc, 2909.0);
        assert_eq!(a.cr, 20365.0);
        assert_eq!(a.ts_ms, 1769389687056); // from data.time.created

        // Streaming completes: the row is rewritten in place with bigger numbers.
        put("msg_a", 99, &data(6, 900));
        assert!(s.ingest_opencode_db_at(&db));
        assert_eq!(s.events.len(), 2, "an updated row must replace, not duplicate");
        let a2 = s.events.iter().find(|e| e.id == "oc:msg_a").unwrap();
        assert_eq!(a2.out_tok, 903.0);

        // Nothing new → nothing to report (and no watermark churn).
        assert!(!s.ingest_opencode_db_at(&db));

        drop(conn);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_preserves_totals_and_is_idempotent() {
        use chrono::{NaiveDate, TimeZone};
        let now = Local::now();
        let today = now.date_naive();
        let yesterday = today - chrono::Duration::days(1);
        let at = |d: NaiveDate, h: u32| -> i64 {
            Local
                .from_local_datetime(&d.and_hms_opt(h, 0, 0).unwrap())
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        let mkev = |ts: i64, id: &str| RawEvent {
            ts_ms: ts,
            session: "s1".into(),
            model: "m".into(),
            in_tok: 10.0,
            cc: 1.0,
            cc_1h: 0.0,
            cr: 2.0,
            out_tok: 3.0,
            n: 1,
            mcp: vec!["srv".into()],
            skills: Vec::new(),
            id: id.into(),
            source: "/log/f.jsonl".into(),
            tool: "omp".into(),
        };
        let mut s = Store {
            events: Vec::new(),
            index: HashMap::new(),
            manifest: Manifest::default(),
        };
        s.push_event(mkev(at(yesterday, 10), "a"));
        s.push_event(mkev(at(yesterday, 11), "b"));
        s.push_event(mkev(at(today, 1), "c"));

        assert!(s.compact(today));
        // two yesterday messages collapse into one day row; today stays raw
        assert_eq!(s.events.len(), 2);
        let agg = s
            .events
            .iter()
            .find(|e| e.id.starts_with(AGG_PREFIX))
            .expect("expected a compacted day row");
        assert_eq!(agg.in_tok, 20.0);
        assert_eq!(agg.cc, 2.0);
        assert_eq!(agg.cr, 4.0);
        assert_eq!(agg.out_tok, 6.0);
        assert_eq!(agg.n, 2); // request counts survive compaction
        assert_eq!(agg.mcp.len(), 2); // so do the MCP call lists
        assert_eq!(agg.ts_ms, at(yesterday, 0)); // stamped at local midnight
        assert_eq!(agg.session, "s1");
        assert!(s.events.iter().any(|e| e.id == "c" && e.ts_ms == at(today, 1)));

        // Re-running must be a no-op, not a second fold into the same row.
        assert!(!s.compact(today));
        let again = s
            .events
            .iter()
            .find(|e| e.id.starts_with(AGG_PREFIX))
            .unwrap();
        assert_eq!(again.in_tok, 20.0);
        assert_eq!(again.n, 2);

        // A later raw event for the same day accumulates into the existing row.
        s.push_event(mkev(at(yesterday, 23), "d"));
        assert!(s.compact(today));
        let third = s
            .events
            .iter()
            .find(|e| e.id.starts_with(AGG_PREFIX))
            .unwrap();
        assert_eq!(third.in_tok, 30.0);
        assert_eq!(third.n, 3);
        assert_eq!(s.events.len(), 2);
    }

    #[test]
    fn session_key_groups_every_agent_file_of_one_conversation() {
        let main = Path::new(
            "/Users/x/.omp/agent/sessions/-proj/2026-09-23T10-36-49-393Z_01a0cdd6.jsonl",
        );
        let sub = Path::new(
            "/Users/x/.omp/profiles/deepseek/agent/sessions/-proj/2026-09-23T10-36-49-393Z_01a0cdd6/__advisor.scout.jsonl",
        );
        assert_eq!(omp_session_from_path(main), "2026-09-23T10-36-49-393Z_01a0cdd6");
        assert_eq!(omp_session_from_path(main), omp_session_from_path(sub));
    }
}
