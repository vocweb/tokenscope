// Parse the agent logs ingested by store.rs (Claude Code, Codex CLI, opencode,
// Oh My Pi), dedupe assistant messages by id, classify tool calls
// (user-installed MCP / Skill only), and aggregate into Day / Week / Month
// reports + a daily heatmap.
use crate::config::UserConfig;
use crate::model::*;
use crate::pricing::{canonical_id, Pricing};
use crate::store::{RawEvent, Store, TOOL_CLAUDE, TOOL_CODEX, TOOL_OMP, TOOL_OPENCODE, TOOL_PI};
use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;

// One lock guards the store *and* serializes dashboard builds, so the background
// refresh thread, the filesystem watcher and the command handler never touch it
// concurrently. The store stays resident for the life of the process: builds
// used to `Store::load()` every time, which re-read and re-parsed the whole
// cache (several MB of JSON) on every refresh — and refreshes are driven by log
// writes.
static STORE: Mutex<Option<Store>> = Mutex::new(None);

// Persistence is decoupled from the refresh rate. The cache is derived data that
// can be rebuilt from the logs, so it is checkpointed every few minutes — plus
// at once on the first change of a process (see below), and on exit (see
// `flush`) — instead of on every refresh. Rewriting the whole cache per refresh
// is what made the app dirty ~8.6 GB in one afternoon; macOS then throttles a
// process's writes (its "disk writes" resource limit) and the refresh loop,
// blocked inside `write()`, stops updating the panel.
//
// 0 means "nothing checkpointed yet in this process", so the first change is
// always persisted immediately. That also covers a cold store, which has just
// re-read every log and would otherwise be re-read again on the next launch.
static LAST_CHECKPOINT_MS: AtomicI64 = AtomicI64::new(0);
static UNSAVED: AtomicBool = AtomicBool::new(false);
const CHECKPOINT_EVERY_MS: i64 = 15 * 60 * 1000;

// One assistant API response, with config + pricing applied (derived per request
// from a RawEvent, since user config / prices / time windows can all change).
struct Event {
    ts: DateTime<Local>,
    session: String,
    tool: String, // source CLI (TOOL_*) — the by-agent breakdown
    model: String,
    input: f64,          // raw tokens, uncached new input only
    cache: f64,          // raw tokens, cache creation + read
    output: f64,         // raw tokens
    cost: f64,           // USD (differentiated by token type), 0 if unknown model
    priced: bool,        // whether a price was found for this model
    n: u64,              // API requests this row represents (1, or a compacted day's sum)
    mcp: Vec<String>,    // user-installed server names called in this msg
    skills: Vec<String>, // user-installed skill names called in this msg
}

/// Display name per source id. Unknown ids fall back to the raw id, so a cache
/// written by a newer build never renders a blank row.
fn tool_label(tool: &str) -> &str {
    match tool {
        TOOL_CLAUDE => "Claude Code",
        TOOL_CODEX => "Codex",
        TOOL_OPENCODE => "opencode",
        TOOL_OMP => "Oh My Pi",
        TOOL_PI => "pi",
        other => other,
    }
}

/// Fixed render order for the by-agent breakdown (biggest installed base first),
/// so rows don't reshuffle as usage shifts between periods.
const TOOL_ORDER: [&str; 5] = [TOOL_CLAUDE, TOOL_CODEX, TOOL_OPENCODE, TOOL_OMP, TOOL_PI];

// Top-5 models keep the green/slate scheme; everything beyond is uniform gray.
const PALETTE: &[&str] = &["#1f9d63", "#34c27e", "#6ad0a0", "#a7e3c5", "#4b5a52"];
const OVERFLOW_GRAY: &str = "#79817b";

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Group key for a model. Agents spell the same model differently — Codex logs
/// "gpt-5.5" where Oh My Pi logs "openai.gpt-5.5" or "global.openai.gpt-5.6-sol"
/// — so the provider namespace comes off before grouping, otherwise one model
/// shows up as several rows. A trailing "-YYYYMMDD" date suffix also merges into
/// its base model (e.g. "claude-haiku-4-5-20251001" → "claude-haiku-4-5").
fn normalize_model(name: &str) -> String {
    let base = canonical_id(name);
    if let Some(idx) = base.rfind('-') {
        let suffix = &base[idx + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            return base[..idx].to_string();
        }
    }
    base
}

