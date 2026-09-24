// Plan-usage windows (5-hour / weekly / monthly) as the *provider* reports them.
// This answers "how much of my quota is gone?", which is a different question
// from "how many tokens did I log" — a subscription meters a rolling window and
// only the provider knows how full it is.
//
// Two read-only sources, merged (live wins per window):
//
//   1. The providers' own usage endpoints, each with the credential it wants:
//      * opencode Go/Zen — GET https://opencode.ai/zen/go/v1/usage with the API
//        key as a Bearer token (endpoint/shape from the same provider registry
//        Oh My Pi uses):
//          { usage: { rolling|weekly|monthly: { percent, status, resetsAt } } }
//      * Claude Code — GET https://api.anthropic.com/api/oauth/usage with the
//        OAuth access token (+ anthropic-beta: oauth-2025-04-20):
//          { five_hour|seven_day|seven_day_opus|seven_day_sonnet: { utilization,
//            resets_at }, limits: [{ kind, group, percent, severity, resets_at,
//            scope.model.display_name }] }
//   2. Oh My Pi's own poller: `~/.omp/**/agent.db` → `usage_history` keeps the
//      newest reported window per (provider, limit). Covers providers whose
//      credential this app can't reach, and covers the gaps between fetches.
//
// The Anthropic OAuth token is tried in order — environment, Claude Code's
// macOS keychain item, ~/.claude/.credentials.json, then Oh My Pi's store — and
// the first one the usage endpoint accepts wins. The copies do not expire
// together (Oh My Pi's can be an expired one-hour token while Claude Code's own
// keychain copy is still valid), so stopping at the first token *found* is what
// left the Claude rows frozen on a stale reading.
//
// Credentials are only ever read from the environment or a credential store and
// put in an Authorization header — never logged, never persisted by this app,
// never handed to the UI.
use chrono::Utc;
use rusqlite::{types::Value as SqlValue, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use crate::store::opencode_data_dir;

const OPENCODE_GO_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
/// Claude Code's own usage endpoint (the same one Oh My Pi calls for the
/// Anthropic plan windows). Needs the OAuth access token, not an API key.
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// How long a fetched snapshot is considered current. The windows reset on the
/// order of hours, so re-fetching every few minutes would only burn requests.
const FRESH: Duration = Duration::from_secs(15 * 60);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UsageLimit {
    pub provider: String,
    /// Window name as the provider/UI labels it ("5 Hour", "Weekly", "Monthly").
    pub window: String,
    pub label: String,
    pub used: f64, // percent 0..100
    pub status: String, // ok | warning | exhausted | rate-limited
    #[serde(rename = "resetsAt")]
    pub resets_at_ms: i64,
    #[serde(rename = "observedAt")]
    pub observed_ms: i64,
    /// "api" (fetched by us) or "oh-my-pi" (last snapshot its poller recorded).
    pub source: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Cached {
    fetched_at: i64,
    items: Vec<UsageLimit>,
}

static LIMITS: LazyLock<RwLock<Arc<Vec<UsageLimit>>>> = LazyLock::new(|| {
    // Seed from the last snapshot on disk so a restart shows the numbers
    // immediately instead of blanking until the first (async) refresh lands.
    let items = read_cache().map(|c| c.items).unwrap_or_default();
    RwLock::new(Arc::new(items))
});

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn cache_path() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("tokenscope");
    let _ = fs::create_dir_all(&d);
    Some(d.join("limits.json"))
}

/// Every Oh My Pi agent dir (the root one plus one per profile), each of which
/// keeps its own usage_history — they're unioned, newest snapshot winning.
fn omp_agent_dbs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let base = home.join(".omp");
    let mut dbs = vec![base.join("agent").join("agent.db")];
    if let Ok(entries) = fs::read_dir(base.join("profiles")) {
        for e in entries.flatten() {
            dbs.push(e.path().join("agent").join("agent.db"));
        }
    }
    dbs.retain(|p| p.is_file());
    dbs
}

