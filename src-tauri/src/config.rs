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

/// Parse ~/.claude.json once (None if missing/unreadable/invalid).
fn read_user_config() -> Option<serde_json::Value> {
    let path = home()?.join(".claude.json");
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
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

/// User-installed skills = every agent's skills folders: Claude Code's global
/// `~/.claude/skills`, each agent dir's `skills/`, and the skills Oh My Pi
/// installs from plugins. Project-level skill dirs are not scanned: they are
/// per-project content, not an install, and folding them in inflates the
/// "installed skills" metric.
fn load_user_skills() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(h) = home() {
        scan_skill_dir(&h.join(".claude").join("skills"), &mut set);
        scan_plugin_skills(
            &h.join(".omp").join("plugins").join("node_modules"),
            &mut set,
        );
    }
    for dir in agent_dirs() {
        scan_skill_dir(&dir.join("skills"), &mut set);
    }
    set
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
        let mut mcp_servers = mcps_from(read_user_config().as_ref());
        for dir in agent_dirs() {
            mcp_servers.extend(mcps_from_agent_file(&dir.join("mcp.json")));
        }
        if let Some(h) = home() {
            mcp_servers.extend(mcps_from_codex_toml(&h.join(".codex").join("config.toml")));
        }
        UserConfig {
            mcp_servers,
            skills: load_user_skills(),
        }
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
}
