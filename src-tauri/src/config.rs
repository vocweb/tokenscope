// Loads the "installed" whitelists so the dashboard only counts MCP servers /
// Skills the user actually added (PRD decision). Every agent the store ingests
// declares its own MCP servers, and every agent keeps its skills as folders, so
// both lists are the union across agents: an Oh My Pi session that calls a
// server installed through Claude Code (or the reverse) is still the user's own
// call, and a skill invoked under a plugin namespace is matched to the folder
// the agent installed it from.
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};
#[derive(Clone, Default)]
pub struct UserConfig {
    pub mcp_servers: HashSet<String>,
    pub skills: HashSet<String>,
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

// ── MCP servers ────────────────────────────────────────────────────

/// `mcpServers` keys of a `{ "mcpServers": { … } }` document. Claude Code's
/// `~/.claude.json` and Oh My Pi's / pi's `<agent>/mcp.json` share this shape.
fn mcps_from_document(json: &serde_json::Value) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(obj) = json.get("mcpServers").and_then(|v| v.as_object()) {
        for k in obj.keys() {
            set.insert(k.clone());
        }
    }
    set
}

/// mcpServers (top level) + projects[*].mcpServers from a parsed ~/.claude.json.
fn mcps_from(json: Option<&serde_json::Value>) -> HashSet<String> {
    let mut set = HashSet::new();
    let Some(json) = json else { return set };
    set.extend(mcps_from_document(json));
    if let Some(projects) = json.get("projects").and_then(|v| v.as_object()) {
        for proj in projects.values() {
            set.extend(mcps_from_document(proj));
        }
    }
    set
}

/// `mcpServers` keys of an agent's `mcp.json` (missing/invalid → empty).
fn mcps_from_agent_file(path: &Path) -> HashSet<String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|j| mcps_from_document(&j))
        .unwrap_or_default()
}

/// `[mcp_servers.<name>]` table headers in Codex's TOML config. Read line-wise
/// so one key needs no TOML parser; a header is the only place a server is
/// declared, and indentation/inline tables can't be confused with it.
fn mcps_from_codex_toml(path: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let Ok(text) = fs::read_to_string(path) else {
        return set;
    };
    for line in text.lines() {
        let line = line.trim();
        let Some(inner) = line.strip_prefix("[mcp_servers.").and_then(|l| l.strip_suffix(']'))
        else {
            continue;
        };
        // a nested table ("[mcp_servers.x.env]") declares no server of its own
        if inner.is_empty() || inner.contains('.') {
            continue;
        }
        set.insert(inner.trim_matches('"').to_string());
    }
    set
}

// ── Skills ─────────────────────────────────────────────────────────

/// Add each subdirectory name of `dir` to the set (skills are folders).
fn scan_skill_dir(dir: &Path, set: &mut HashSet<String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    set.insert(name.to_string());
                }
            }
        }
    }
}

/// Skills shipped by plugins, one level of package nesting deep
/// (`node_modules/<pkg>/skills`, `node_modules/@scope/<pkg>/skills`). Bounded
/// rather than a recursive walk: this runs on every dashboard refresh.
fn scan_plugin_skills(node_modules: &Path, set: &mut HashSet<String>) {
    let Ok(pkgs) = fs::read_dir(node_modules) else {
        return;
    };
    for pkg in pkgs.flatten() {
        let dir = pkg.path();
        scan_skill_dir(&dir.join("skills"), set);
        // a scope directory holds packages, not skills
        if pkg.file_name().to_string_lossy().starts_with('@') {
            if let Ok(scoped) = fs::read_dir(&dir) {
                for inner in scoped.flatten() {
                    scan_skill_dir(&inner.path().join("skills"), set);
                }
            }
        }
    }
}