/// The opencode-go API key, in order of explicitness: an environment variable
/// the user sets for this app, then Oh My Pi's credential store (where the key
/// the user already added lives), then the opencode CLI's own store.
fn opencode_go_key() -> Option<String> {
    for var in ["OPENCODE_GO_API_KEY", "OPENCODE_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    for db in omp_agent_dbs() {
        let Ok(conn) = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT data FROM auth_credentials
             WHERE provider = 'opencode-go' AND credential_type = 'api_key'
               AND (disabled_cause IS NULL OR disabled_cause = '')
             ORDER BY updated_at DESC",
        ) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
            continue;
        };
        for data in rows.flatten() {
            if let Some(k) = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|v| v.get("key").and_then(|k| k.as_str()).map(str::to_string))
            {
                if !k.trim().is_empty() {
                    return Some(k);
                }
            }
        }
    }
    // opencode CLI's own store (`opencode auth login`), same dir the log ingest uses.
    let auth = opencode_data_dir()?.join("auth.json");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(auth).ok()?).ok()?;
    for id in ["opencode-go", "opencode"] {
        let v = json.get(id)?;
        let key = v
            .get("key")
            .or_else(|| v.get("apiKey"))
            .and_then(|k| k.as_str())
            .or_else(|| v.as_str());
        if let Some(k) = key {
            if !k.trim().is_empty() {
                return Some(k.to_string());
            }
        }
    }
    None
}

/// { "provider", "window", "label" } for each opencode-go window. Labels mirror
/// Oh My Pi's, so a live row and a stored row for the same window read alike.
const OPENCODE_WINDOWS: [(&str, &str, &str); 3] = [
    ("rolling", "5 Hour", "5 Hour limit"),
    ("weekly", "Weekly", "Weekly limit"),
    ("monthly", "Monthly", "Monthly limit"),
];

fn parse_opencode_usage(v: &serde_json::Value, observed_ms: i64) -> Option<Vec<UsageLimit>> {
    let usage = v.get("usage")?.as_object()?;
    let mut out = Vec::new();
    for (key, window, label) in OPENCODE_WINDOWS {
        let Some(w) = usage.get(key) else { continue };
        // `percent` is 0..100; a window without it is one we can't render.
        let Some(used) = w.get("percent").and_then(|p| p.as_f64()) else {
            continue;
        };
        out.push(UsageLimit {
            provider: "opencode-go".to_string(),
            window: window.to_string(),
            label: label.to_string(),
            used,
            status: w
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("ok")
                .to_string(),
            resets_at_ms: w
                .get("resetsAt")
                .and_then(|r| r.as_str())
                .and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|d| d.timestamp_millis())
                })
                .unwrap_or(0),
            observed_ms,
            source: "api".to_string(),
        });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn fetch_opencode_go(key: &str) -> Option<Vec<UsageLimit>> {
    let resp = ureq::get(OPENCODE_GO_USAGE_URL)
        .timeout(Duration::from_secs(10))
        .set("accept", "application/json")
        .set("authorization", &format!("Bearer {key}"))
        .call()
        .ok()?;
    let v: serde_json::Value = resp.into_json().ok()?;
    parse_opencode_usage(&v, now_ms())
}

/// The `accessToken` inside a Claude credential document. Claude Code's file and
/// its keychain item carry `{ claudeAiOauth: { accessToken, … } }`; Oh My Pi's
/// `auth_credentials` row carries `{ access, refresh, … }` at the top level.
fn claude_access_token(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let pick = |o: &serde_json::Value| {
        ["accessToken", "access_token", "access"]
            .iter()
            .find_map(|k| o.get(k).and_then(|t| t.as_str()))
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    };
    v.get("claudeAiOauth")
        .and_then(pick)
        .or_else(|| pick(&v))
}