fn vendor_of(model: &str) -> &'static str {
    let m = model.to_lowercase();
    if m.contains("claude") {
        "Anthropic"
    } else if m.contains("gpt") || m.contains("o1") || m.contains("o3") {
        "OpenAI"
    } else if m.contains("gemini") {
        "Google"
    } else if m.contains("llama") {
        "Local"
    } else if m.contains("glm") {
        "Zhipu"
    } else if m.contains("deepseek") {
        "DeepSeek"
    } else {
        "Other"
    }
}

pub fn build_dashboard() -> Dashboard {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    // Loaded once per process; every later build reuses it.
    let store = guard.get_or_insert_with(Store::load);

    // 1. Ingest incrementally (full scan only on first run; afterwards just the
    //    appended lines), prune events older than the heatmap window, and
    //    checkpoint on a schedule rather than on every refresh.
    let mut dirty = store.ingest();
    // Sampled once so every window in this build agrees on the current date.
    let now = Local::now();
    let today = now.date_naive();
    // Reports/heatmap span ~26 weeks (+ prev month); 210 days leaves margin.
    let cutoff = (now - Duration::days(210)).timestamp_millis();
    if store.prune_before(cutoff) {
        dirty = true;
    }
    // Fold finished days into one row per (day, tool, session, model) before
    // saving: the dashboard only needs sub-day resolution for today, and an
    // agent logging tens of thousands of requests a day would otherwise push the
    // persisted cache into the hundreds of MB.
    if store.compact(today) {
        dirty = true;
    }
    if dirty {
        let now_ms = now.timestamp_millis();
        let due = now_ms - LAST_CHECKPOINT_MS.load(Ordering::Relaxed) >= CHECKPOINT_EVERY_MS;
        // Past the schedule, write it out; before that, only remember that the
        // cache is behind, so an exit can still persist it (see `flush`).
        if due {
            store.save();
            LAST_CHECKPOINT_MS.store(now_ms, Ordering::Relaxed);
            UNSAVED.store(false, Ordering::Relaxed);
        } else {
            UNSAVED.store(true, Ordering::Relaxed);
        }
    }

    // 2. Aggregate: apply current config + prices, slice by current time.
    let cfg = UserConfig::load();
    // Memoized price table (cheap clone); loaded/refreshed off-thread elsewhere
    // so neither parsing nor the network runs while we hold the store lock.
    let pricing = Pricing::shared();
    let events: Vec<Event> = store
        .events
        .iter()
        .map(|r| compute_event(r, &cfg, &pricing))
        .collect();

    let mut day = report_day(&events, now);
    let mut week = report_week(&events, now);
    let mut month = report_month(&events, now);
    let mut five_hour = report_five_hour(&events, now);
    let heatmap = build_heatmap(&events, today);

    // "servers"/"skills" = how many the user has *installed* (global, constant
    // across periods), not how many were called in the window.
    let installed_servers = cfg.mcp_servers.len() as u64;
    let installed_skills = cfg.skills.len() as u64;
    for r in [&mut day, &mut week, &mut month, &mut five_hour] {
        r.metrics.servers = installed_servers;
        r.metrics.skills = installed_skills;
    }

    // today's displayed tokens (M) for the tray
    let today_tokens: f64 = events
        .iter()
        .filter(|e| e.ts.date_naive() == today)
        .map(|e| (e.input + e.cache + e.output) / 1e6)
        .sum();

    Dashboard {
        day,
        week,
        month,
        five_hour,
        limits: crate::limits::shared().as_ref().clone(),
        heatmap,
        today_tokens,
        generated_at: now.to_rfc3339(),
    }
}

/// Write the store out if a refresh left changes uncheckpointed. Called when the
/// app exits, so a clean quit never makes the next launch re-read everything
/// logged since the last checkpoint.
///
/// A no-op when nothing is pending, which keeps the exit path free of a
/// multi-megabyte write on an idle app. It also uses `try_lock` rather than
/// `lock`: this runs on the event thread while quitting, and a build already in
/// flight (a cold rebuild re-reads every log) must not hold up the exit.
/// Skipping the write costs the next launch a re-read of the logs, nothing more.
pub fn flush() {
    if !UNSAVED.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Ok(mut guard) = STORE.try_lock() {
        if let Some(store) = guard.as_mut() {
            store.save();
        }
    }
}