/// Agent data dirs holding their `mcp.json` and `skills/`: Oh My Pi (default
/// `~/.omp/agent` plus one per profile) and pi (`$PI_CODING_AGENT_DIR`, which
/// Oh My Pi also points at its active profile, else `~/.pi/agent`).
fn agent_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home() {
        let omp = home.join(".omp");
        dirs.push(omp.join("agent"));
        if let Ok(profiles) = fs::read_dir(omp.join("profiles")) {
            for p in profiles.flatten() {
                dirs.push(p.path().join("agent"));
            }
        }
    }
    if let Some(agent) = std::env::var_os("PI_CODING_AGENT_DIR") {
        dirs.push(PathBuf::from(agent));
    } else if let Some(home) = home() {
        dirs.push(home.join(".pi").join("agent"));
    }
    dirs
}

/// Scan every config source under one home plus the given agent dirs.
/// Takes the roots as parameters so tests can point at temp dirs; the
/// production caller passes the real home and `agent_dirs()`.
fn scan_sources(home: &Path, agents: &[PathBuf]) -> UserConfig {
    let text = fs::read_to_string(home.join(".claude.json")).ok();
    let parsed: Option<serde_json::Value> = text.and_then(|t| serde_json::from_str(&t).ok());
    let mut mcp_servers = mcps_from(parsed.as_ref());
    for dir in agents {
        mcp_servers.extend(mcps_from_agent_file(&dir.join("mcp.json")));
    }
    mcp_servers.extend(mcps_from_codex_toml(&home.join(".codex").join("config.toml")));
    let mut skills = HashSet::new();
    scan_skill_dir(&home.join(".claude").join("skills"), &mut skills);
    scan_plugin_skills(
        &home.join(".omp").join("plugins").join("node_modules"),
        &mut skills,
    );
    for dir in agents {
        scan_skill_dir(&dir.join("skills"), &mut skills);
    }
    UserConfig {
        mcp_servers,
        skills,
    }
}
/// 前回結果とmtime群を保持し再読込を抑えるキャッシュ。
/// Ghi nhớ kết quả quét và mtime để bỏ qua lần quét lại khi không đổi.
#[derive(Default)]
struct CachedConfig {
    config: UserConfig,
    snap: Vec<Option<SystemTime>>,
    at: Option<Instant>,
    #[cfg(test)]
    scans: u32,
}

impl CachedConfig {
    /// mtime不変でも再走査するまでの保持時間で新規導入を拾う。
    /// TTL giữ kết quả quét dù mtime không đổi, đủ nhanh cho cài đặt mới.
    const TTL: Duration = Duration::from_secs(5 * 60);

    fn source_paths(home: &Path, agents: &[PathBuf]) -> Vec<PathBuf> {
        let mut paths = vec![
            home.join(".claude.json"),
            home.join(".codex").join("config.toml"),
            home.join(".claude").join("skills"),
            home.join(".omp").join("plugins").join("node_modules"),
            home.join(".omp").join("profiles"),
        ];
        for dir in agents {
            paths.push(dir.join("mcp.json"));
            paths.push(dir.join("skills"));
        }
        paths
    }

    fn fingerprint(home: &Path, agents: &[PathBuf]) -> Vec<Option<SystemTime>> {
        Self::source_paths(home, agents)
            .iter()
            .map(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
            .collect()
    }

    fn stale(&self, home: &Path, agents: &[PathBuf], now: Instant) -> bool {
        let Some(at) = self.at else { return true };
        if Self::fingerprint(home, agents) != self.snap {
            return true;
        }
        now.duration_since(at) >= Self::TTL
    }

    fn replace(&mut self, scanned: UserConfig, home: &Path, agents: &[PathBuf], now: Instant) {
        self.config = scanned;
        self.snap = Self::fingerprint(home, agents);
        self.at = Some(now);
    }

    /// Shared refresh path for production and tests: rescan only when a
    /// source mtime moved or the TTL elapsed, else return the cached result.
    fn load_from(&mut self, home: &Path, agents: &[PathBuf], now: Instant) -> UserConfig {
        if !self.stale(home, agents, now) {
            return self.config.clone();
        }
        #[cfg(test)]
        {
            self.scans += 1;
        }
        let scanned = scan_sources(home, agents);
        self.replace(scanned, home, agents, now);
        self.config.clone()
    }
}

/// Agents spell the same server differently — `unity-mcp` in a config,
/// `UnityMCP` in Claude Code's own project entry, `chrome-devtools` in Oh My
/// Pi's mcp.json against `chrome_devtools` inside its tool names — so case and
/// the `-`/`_` separator are folded before matching.
fn normalize_server(name: &str) -> String {
    name.to_lowercase().replace('-', "_")
}

impl UserConfig {
    pub fn load() -> Self {
        static CACHE: LazyLock<Mutex<CachedConfig>> =
            LazyLock::new(|| Mutex::new(CachedConfig::default()));
        // スキャンが高コストなので保持したロック下で重複実行を抑える。
        // Giữ khoá suốt quá trình quét để hai luồng không quét trùng lặp.
        let mut guard = CACHE.lock().unwrap();
        let Some(home) = home() else {
            return guard.config.clone();
        };
        let agents = agent_dirs();
        let now = Instant::now();
        guard.load_from(&home, &agents, now)
    }