/// Service names of Claude Code's keychain credential items, most likely first.
/// Claude Code suffixes the service with an account hash (`Claude
/// Code-credentials-48514145`), so the name has to be discovered rather than
/// assumed; the unsuffixed name stays first for older layouts.
#[cfg(target_os = "macos")]
fn keychain_credential_services() -> Vec<String> {
    const SERVICE: &str = "Claude Code-credentials";
    let mut found = vec![SERVICE.to_string()];
    let Ok(out) = Command::new("/usr/bin/security")
        .arg("dump-keychain")
        .output()
    else {
        return found;
    };
    if !out.status.success() {
        return found;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        //     "svce"<blob>="Claude Code-credentials-48514145"
        let Some(rest) = line.trim().strip_prefix("\"svce\"<blob>=\"") else {
            continue;
        };
        let Some(svce) = rest.strip_suffix('"') else { continue };
        if svce.starts_with(SERVICE) && !found.contains(&svce.to_string()) {
            found.push(svce.to_string());
        }
    }
    found
}

/// One keychain item's secret, via `/usr/bin/security`. Reading through the CLI
/// rather than the Security framework keeps the access grant tied to a stable
/// system binary: macOS ties it to the asking binary's signature, so an ad-hoc
/// signed build would otherwise re-prompt after every rebuild.
#[cfg(target_os = "macos")]
fn keychain_secret(service: &str) -> Option<String> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Claude Code's OAuth access tokens, in order of explicitness and freshness.
fn claude_oauth_tokens() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |t: String| {
        let t = t.trim().to_string();
        if !t.is_empty() && !out.iter().any(|o| *o == t) {
            out.push(t);
        }
    };
    if let Ok(v) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        add(v);
    }
    // Claude Code's own copy, where it keeps the credential it refreshes.
    #[cfg(target_os = "macos")]
    for svce in keychain_credential_services() {
        if let Some(t) = keychain_secret(&svce).and_then(|raw| claude_access_token(&raw)) {
            add(t);
        }
    }
    // `~/.claude/.credentials.json`: { claudeAiOauth: { accessToken, expiresAt } }
    if let Some(home) = dirs::home_dir() {
        if let Ok(text) = fs::read_to_string(home.join(".claude").join(".credentials.json")) {
            if let Some(t) = claude_access_token(&text) {
                add(t);
            }
        }
    }
    // Oh My Pi's store, when it holds the Anthropic OAuth credential.
    for db in omp_agent_dbs() {
        let Ok(conn) = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT data FROM auth_credentials
             WHERE provider = 'anthropic' AND credential_type = 'oauth'
               AND (disabled_cause IS NULL OR disabled_cause = '')
             ORDER BY updated_at DESC",
        ) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
            continue;
        };
        for data in rows.flatten() {
            if let Some(t) = claude_access_token(&data) {
                add(t);
            }
        }
    }
    out
}

/// `utilization`/`percent` arrive as a 0..100 percentage. `severity` (on the
/// `limits[]` entries only) is the provider's own verdict; the top-level windows
/// carry no verdict, so one is derived from the fill level.
fn claude_status(severity: Option<&str>, used: f64) -> String {
    match severity.unwrap_or("") {
        "critical" | "exceeded" | "rate-limited" => "exhausted".to_string(),
        "warning" => "warning".to_string(),
        "normal" | "ok" => "ok".to_string(),
        _ if used >= 100.0 => "exhausted".to_string(),
        _ if used >= 80.0 => "warning".to_string(),
        _ => "ok".to_string(),
    }
}