/// Derive a computed Event from a stored RawEvent, applying the *current* user
/// config (MCP/Skill whitelist) and prices. This is why these aren't baked into
/// the store: installing an MCP or a price refresh applies retroactively.
fn compute_event(r: &RawEvent, cfg: &UserConfig, pricing: &Pricing) -> Event {
    let ts = DateTime::from_timestamp_millis(r.ts_ms)
        .unwrap_or_default()
        .with_timezone(&Local);
    let model = normalize_model(&r.model);
    // price lookup uses the raw (possibly dated) id, then the normalized one
    let cost_opt = pricing
        .cost(&r.model, r.in_tok, r.out_tok, r.cc, r.cc_1h, r.cr)
        .or_else(|| pricing.cost(&model, r.in_tok, r.out_tok, r.cc, r.cc_1h, r.cr));
    let mcp = r.mcp.iter().filter_map(|s| cfg.resolve_mcp(s)).collect();
    let skills = r
        .skills
        .iter()
        .filter(|s| cfg.is_user_skill(s))
        .map(|s| s.rsplit(':').next().unwrap_or(s).to_string())
        .collect();
    Event {
        ts,
        session: r.session.clone(),
        tool: if r.tool.is_empty() {
            TOOL_CLAUDE.to_string() // pre-v5 cache entries came from Claude Code
        } else {
            r.tool.clone()
        },
        model,
        input: r.in_tok,
        // Cache creation has two TTL buckets (5-minute and 1-hour) and they are
        // stored separately because they bill differently — but they are both
        // cache-write tokens, so the displayed total has to add both. Dropping
        // `cc_1h` here silently undercounts every agent that reports a 1-hour
        // share (Claude Code and Oh My Pi; both of theirs are mostly 1-hour).
        cache: r.cc + r.cc_1h + r.cr,
        output: r.out_tok,
        cost: cost_opt.unwrap_or(0.0),
        priced: cost_opt.is_some(),
        n: r.n,
        mcp,
        skills,
    }
}

// ── aggregation helpers ────────────────────────────────────────────
#[derive(Default)]
struct Agg {
    input: f64,
    cache: f64,
    output: f64,
    cost: f64,
    requests: u64,
    sessions: HashSet<String>,
    mcp_calls: u64,
    skill_calls: u64,
    model_tok: HashMap<String, f64>,
    model_cost: HashMap<String, f64>,
    model_priced: HashMap<String, bool>,
    mcp_counts: HashMap<String, u64>,
    skill_counts: HashMap<String, u64>,
    // per-source-CLI split (Claude Code / Codex / opencode / Oh My Pi)
    tool_tok: HashMap<String, f64>,
    tool_cost: HashMap<String, f64>,
    tool_req: HashMap<String, u64>,
    tool_sessions: HashMap<String, HashSet<String>>,
}

impl Agg {
    fn add(&mut self, e: &Event) {
        self.input += e.input;
        self.cache += e.cache;
        self.output += e.output;
        self.cost += e.cost;
        if !e.session.is_empty() {
            self.sessions.insert(e.session.clone());
        }
        // Slash-command skill events carry no model (empty) — they're not LLM
        // requests, so they must not inflate request counts or the model split.
        if !e.model.is_empty() {
            self.requests += e.n;
            // model totals keep all token types so shares sum to Total tokens
            *self.model_tok.entry(e.model.clone()).or_default() += e.input + e.cache + e.output;
            *self.model_cost.entry(e.model.clone()).or_default() += e.cost;
            // a model is "priced" if any of its messages had a known price
            *self.model_priced.entry(e.model.clone()).or_default() |= e.priced;
            // same totals, sliced by source CLI
            *self.tool_tok.entry(e.tool.clone()).or_default() += e.input + e.cache + e.output;
            *self.tool_cost.entry(e.tool.clone()).or_default() += e.cost;
            *self.tool_req.entry(e.tool.clone()).or_default() += e.n;
            if !e.session.is_empty() {
                self.tool_sessions
                    .entry(e.tool.clone())
                    .or_default()
                    .insert(e.session.clone());
            }
        }
        for s in &e.mcp {
            self.mcp_calls += 1;
            *self.mcp_counts.entry(s.clone()).or_default() += 1;
        }
        for s in &e.skills {
            self.skill_calls += 1;
            *self.skill_counts.entry(s.clone()).or_default() += 1;
        }
    }