    /// Resolve a tool name's server candidate against the installed servers,
    /// returning the installed server's own name so the breakdown shows one row
    /// per configured server. Claude Code names a call `mcp__<server>__<tool>`
    /// and Codex follows the same convention, while Oh My Pi flattens the parts
    /// (`xd_mcp__unitymcp_manage_asset`), so the candidate may still carry the
    /// tool name after the server's separator; the longest match wins, which
    /// keeps a server called `chrome` from claiming `chrome-devtools` calls.
    pub fn resolve_mcp(&self, candidate: &str) -> Option<String> {
        let c = normalize_server(candidate);
        if c.is_empty() {
            return None;
        }
        self.mcp_servers
            .iter()
            .filter(|s| {
                let n = normalize_server(s);
                !n.is_empty() && (c == n || c.starts_with(&format!("{n}_")))
            })
            .max_by_key(|s| normalize_server(s).len())
            .map(|s| s.to_string())
    }

    /// A skill id → is it one the user installed? Ids are spelled per agent:
    /// Claude Code logs `input.skill` (`ak-plan`, `ck-plan`), Oh My Pi logs the
    /// plugin-qualified id it was invoked under (`skill://ak:debug`), and a
    /// plugin-qualified id matches the folder it came from (`ak:debug` →
    /// `ak-debug`), which is how agents name installed plugin skills.
    pub fn is_user_skill(&self, skill: &str) -> bool {
        let bare = skill.rsplit(':').next().unwrap_or(skill);
        self.skills.contains(skill)
            || self.skills.contains(&skill.replace(':', "-"))
            || self.skills.contains(bare)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mcp: &[&str], skills: &[&str]) -> UserConfig {
        UserConfig {
            mcp_servers: mcp.iter().map(|s| s.to_string()).collect(),
            skills: skills.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn mcp_candidates_resolve_to_the_configured_server() {
        let c = cfg(&["unity-mcp", "pencil", "UnityMCP"], &[]);
        // Claude Code names the server exactly
        assert_eq!(c.resolve_mcp("UnityMCP").as_deref(), Some("UnityMCP"));
        assert_eq!(c.resolve_mcp("unity-mcp").as_deref(), Some("unity-mcp"));
        // Oh My Pi flat name carries the tool after the server
        assert_eq!(
            c.resolve_mcp("unitymcp_manage_asset").as_deref(),
            Some("UnityMCP")
        );
        assert_eq!(c.resolve_mcp("pencil_batch_design").as_deref(), Some("pencil"));
        // a server the user never installed is dropped (Anthropic's bundled MCP)
        assert_eq!(c.resolve_mcp("claude_ai_Figma"), None);
    }

    #[test]
    fn the_longest_server_prefix_wins_so_nested_names_are_not_stolen() {
        let c = cfg(&["chrome", "chrome-devtools"], &[]);
        assert_eq!(
            c.resolve_mcp("chrome_devtools_navigate_page").as_deref(),
            Some("chrome-devtools")
        );
        assert_eq!(c.resolve_mcp("chrome_open_page").as_deref(), Some("chrome"));
        // a prefix must be delimited by the separator, not a substring match
        let c = cfg(&["chrome"], &[]);
        assert_eq!(c.resolve_mcp("chromedevtools_open"), None);
    }

    #[test]
    fn installed_skills_match_their_plugin_qualified_ids() {
        let c = cfg(&[], &["ak-debug", "advanced-skill"]);
        assert!(c.is_user_skill("ak:debug"), "plugin id → its installed folder");
        assert!(c.is_user_skill("ak-debug"));
        assert!(c.is_user_skill("advanced-skill"));
        assert!(
            !c.is_user_skill("advanced"),
            "the bare-name fallback must not turn a prefix into a match"
        );
        assert!(!c.is_user_skill("ck:advise"), "not installed under any agent");
    }

    #[test]
    fn codex_mcp_servers_are_read_from_table_headers() {
        let dir = std::env::temp_dir().join(format!("ts-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[mcp_servers.unityMCP]\nurl = \"http://127.0.0.1:8080/mcp\"\n\n\
             [mcp_servers.pencil]\ncommand = \"x\"\n\n\
             [mcp_servers.pencil.env]\nK = \"v\"\n",
        )
        .unwrap();
        let found = mcps_from_codex_toml(&path);
        assert_eq!(found.len(), 2, "a nested `.env` table is not a server: {found:?}");
        assert!(found.contains("unityMCP") && found.contains("pencil"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unchanged_sources_reuse_the_cached_result() {
        use std::time::{Duration, Instant};
        let home = temp_home("cache-hit");
        write_text(&home.join(".claude.json"), r#"{"mcpServers":{"alpha":{}}}"#);
        let agents = vec![home.join("agents").join("a")];
        write_text(&agents[0].join("mcp.json"), r#"{"mcpServers":{"beta":{}}}"#);
        fs::create_dir_all(home.join(".claude").join("skills").join("my-skill")).unwrap();
        let t0 = Instant::now();
        let mut cache = CachedConfig::default();
        let first = cache.load_from(&home, &agents, t0);
        assert!(first.mcp_servers.contains("alpha"));
        assert!(first.mcp_servers.contains("beta"));
        assert!(first.skills.contains("my-skill"));
        assert_eq!(cache.scans, 1);
        // same sources, well within the TTL: no rescan
        let second = cache.load_from(&home, &agents, t0 + Duration::from_secs(59));
        assert_eq!(cache.scans, 1, "same mtimes within TTL must not rescan");
        assert_eq!(second.mcp_servers, first.mcp_servers);
        assert_eq!(second.skills, first.skills);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn moved_source_mtime_triggers_a_rescan() {
        use std::time::{Duration, Instant};
        let home = temp_home("cache-mtime");
        write_text(&home.join(".claude.json"), r#"{"mcpServers":{}}"#);
        let agents = vec![home.join("agents").join("a")];
        write_text(&agents[0].join("mcp.json"), r#"{"mcpServers":{"beta":{}}}"#);
        let t0 = Instant::now();
        let mut cache = CachedConfig::default();
        let first = cache.load_from(&home, &agents, t0);
        assert!(!first.mcp_servers.contains("gamma"));
        // installing another server rewrites the file, moving its mtime
        write_text(
            &agents[0].join("mcp.json"),
            r#"{"mcpServers":{"beta":{},"gamma":{}}}"#,
        );
        let second = cache.load_from(&home, &agents, t0 + Duration::from_secs(1));
        assert_eq!(cache.scans, 2, "moved mtime must rescan");
        assert!(second.mcp_servers.contains("gamma"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn expired_ttl_triggers_a_rescan() {
        use std::time::{Duration, Instant};
        let home = temp_home("cache-ttl");
        write_text(&home.join(".claude.json"), r#"{"mcpServers":{}}"#);
        let agents: Vec<PathBuf> = Vec::new();
        let t0 = Instant::now();
        let mut cache = CachedConfig::default();
        cache.load_from(&home, &agents, t0);
        assert_eq!(cache.scans, 1);
        // nothing changed on disk, but the TTL elapsed: rescan anyway
        cache.load_from(&home, &agents, t0 + Duration::from_secs(6 * 60));
        assert_eq!(cache.scans, 2, "expired TTL must rescan");
        let _ = fs::remove_dir_all(&home);
    }

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ts-cfg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_text(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}