/// The four canonical Claude windows plus any scoped one the plan adds (Claude
/// reports "weekly_scoped" with a model name, which is where the Fable window
/// comes from). Unknown codename windows are ignored.
fn parse_claude_usage(v: &serde_json::Value, observed_ms: i64) -> Option<Vec<UsageLimit>> {
    const WINDOWS: [(&str, &str, &str); 4] = [
        ("five_hour", "5 Hour", "Claude 5 Hour"),
        ("seven_day", "7 Day", "Claude 7 Day"),
        ("seven_day_opus", "7 Day", "Claude 7 Day (Opus)"),
        ("seven_day_sonnet", "7 Day", "Claude 7 Day (Sonnet)"),
    ];
    let mut out = Vec::new();
    let mut push = |used: f64, status: String, resets_ms: i64, window: &str, label: String| {
        out.push(UsageLimit {
            provider: "anthropic".to_string(),
            window: window.to_string(),
            label,
            used,
            status,
            resets_at_ms: resets_ms,
            observed_ms,
            source: "api".to_string(),
        });
    };
    let resets_of = |w: &serde_json::Value| -> i64 {
        w.get("resets_at")
            .and_then(|r| r.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis())
            .unwrap_or(0)
    };

    for (key, window, label) in WINDOWS {
        let Some(w) = v.get(key).filter(|w| !w.is_null()) else {
            continue;
        };
        let Some(used) = w.get("utilization").and_then(|u| u.as_f64()) else {
            continue;
        };
        let status = claude_status(w.get("severity").and_then(|s| s.as_str()), used);
        push(used, status, resets_of(w), window, label.to_string());
    }

    if let Some(limits) = v.get("limits").and_then(|l| l.as_array()) {
        for l in limits {
            let name = l
                .pointer("/scope/model/display_name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if name.is_empty() {
                continue; // session / weekly_all duplicate the windows above
            }
            let Some(used) = l.get("percent").and_then(|p| p.as_f64()) else {
                continue;
            };
            let window = match l.get("group").and_then(|g| g.as_str()).unwrap_or("") {
                "session" => "5 Hour",
                "weekly" => "7 Day",
                "monthly" => "Monthly",
                other => other,
            };
            let status = claude_status(l.get("severity").and_then(|s| s.as_str()), used);
            push(
                used,
                status,
                resets_of(l),
                window,
                format!("Claude {window} ({name})"),
            );
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn fetch_claude(token: &str) -> Option<Vec<UsageLimit>> {
    let resp = ureq::get(CLAUDE_USAGE_URL)
        .timeout(Duration::from_secs(10))
        .set("accept", "application/json")
        .set("anthropic-beta", CLAUDE_OAUTH_BETA)
        .set("authorization", &format!("Bearer {token}"))
        .call()
        .ok()?;
    let v: serde_json::Value = resp.into_json().ok()?;
    parse_claude_usage(&v, now_ms())
}

fn sql_f64(v: Option<SqlValue>) -> Option<f64> {
    match v? {
        SqlValue::Real(f) => Some(f),
        SqlValue::Integer(i) => Some(i as f64),
        SqlValue::Text(t) => t.trim().parse().ok(),
        _ => None,
    }
}

fn sql_i64(v: Option<SqlValue>) -> Option<i64> {
    match v? {
        SqlValue::Integer(i) => Some(i),
        SqlValue::Real(f) => Some(f as i64),
        SqlValue::Text(t) => t.trim().parse().ok(),
        _ => None,
    }
}

fn sql_str(v: Option<SqlValue>) -> Option<String> {
    match v? {
        SqlValue::Text(t) => Some(t),
        SqlValue::Integer(i) => Some(i.to_string()),
        SqlValue::Real(f) => Some(f.to_string()),
        _ => None,
    }
}

/// Latest reported window per (provider, limit) across every Oh My Pi agent db.
fn from_omp() -> Vec<UsageLimit> {
    from_omp_dbs(&omp_agent_dbs())
}

fn from_omp_dbs(dbs: &[PathBuf]) -> Vec<UsageLimit> {
    let mut best: HashMap<(String, String), UsageLimit> = HashMap::new();
    for db in dbs {
        let Ok(conn) = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT provider, limit_id, label, window_label, used_fraction, status,
                    resets_at, recorded_at
             FROM usage_history",
        ) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, SqlValue>(0).ok(),
                r.get::<_, SqlValue>(1).ok(),
                r.get::<_, SqlValue>(2).ok(),
                r.get::<_, SqlValue>(3).ok(),
                r.get::<_, SqlValue>(4).ok(),
                r.get::<_, SqlValue>(5).ok(),
                r.get::<_, SqlValue>(6).ok(),
                r.get::<_, SqlValue>(7).ok(),
            ))
        }) else {
            continue;
        };
        for row in rows.flatten() {
            let (provider, limit_id, label, window, used, status, resets, recorded) = row;
            let (Some(provider), Some(limit_id)) = (sql_str(provider), sql_str(limit_id)) else {
                continue;
            };
            let Some(used) = sql_f64(used) else { continue };
            let observed = sql_i64(recorded).unwrap_or(0);
            let item = UsageLimit {
                provider: provider.clone(),
                window: sql_str(window).unwrap_or_else(|| limit_id.clone()),
                // Oh My Pi labels these "Claude 5 Hour" / "5 Hour limit"; the
                // window is what the row is scanned by, so fold the two.
                label: sql_str(label).unwrap_or_else(|| limit_id.clone()),
                used: (used * 100.0 * 10.0).round() / 10.0,
                status: sql_str(status).unwrap_or_else(|| "ok".to_string()),
                resets_at_ms: sql_i64(resets).unwrap_or(0),
                observed_ms: observed,
                source: "oh-my-pi".to_string(),
            };
            let key = (provider, limit_id);
            match best.get(&key) {
                Some(prev) if prev.observed_ms >= item.observed_ms => {}
                _ => {
                    best.insert(key, item);
                }
            }
        }
    }
    best.into_values().collect()
}