    fn models(&self) -> Vec<ModelStat> {
        let mut v: Vec<(String, f64, f64)> = self
            .model_tok
            .iter()
            .map(|(k, t)| (k.clone(), *t, *self.model_cost.get(k).unwrap_or(&0.0)))
            .collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter()
            .enumerate()
            .map(|(i, (name, tok, cost))| {
                let priced = *self.model_priced.get(&name).unwrap_or(&false);
                ModelStat {
                    vendor: vendor_of(&name).to_string(),
                    tokens: (tok / 1e6 * 100.0).round() / 100.0,
                    cost: (cost * 100.0).round() / 100.0,
                    color: if i < PALETTE.len() {
                        PALETTE[i]
                    } else {
                        OVERFLOW_GRAY
                    }
                    .to_string(),
                    priced,
                    name,
                }
            })
            .collect()
    }

    /// Per-source breakdown in a fixed order, so a row keeps its place as the
    /// mix shifts; sources with no model-bearing requests in the window are
    /// omitted (an installed-but-unused CLI shouldn't take a row).
    fn tools(&self) -> Vec<ToolStat> {
        let mut v: Vec<ToolStat> = self
            .tool_tok
            .iter()
            .map(|(k, t)| ToolStat {
                name: k.clone(),
                label: tool_label(k).to_string(),
                tokens: (t / 1e6 * 100.0).round() / 100.0,
                cost: (self.tool_cost.get(k).unwrap_or(&0.0) * 100.0).round() / 100.0,
                requests: *self.tool_req.get(k).unwrap_or(&0),
                sessions: self
                    .tool_sessions
                    .get(k)
                    .map(|s| s.len() as u64)
                    .unwrap_or(0),
            })
            .collect();
        v.sort_by_key(|t| {
            TOOL_ORDER
                .iter()
                .position(|x| *x == t.name)
                .unwrap_or(usize::MAX)
        });
        v
    }

    fn named(counts: &HashMap<String, u64>) -> Vec<NamedCount> {
        let mut v: Vec<NamedCount> = counts
            .iter()
            .map(|(k, c)| NamedCount {
                name: k.clone(),
                count: *c,
            })
            .collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.count));
        v
    }

    fn metrics(&self, delta_tokens: f64, delta_cost: f64) -> Metrics {
        Metrics {
            total_tokens: ((self.input + self.cache + self.output) / 1e6 * 100.0).round() / 100.0,
            input_tokens: (self.input / 1e6 * 100.0).round() / 100.0,
            cache_tokens: (self.cache / 1e6 * 100.0).round() / 100.0,
            output_tokens: (self.output / 1e6 * 100.0).round() / 100.0,
            cost: (self.cost * 100.0).round() / 100.0,
            mcp_calls: self.mcp_calls,
            skill_calls: self.skill_calls,
            requests: self.requests,
            sessions: self.sessions.len() as u64,
            delta_tokens,
            delta_cost,
            servers: self.mcp_counts.len() as u64,
            skills: self.skill_counts.len() as u64,
        }
    }
}

/// Percentage change of `cur` vs `prev`, e.g. +20.0 for a 20% increase,
/// rounded to 2 decimals. Returns 0 when there's no baseline to compare.
fn pct_delta(cur: f64, prev: f64) -> f64 {
    if prev <= 0.0 {
        return 0.0;
    }
    ((cur - prev) / prev * 10000.0).round() / 100.0
}

// ── Day report: today, 24 hourly buckets ───────────────────────────
fn report_day(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    let today = now.date_naive();
    let yesterday = today - Duration::days(1);
    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); 24]; // (input, cache, output) M
    let mut req_b = vec![0.0f64; 24];
    let mut cost_b = vec![0.0f64; 24];

    for e in events {
        let d = e.ts.date_naive();
        if d == today {
            agg.add(e);
            let h = e.ts.hour() as usize;
            buckets[h].0 += e.input / 1e6;
            buckets[h].1 += e.cache / 1e6;
            buckets[h].2 += e.output / 1e6;
            // Match Agg::add exactly: only the request COUNT excludes model-less
            // (slash-command) events; total cost accumulates unconditionally
            // (those events carry cost 0, so this is identical today).
            if !e.model.is_empty() {
                req_b[h] += e.n as f64;
            }
            cost_b[h] += e.cost;
        } else if d == yesterday {
            prev.add(e);
        }
    }

    let series = (0..24)
        .map(|h| SeriesPoint {
            // axis ticks every 4h, skipping the 00/24 endpoints
            label: if h % 4 == 0 && h != 0 {
                format!("{:02}", h)
            } else {
                String::new()
            },
            full: format!("{:02}:00", h),
            input: buckets[h].0,
            cache: buckets[h].1,
            output: buckets[h].2,
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        tools: agg.tools(),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
    }
}