/// Provider order first (the plans the user actually meters), then window length.
fn provider_rank(p: &str) -> usize {
    match p {
        "opencode-go" => 0,
        "anthropic" => 1,
        _ => 2,
    }
}

fn window_rank(w: &str) -> usize {
    ["5 Hour", "Weekly", "7 Day", "Monthly"]
        .iter()
        .position(|x| *x == w)
        .unwrap_or(usize::MAX)
}

fn sorted(mut items: Vec<UsageLimit>) -> Vec<UsageLimit> {
    items.sort_by(|a, b| {
        provider_rank(&a.provider)
            .cmp(&provider_rank(&b.provider))
            .then(a.provider.cmp(&b.provider))
            .then(window_rank(&a.window).cmp(&window_rank(&b.window)))
            .then(a.label.cmp(&b.label))
    });
    items
}

/// Identity of a window. The *label* is part of it on purpose: a provider can
/// report several windows under one name (Claude's "7 Day" and "7 Day (Fable)"),
/// and keying on (provider, window) alone silently dropped one of them.
type WindowKey = (String, String, String);

fn window_key(l: &UsageLimit) -> WindowKey {
    (l.provider.clone(), l.window.clone(), l.label.clone())
}

/// Merge two snapshots: the newest observation wins per window; on an equal
/// timestamp the later argument wins, so callers pass the most authoritative
/// source (a live fetch) last.
fn merge(base: Vec<UsageLimit>, extra: Vec<UsageLimit>) -> Vec<UsageLimit> {
    let mut by_key: HashMap<WindowKey, UsageLimit> = HashMap::new();
    for it in base.into_iter().chain(extra) {
        let k = window_key(&it);
        match by_key.get(&k) {
            Some(prev) if prev.observed_ms > it.observed_ms => {}
            _ => {
                by_key.insert(k, it);
            }
        }
    }
    sorted(by_key.into_values().collect())
}

fn read_cache() -> Option<Cached> {
    let text = fs::read_to_string(cache_path()?).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(items: &[UsageLimit]) {
    let Some(path) = cache_path() else { return };
    let c = Cached {
        fetched_at: now_ms(),
        items: items.to_vec(),
    };
    if let Ok(text) = serde_json::to_string(&c) {
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, &text).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

/// Refresh the shared snapshot. Returns None when a still-fresh cache means
/// there is nothing to do, so a background poll that learned nothing doesn't
/// churn the UI. MUST run off the main thread: the live fetch blocks up to ~10s.
pub fn reload_shared(force: bool) -> Option<Arc<Vec<UsageLimit>>> {
    let cached = read_cache();
    if !force {
        if let Some(c) = cached.as_ref() {
            let age = now_ms() - c.fetched_at;
            if (0..FRESH.as_millis() as i64).contains(&age) {
                return None;
            }
        }
    }
    // Precedence, weakest first: the previous snapshot on disk, then what Oh My
    // Pi's poller last recorded, then a live fetch. (A tie is broken by order,
    // and a fresh local read must outrank the copy we cached minutes ago.)
    let restored = cached.map(|c| c.items).unwrap_or_default();
    let mut live = Vec::new();
    if let Some(key) = opencode_go_key() {
        live.extend(fetch_opencode_go(&key).unwrap_or_default());
    }
    // First token the endpoint accepts wins: the copies expire independently,
    // so a dead one must not shadow a live one.
    for token in claude_oauth_tokens() {
        if let Some(items) = fetch_claude(&token) {
            live.extend(items);
            break;
        }
    }
    let items = Arc::new(merge(merge(restored, from_omp()), live));
    if let Ok(mut g) = LIMITS.write() {
        *g = items.clone();
    }
    write_cache(&items);
    Some(items)
}

/// The current snapshot (cheap Arc clone).
pub fn shared() -> Arc<Vec<UsageLimit>> {
    match LIMITS.read() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    }
}

/// Only used by the dev snapshot example, which has no background threads.
pub fn warm() {
    let _ = reload_shared(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_access_token_is_read_from_every_credential_shape() {
        // Claude Code's file and its keychain item.
        assert_eq!(
            claude_access_token(r#"{"claudeAiOauth":{"accessToken":" sk-ant-oat01-x "}}"#).as_deref(),
            Some("sk-ant-oat01-x")
        );
        // Oh My Pi's auth_credentials row.
        assert_eq!(
            claude_access_token(r#"{"access":"sk-ant-oat01-y","refresh":"r"}"#).as_deref(),
            Some("sk-ant-oat01-y")
        );
        // Empty, absent and non-JSON documents yield no token.
        assert_eq!(claude_access_token(r#"{"claudeAiOauth":{"accessToken":"  "}}"#), None);
        assert_eq!(claude_access_token(r#"{"claudeAiOauth":{"scopes":[]}}"#), None);
        assert_eq!(claude_access_token("not json"), None);
    }

    #[test]
    fn parses_the_opencode_go_usage_response() {
        // captured from GET https://opencode.ai/zen/go/v1/usage
        let json = r#"{"usage":{
            "rolling":{"status":"ok","percent":16,"resetsAt":"2026-09-23T12:42:40.263Z"},
            "weekly":{"status":"ok","percent":32,"resetsAt":"2026-09-28T00:00:00.000Z"},
            "monthly":{"status":"ok","percent":16,"resetsAt":"2026-10-22T15:19:19.000Z"}}}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let items = parse_opencode_usage(&v, 42).expect("three windows");
        assert_eq!(items.len(), 3);
        assert_eq!(
            items.iter().map(|i| i.window.as_str()).collect::<Vec<_>>(),
            vec!["5 Hour", "Weekly", "Monthly"]
        );
        assert_eq!(items[0].used, 16.0);
        assert_eq!(items[1].used, 32.0);
        assert!(items.iter().all(|i| i.provider == "opencode-go" && i.source == "api"));
        assert_eq!(items[0].resets_at_ms, 1790167360263); // 2026-09-23T12:42:40.263Z
        assert!(items.iter().all(|i| i.observed_ms == 42));
        // an envelope without the windows is not a snapshot
        assert!(parse_opencode_usage(&serde_json::json!({"error":"nope"}), 1).is_none());
        // a window missing `percent` is dropped (percent is the whole point)
        let partial =
            serde_json::json!({"usage":{"rolling":{"status":"ok","percent":5,"resetsAt":"2026-09-23T12:42:40Z"},
                                         "weekly":{"status":"ok"}}});
        let items = parse_opencode_usage(&partial, 1).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].window, "5 Hour");
    }

    #[test]
    fn parses_the_claude_usage_response() {
        // shape captured from GET https://api.anthropic.com/api/oauth/usage
        let json = r#"{
          "five_hour": {"utilization": 100.0, "resets_at": "2026-09-23T12:20:00.487861+00:00"},
          "seven_day": {"utilization": 27.0, "resets_at": "2026-09-25T10:00:00.487881+00:00"},
          "seven_day_opus": null, "seven_day_sonnet": null,
          "nimbus_quill": {"utilization": 0.0, "resets_at": null},
          "limits": [
            {"kind":"session","group":"session","percent":100,"severity":"critical","resets_at":"2026-09-23T12:20:00.487861+00:00","scope":null},
            {"kind":"weekly_all","group":"weekly","percent":27,"severity":"normal","resets_at":"2026-09-25T10:00:00.487881+00:00","scope":null},
            {"kind":"weekly_scoped","group":"weekly","percent":0,"severity":"normal","resets_at":"2026-09-25T10:00:00+00:00",
             "scope":{"model":{"id":null,"display_name":"Fable"}}}
          ]}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let items = parse_claude_usage(&v, 7).expect("windows");
        // the two canonical windows plus the scoped Fable one — the session /
        // weekly_all entries in limits[] must not show up twice, and the
        // unknown codename window must be ignored
        assert_eq!(items.len(), 3);
        let five = items.iter().find(|i| i.window == "5 Hour").unwrap();
        assert_eq!(five.used, 100.0);
        assert_eq!(five.status, "exhausted"); // derived from the fill level
        assert_eq!(five.source, "api");
        assert_eq!(five.observed_ms, 7);
        assert!(five.resets_at_ms > 0);
        let week = items.iter().find(|i| i.label == "Claude 7 Day").unwrap();
        assert_eq!(week.used, 27.0);
        assert_eq!(week.status, "ok");
        let fable = items.iter().find(|i| i.label == "Claude 7 Day (Fable)").unwrap();
        assert_eq!(fable.window, "7 Day");
        assert_eq!(fable.used, 0.0);
        // no recognizable window at all → not a snapshot
        assert!(parse_claude_usage(&serde_json::json!({"limits":[]}), 1).is_none());
    }

    #[test]
    fn live_windows_win_and_are_never_dropped_by_a_merge() {
        let stored = vec![
            UsageLimit {
                provider: "opencode-go".into(),
                window: "5 Hour".into(),
                label: "5 Hour limit".into(),
                used: 15.0,
                status: "ok".into(),
                resets_at_ms: 1,
                observed_ms: 100,
                source: "oh-my-pi".into(),
            },
            UsageLimit {
                provider: "anthropic".into(),
                window: "5 Hour".into(),
                label: "Claude 5 Hour".into(),
                used: 84.0,
                status: "warning".into(),
                resets_at_ms: 2,
                observed_ms: 100,
                source: "oh-my-pi".into(),
            },
        ];
        let live = vec![UsageLimit {
            provider: "opencode-go".into(),
            window: "5 Hour".into(),
            label: "5 Hour limit".into(),
            used: 21.0,
            status: "ok".into(),
            resets_at_ms: 3,
            observed_ms: 200,
            source: "api".into(),
        }];
        let merged = merge(stored, live);
        // opencode-go comes from the live fetch; the provider we can't call is kept
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].provider, "opencode-go");
        assert_eq!(merged[0].used, 21.0);
        assert_eq!(merged[0].source, "api");
        assert_eq!(merged[1].provider, "anthropic");
        assert_eq!(merged[1].used, 84.0);
        // a failed fetch keeps whatever we already knew
        let kept = merge(merged.clone(), Vec::new());
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].used, 21.0);
    }

    #[test]
    fn two_windows_sharing_a_label_are_both_kept() {
        // Claude reports "7 Day" and "7 Day (Fable)" — same window name, so
        // keying on (provider, window) alone would drop one of them.
        let mk = |label: &str, used: f64| UsageLimit {
            provider: "anthropic".into(),
            window: "7 Day".into(),
            label: label.into(),
            used,
            status: "ok".into(),
            resets_at_ms: 0,
            observed_ms: 10,
            source: "oh-my-pi".into(),
        };
        let out = merge(
            vec![mk("Claude 7 Day", 27.0), mk("Claude 7 Day (Fable)", 0.0)],
            Vec::new(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].used, 27.0);
        assert_eq!(out[1].used, 0.0);
    }

    #[test]
    fn omp_usage_history_rows_become_windows_with_the_newest_snapshot_per_limit() {
        let dir = std::env::temp_dir().join(format!("tokenscope-limits-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("agent.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_history (
               id INTEGER PRIMARY KEY AUTOINCREMENT, recorded_at INTEGER, provider TEXT,
               account_key TEXT, email TEXT, account_id TEXT, limit_id TEXT, label TEXT,
               window_label TEXT, used_fraction REAL, status TEXT, resets_at INTEGER)",
        )
        .unwrap();
        let put = |rec: i64, limit: &str, window: &str, used: f64, status: &str| {
            conn.execute(
                "INSERT INTO usage_history (recorded_at, provider, limit_id, label, window_label,
                                            used_fraction, status, resets_at)
                 VALUES (?1, 'anthropic', ?2, ?3, ?4, ?5, ?6, 99)",
                rusqlite::params![rec, limit, format!("Claude {limit}"), window, used, status],
            )
            .unwrap();
        };
        put(100, "anthropic:5h", "5 Hour", 0.10, "ok");
        put(300, "anthropic:5h", "5 Hour", 0.84, "warning"); // newer snapshot must win
        put(200, "anthropic:7d", "7 Day", 0.49, "ok");

        let items = from_omp_dbs(&[db]);
        assert_eq!(items.len(), 2);
        let five = items.iter().find(|i| i.window == "5 Hour").unwrap();
        assert_eq!(five.used, 84.0); // fraction → percent, newest row
        assert_eq!(five.status, "warning");
        assert_eq!(five.observed_ms, 300);
        assert_eq!(five.source, "oh-my-pi");
        assert_eq!(five.resets_at_ms, 99);
        let seven = items.iter().find(|i| i.window == "7 Day").unwrap();
        assert_eq!(seven.used, 49.0);

        drop(conn);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_order_is_stable_and_shorter_windows_come_first() {
        let mk = |p: &str, w: &str| UsageLimit {
            provider: p.into(),
            window: w.into(),
            label: w.into(),
            used: 0.0,
            status: "ok".into(),
            resets_at_ms: 0,
            observed_ms: 0,
            source: "oh-my-pi".into(),
        };
        let out = sorted(vec![
            mk("anthropic", "Monthly"),
            mk("opencode-go", "Weekly"),
            mk("anthropic", "5 Hour"),
            mk("opencode-go", "5 Hour"),
        ]);
        let seen: Vec<(String, String)> = out
            .iter()
            .map(|i| (i.provider.clone(), i.window.clone()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("opencode-go".to_string(), "5 Hour".to_string()),
                ("opencode-go".to_string(), "Weekly".to_string()),
                ("anthropic".to_string(), "5 Hour".to_string()),
                ("anthropic".to_string(), "Monthly".to_string()),
            ]
        );
    }
}