// ── Week report: current calendar week (Mon-Sun) vs previous week ────
fn report_week(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    let today = now.date_naive();
    // Monday of the current week (Mon=0 … Sun=6).
    let start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let next_start = start + Duration::days(7);
    let prev_start = start - Duration::days(7);

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = [(0.0f64, 0.0f64, 0.0f64); 7];
    let mut req_b = vec![0.0f64; 7];
    let mut cost_b = vec![0.0f64; 7];

    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d < next_start {
            agg.add(e);
            let idx = (d - start).num_days() as usize;
            if idx < buckets.len() {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += e.n as f64;
                }
                cost_b[idx] += e.cost;
            }
        } else if d >= prev_start && d < start {
            prev.add(e);
        }
    }

    let weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let series = (0..7usize)
        .map(|i| {
            let date = start + Duration::days(i as i64);
            let wd = weekday[i];
            SeriesPoint {
                label: wd.to_string(),
                full: format!(
                    "{} {} {}",
                    wd,
                    MONTHS[(date.month() - 1) as usize],
                    date.day()
                ),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        tools: agg.tools(),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
    }
}

// ── Month report: current calendar month vs previous calendar month ──
fn report_month(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    use chrono::NaiveDate;
    let today = now.date_naive();
    let (y, m) = (today.year(), today.month());
    let cur_first = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let next_first = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1).unwrap()
    };
    let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    let prev_first = NaiveDate::from_ymd_opt(py, pm, 1).unwrap();
    let days_in_month = (next_first - cur_first).num_days() as usize;

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); days_in_month];
    let mut req_b = vec![0.0f64; days_in_month];
    let mut cost_b = vec![0.0f64; days_in_month];

    for e in events {
        let d = e.ts.date_naive();
        if d >= cur_first && d < next_first {
            agg.add(e);
            let idx = (d - cur_first).num_days() as usize;
            if idx < buckets.len() {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += e.n as f64;
                }
                cost_b[idx] += e.cost;
            }
        } else if d >= prev_first && d < cur_first {
            prev.add(e);
        }
    }

    let series = (0..days_in_month)
        .map(|i| {
            let dn = (i + 1) as u32;
            let label = if i == 0 || dn.is_multiple_of(5) {
                dn.to_string()
            } else {
                String::new()
            };
            SeriesPoint {
                label,
                full: format!("{} {}", MONTHS[(m - 1) as usize], dn),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        tools: agg.tools(),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
    }
}

// ── 5-hour window: the rolling block subscription plans meter ────────
/// Rolling last 5 hours in 20-minute buckets, compared against the 5 hours
/// before it. Plan limits ("5-hour session" / rolling usage windows) are
/// expressed in this window, and unlike the Day view it doesn't reset at
/// midnight — so it answers "how much of the current session block have I
/// burned?" even for a block that started yesterday evening.
///
/// The window is aligned to the next 20-minute wall-clock boundary so buckets
/// land on :00/:20/:40 and can be labelled; the tail beyond `now` is empty
/// because nothing is logged in the future.
fn report_five_hour(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    const BUCKETS: usize = 15; // 15 × 20 min = 5 h
    const STEP_MIN: i64 = 20;
    const WINDOW_MIN: i64 = BUCKETS as i64 * STEP_MIN;

    let to_boundary = (STEP_MIN - now.minute() as i64 % STEP_MIN) % STEP_MIN;
    let end = now + Duration::minutes(to_boundary);
    let start = end - Duration::minutes(WINDOW_MIN);
    let prev_start = start - Duration::minutes(WINDOW_MIN);

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); BUCKETS];
    let mut req_b = vec![0.0f64; BUCKETS];
    let mut cost_b = vec![0.0f64; BUCKETS];

    for e in events {
        if e.ts >= start && e.ts <= end {
            agg.add(e);
            let idx = ((e.ts - start).num_minutes() / STEP_MIN) as usize;
            if idx < BUCKETS {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += e.n as f64;
                }
                cost_b[idx] += e.cost;
            }
        } else if e.ts >= prev_start && e.ts < start {
            prev.add(e);
        }
    }

    let series = (0..BUCKETS)
        .map(|i| {
            let at = start + Duration::minutes(i as i64 * STEP_MIN);
            SeriesPoint {
                // an hourly tick per bucket that lands on the hour
                label: if at.minute() == 0 {
                    format!("{:02}", at.hour())
                } else {
                    String::new()
                },
                full: at.format("%H:%M").to_string(),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        tools: agg.tools(),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
    }
}

// ── Heatmap: last ~26 weeks daily totals ────────────────────────────
fn build_heatmap(events: &[Event], today: chrono::NaiveDate) -> Vec<HeatDay> {
    let start = today - Duration::days(25 * 7 + today.weekday().num_days_from_sunday() as i64);
    let mut by_day: HashMap<chrono::NaiveDate, f64> = HashMap::new();
    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d <= today {
            *by_day.entry(d).or_default() += (e.input + e.cache + e.output) / 1e6;
        }
    }
    let mut days = Vec::new();
    let mut d = start;
    let mut maxv = 0.0f64;
    while d <= today {
        let t = *by_day.get(&d).unwrap_or(&0.0);
        maxv = maxv.max(t);
        days.push((d, t));
        d += Duration::days(1);
    }
    days.into_iter()
        .map(|(date, tokens)| {
            let f = if maxv > 0.0 { tokens / maxv } else { 0.0 };
            let level = if tokens == 0.0 {
                0
            } else if f < 0.25 {
                1
            } else if f < 0.5 {
                2
            } else if f < 0.75 {
                3
            } else {
                4
            };
            HeatDay {
                date: date.format("%Y-%m-%d").to_string(),
                tokens: (tokens * 100.0).round() / 100.0,
                level,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: DateTime<Local>, input: f64, cache: f64, output: f64) -> Event {
        Event {
            ts,
            session: "s1".to_string(),
            tool: TOOL_OMP.to_string(),
            model: "gpt-5.5".to_string(),
            input,
            cache,
            output,
            cost: 0.0,
            priced: false,
            n: 1,
            mcp: Vec::new(),
            skills: Vec::new(),
        }
    }

    #[test]
    fn one_hour_cache_writes_count_towards_the_displayed_totals() {
        // `cc` is only the 5-minute share; the 1-hour share lives in `cc_1h` and
        // must still appear in the cache/total numbers the UI shows.
        let cfg = UserConfig {
            mcp_servers: HashSet::new(),
            skills: HashSet::new(),
        };
        let pricing = Pricing::shared();
        let raw = RawEvent {
            ts_ms: 0,
            session: "s1".to_string(),
            model: "claude-opus-5".to_string(),
            in_tok: 10.0,
            cc: 100.0,
            cc_1h: 50.0,
            cr: 1000.0,
            out_tok: 20.0,
            n: 1,
            mcp: Vec::new(),
            skills: Vec::new(),
            id: "m1".to_string(),
            source: String::new(),
            tool: TOOL_CLAUDE.to_string(),
        };
        let e = compute_event(&raw, &cfg, &pricing);
        assert_eq!(
            e.cache, 1150.0,
            "1-hour cache writes belong in the cache total"
        );
        assert_eq!(e.input + e.cache + e.output, 1180.0);
    }

    #[test]
    fn an_oh_my_pi_mcp_call_survives_the_installed_server_filter() {
        // Oh My Pi flattens the call into one name; it must still resolve to the
        // server the user installed, or the MCP breakdown reads zero for a user
        // who only works in Oh My Pi.
        let cfg = UserConfig {
            mcp_servers: ["chrome-devtools"].iter().map(|s| s.to_string()).collect(),
            skills: HashSet::new(),
        };
        let pricing = Pricing::shared();
        let raw = RawEvent {
            ts_ms: 0,
            session: "s1".to_string(),
            model: "claude-opus-5".to_string(),
            in_tok: 1.0,
            cc: 0.0,
            cc_1h: 0.0,
            cr: 0.0,
            out_tok: 1.0,
            n: 1,
            mcp: vec!["chrome_devtools_navigate_page".to_string()],
            skills: Vec::new(),
            id: "m2".to_string(),
            source: String::new(),
            tool: TOOL_OMP.to_string(),
        };
        let e = compute_event(&raw, &cfg, &pricing);
        assert_eq!(e.mcp, vec!["chrome-devtools"]);
    }

    #[test]
    fn model_grouping_merges_provider_namespaces_across_agents() {
        // the same model as logged by Codex and by Oh My Pi must be one row
        assert_eq!(normalize_model("openai.gpt-5.5"), "gpt-5.5");
        assert_eq!(normalize_model("gpt-5.5"), "gpt-5.5");
        assert_eq!(normalize_model("global.openai.gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(normalize_model("anthropic.claude-opus-5"), "claude-opus-5");
        // dated releases merge into their base model
        assert_eq!(
            normalize_model("claude-haiku-4-5-20251001"),
            "claude-haiku-4-5"
        );
        // but a version dot or quantization tag is part of the model name
        assert_eq!(normalize_model("glm-5.1"), "glm-5.1");
        assert_eq!(normalize_model("qwen3.8-27b-mlx@4bit"), "qwen3.8-27b-mlx");
    }

    #[test]
    fn five_hour_window_rolls_and_compares_with_the_previous_block() {
        let now = Local::now();
        // 3 M inside the window, 2 M in the 5 h before it, 100 M older than both
        let inside = ev(now - Duration::minutes(30), 2e6, 0.0, 1e6);
        let previous = ev(now - Duration::minutes(7 * 60), 1e6, 0.0, 1e6);
        let ancient = ev(now - Duration::minutes(11 * 60), 100e6, 0.0, 0.0);
        let r = report_five_hour(&[inside, previous, ancient], now);

        assert_eq!(r.metrics.total_tokens, 3.0);
        assert_eq!(r.metrics.requests, 1); // neither older event is a request in the window
                                           // delta is measured against the previous 5 h block, not the previous day
        assert_eq!(r.metrics.delta_tokens, 50.0);
        // per-model split works for the window too (this is what "count each
        // model" rides on)
        assert_eq!(r.models.len(), 1);
        assert_eq!(r.models[0].name, "gpt-5.5");
        assert_eq!(r.models[0].tokens, 3.0);
        // every token landed in exactly one 20-minute bucket
        assert_eq!(r.series.len(), 15);
        let bucketed: f64 = r.series.iter().map(|p| p.input + p.cache + p.output).sum();
        assert!((bucketed - 3.0).abs() < 1e-9);
        assert_eq!(r.req_trend.iter().sum::<f64>(), 1.0);
    }

    #[test]
    fn five_hour_buckets_align_to_twenty_minutes_and_label_on_the_hour() {
        let now = Local::now();
        let r = report_five_hour(&[], now);
        assert_eq!(r.series.len(), 15);
        for p in &r.series {
            // aligned to :00/:20/:40 whatever the wall clock is right now
            assert!(
                p.full.ends_with(":00") || p.full.ends_with(":20") || p.full.ends_with(":40"),
                "bucket starts at {}",
                p.full
            );
            // only the hour marks carry an axis label
            if p.full.ends_with(":00") {
                assert_eq!(p.label, p.full[..2]);
            } else {
                assert!(p.label.is_empty());
            }
        }
    }
}
