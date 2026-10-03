// Token pricing. Primary source: models.dev (bare model names, matches Claude
// CLI logs). Fallback: LiteLLM. Final backstop: a tiny built-in snapshot.
//
// Matching is layered: exact id → normalized id (strip provider path prefix +
// unify the ".'↔'p" version separator, e.g. "glm-5.1" ⇄ "glm-5p1").
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

// Process-wide memoized price table. Loaded once off the main thread (see
// reload_shared) and refreshed every 24h, so build_dashboard — which holds the
// store lock — only ever does a cheap Arc clone, never JSON parsing or network.
static PRICING: OnceLock<RwLock<Arc<Pricing>>> = OnceLock::new();

const MODELSDEV_URL: &str = "https://models.dev/api.json";
const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60); // 24h
                                                             // Bundled LiteLLM price table snapshot — offline fallback so a first launch
                                                             // with no network (and no prior cache) still prices the common third-party
                                                             // models, not just the few hardcoded in `ingest_builtin`. Live sources, when
                                                             // reachable, are ingested first and win.
const LITELLM_SNAPSHOT: &str = include_str!("../snapshots/litellm.json");

#[derive(Clone, Default)]
pub struct ModelPrice {
    pub input: f64,        // per-token USD
    pub output: f64,       // per-token USD
    pub cache_create: f64, // per-token USD, 5-minute cache write
    pub cache_read: f64,   // per-token USD
    /// Per-token USD for a 1-hour cache write. LiteLLM publishes this
    /// (`cache_creation_input_token_cost_above_1hr`); models.dev does not, so for
    /// Claude ids it is derived in `insert`. 0 means "not published", and `cost`
    /// falls back to the 5-minute rate rather than billing the tokens at zero.
    pub cache_create_1h: f64,
}

impl ModelPrice {
    fn is_zero(&self) -> bool {
        self.input == 0.0
            && self.output == 0.0
            && self.cache_create == 0.0
            && self.cache_read == 0.0
    }
}

pub struct Pricing {
    exact: HashMap<String, ModelPrice>,
    norm: HashMap<String, ModelPrice>,
}

/// Strip provider path prefix (after last '/') and unify version separators
/// so "z-ai/glm-5.1", "glm-5p1" and "glm-5.1" all collapse to one key.
fn normalize_key(s: &str) -> String {
    let base = s.rsplit('/').next().unwrap_or(s);
    base.to_lowercase().replace('.', "p")
}

fn bare(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Leading namespace words seen in front of a model id in agent logs. Bedrock /
/// Azure / Vertex deployments and resellers spell the same model with a dotted
/// provider path, and Oh My Pi logs those verbatim ("global.openai.gpt-5.6-sol",
/// "anthropic.claude-opus-5", "bedrock-mantle.openai.gpt-5.5").
const PROVIDER_SEGMENTS: &[&str] = &[
    "global",
    "us",
    "eu",
    "jp",
    "au",
    "ca",
    "sa",
    "ap",
    "apac",
    "us-gov",
    "gov",
    "anthropic",
    "openai",
    "google",
    "gemini",
    "meta",
    "mistral",
    "xai",
    "deepseek",
    "qwen",
    "moonshot",
    "minimax",
    "bedrock",
    "bedrock-mantle",
    "azure",
    "azure-anthropic",
    "aws-bedrock",
    "vertex",
    "vertex-anthropic",
    "amazon",
    "aws",
    "amazon-bedrock",
    "microsoft",
];

/// Reduce a logged model id to the name the price tables index and the UI groups
/// by: drop leading provider segments and a local quantization suffix
/// ("...@4bit"). Only *known* provider words are stripped, so a version dot
/// ("glm-5.1", "gpt-5.6-sol") is never touched.
pub fn canonical_id(id: &str) -> String {
    let mut s = id.split('@').next().unwrap_or(id);
    while let Some((head, tail)) = s.split_once('.') {
        if tail.is_empty() || !PROVIDER_SEGMENTS.contains(&head.to_ascii_lowercase().as_str()) {
            break;
        }
        s = tail;
    }
    s.to_string()
}

/// Whether `provider` is `id`'s first-party vendor, as opposed to a reseller,
/// gateway, or cloud that re-lists the same model (often with a markup, or with
/// cache-token pricing omitted). Lets the authoritative price win regardless of
/// the order models.dev happens to iterate its providers in. Unknown vendors
/// return false and fall back to the completeness/bare-id ordering.
///
/// Vendors with both an international and a China provider key list both;
/// subscription-plan keys (`*-coding-plan`, `*-token-plan`) are deliberately
/// excluded — plan rates aren't the pay-as-you-go API price. When both keys
/// carry the same bare id, the stable sort keeps models.dev's key order, so the
/// alphabetically-first (international, USD) entry wins the tie.
fn is_first_party(provider: &str, id: &str) -> bool {
    let l = id.to_lowercase();
    let vendors: &[&str] = if l.contains("claude") {
        &["anthropic"]
    } else if l.contains("gpt") || l.starts_with("o1") || l.starts_with("o3") {
        &["openai"]
    } else if l.contains("gemini") {
        &["google"]
    } else if l.contains("deepseek") {
        &["deepseek"]
    } else if l.contains("grok") {
        &["xai"]
    } else if l.contains("glm") {
        &["zai", "zhipuai"]
    } else if l.contains("qwen") {
        &["alibaba", "alibaba-cn"]
    } else if l.contains("kimi") {
        &["moonshotai", "moonshotai-cn"]
    } else if l.contains("minimax") {
        &["minimax", "minimax-cn"]
    } else {
        return false;
    };
    vendors.contains(&provider)
}

/// Whether an id names a Claude model. Anthropic's cache multipliers follow the
/// model, not the seller, so this holds for a reseller's re-listing too.
fn is_claude(id: &str) -> bool {
    id.to_lowercase().contains("claude")
}

fn cache_dir() -> Option<PathBuf> {
    let dir = dirs::cache_dir()?.join("tokenscope");
    let _ = fs::create_dir_all(&dir);
    Some(dir)
}

/// A models.dev payload: at least one provider with a non-empty `models` map.
fn valid_modelsdev(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.as_object().map(|root| {
                root.values().any(|p| {
                    p.get("models")
                        .and_then(|m| m.as_object())
                        .map(|m| !m.is_empty())
                        .unwrap_or(false)
                })
            })
        })
        .unwrap_or(false)
}

/// A LiteLLM payload: at least one entry carrying a per-token cost field.
fn valid_litellm(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.as_object().map(|root| {
                root.values().filter_map(|m| m.as_object()).any(|m| {
                    m.contains_key("input_cost_per_token")
                        || m.contains_key("output_cost_per_token")
                })
            })
        })
        .unwrap_or(false)
}

/// Read a fresh (<24h) cache for `name`, else fetch `url` & cache it, else fall
/// back to any stale cache. Returns `(text, changed)`: `changed` is true only
/// when a 200 actually fetched new content (a 304, a fresh local cache, or a
/// stale fallback all leave it false) — the caller uses it to skip re-parsing
/// ~5MB of JSON when nothing moved. `force` skips the freshness check and
/// re-fetches unconditionally (only the tray "Refresh" item passes true; the
/// 24h background poll passes false). `valid` gates what gets written to the
/// cache: a 200 carrying a JSON error envelope (CDN/proxy/rate limit) would
/// otherwise poison the cache for 24h with zero usable prices, so we only
/// persist a body that actually parses as a price table — and keep the previous
/// good cache otherwise. Fetches are conditional GETs: we send the last ETag
/// as If-None-Match, so an unchanged table gets a 304 with no body — skipping
/// the ~3MB download and the write.
fn fetch_cached(
    name: &str,
    url: &str,
    valid: impl Fn(&str) -> bool,
    force: bool,
) -> (Option<String>, bool) {
    let Some(dir) = cache_dir() else {
        return (None, false);
    };
    let path = dir.join(format!("{name}.json"));
    let etag_path = dir.join(format!("{name}.etag"));
    if !force {
        if let Ok(meta) = fs::metadata(&path) {
            let fresh = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .map(|age| age < MAX_AGE)
                .unwrap_or(false);
            if fresh {
                if let Ok(t) = fs::read_to_string(&path) {
                    return (Some(t), false);
                }
            }
        }
    }
    // Conditional GET: send the cached ETag so the CDN can answer 304 (no body)
    // when the table hasn't changed. Only send an ETag when we still have the
    // body it describes — an orphan .etag (e.g. user deleted the .json) would
    // otherwise draw a 304 for a body we no longer have.
    let cached_etag = fs::read_to_string(&etag_path)
        .ok()
        .filter(|_| fs::metadata(&path).is_ok());
    let mut req = ureq::get(url).timeout(Duration::from_secs(10));
    if let Some(e) = cached_etag.as_deref() {
        req = req.set("If-None-Match", e);
    }
    if let Ok(resp) = req.call() {
        if resp.status() == 304 {
            return (fs::read_to_string(&path).ok(), false);
        }
        let new_etag = resp.header("etag").map(str::to_string);
        if let Ok(text) = resp.into_string() {
            if valid(&text) {
                let _ = fs::write(&path, &text);
                if let Some(etag) = new_etag {
                    let _ = fs::write(&etag_path, etag);
                }
                return (Some(text), true);
            }
        }
    }
    // stale cache as last resort
    (fs::read_to_string(&path).ok(), false)
}

impl Pricing {
    pub fn load(force: bool) -> Option<Self> {
        let (md, md_changed) = fetch_cached("modelsdev", MODELSDEV_URL, valid_modelsdev, force);
        let (ll, ll_changed) = fetch_cached("litellm", LITELLM_URL, valid_litellm, force);
        // Nothing moved (both 304 / fresh cache / stale fallback) and we already
        // have a table loaded — skip the ~5MB re-parse and keep the current one.
        // First-ever load still parses (PRICING is unset) so a table exists
        // before the first build_dashboard runs.
        if PRICING.get().is_some() && !md_changed && !ll_changed {
            return None;
        }
        let mut p = Pricing {
            exact: HashMap::new(),
            norm: HashMap::new(),
        };
        // 1. models.dev — primary (inserted first, so it wins on conflict)
        if let Some(text) = md {
            p.ingest_modelsdev(&text);
        }
        // 2. LiteLLM — fills gaps models.dev doesn't cover
        if let Some(text) = ll {
            p.ingest_litellm(&text);
        }
        // 3. bundled LiteLLM snapshot — offline fallback for anything the live
        //    sources didn't supply (only fills gaps; live prices already won).
        p.ingest_litellm(LITELLM_SNAPSHOT);
        // 4. built-in backstop (a handful of core models, last resort)
        p.ingest_builtin();
        Some(p)
    }

    /// Just the built-in snapshot — no disk, no network. Returned by `shared()`
    /// before the background loader has run, so the common Claude models still
    /// price during the first moments after launch.
    fn builtin_only() -> Self {
        let mut p = Pricing {
            exact: HashMap::new(),
            norm: HashMap::new(),
        };
        p.ingest_builtin();
        p
    }

    /// The process-wide memoized price table (cheap Arc clone). Never blocks on
    /// disk/network — until `reload_shared` has populated the cell it returns the
    /// built-in snapshot, so callers holding the store lock are never stalled.
    pub fn shared() -> Arc<Pricing> {
        if let Some(lock) = PRICING.get() {
            if let Ok(g) = lock.read() {
                return g.clone();
            }
        }
        Arc::new(Pricing::builtin_only())
    }

    /// Load the full table (cache read + network on cold/stale cache) and swap it
    /// into the shared cell. MUST run on a background thread — never the main
    /// thread or a store-lock holder — since the fetch can block up to ~20s.
    pub fn reload_shared(force: bool) {
        let Some(p) = Pricing::load(force) else {
            return; // nothing changed — keep the current table, no re-parse, no swap
        };
        let p = Arc::new(p);
        match PRICING.get() {
            Some(lock) => {
                if let Ok(mut g) = lock.write() {
                    *g = p;
                }
            }
            None => {
                let _ = PRICING.set(RwLock::new(p));
            }
        }
    }

    fn insert(&mut self, id: &str, mut price: ModelPrice) {
        if price.is_zero() {
            return;
        }
        // Anthropic bills a 1-hour cache write at 2x the base input price and a
        // 5-minute one at 1.25x. Only LiteLLM publishes the 1h rate, so a
        // models.dev entry — inserted first, and therefore the winner — would
        // otherwise bill every cache write at the 5-minute rate. That is not a
        // rounding error: a Claude Code session holds its prompt cache for an
        // hour by default, so all of its cache-write tokens are 1h writes.
        if price.cache_create_1h == 0.0 && price.input > 0.0 && is_claude(id) {
            price.cache_create_1h = 2.0 * price.input;
        }
        self.exact
            .entry(id.to_string())
            .or_insert_with(|| price.clone());
        self.exact
            .entry(bare(id).to_string())
            .or_insert_with(|| price.clone());
        self.norm.entry(normalize_key(id)).or_insert(price);
    }

    // models.dev: { provider: { models: { id: { cost: {input,output,cache_read,cache_write} } } } }
    // cost is per-1M tokens → divide by 1e6 for per-token.
    fn ingest_modelsdev(&mut self, text: &str) {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        let Some(root) = json.as_object() else { return };
        // gather (provider, id, price)
        let mut entries: Vec<(&str, String, ModelPrice)> = Vec::new();
        for (prov_name, prov) in root {
            let Some(models) = prov.get("models").and_then(|m| m.as_object()) else {
                continue;
            };
            for (id, m) in models {
                let Some(c) = m.get("cost").and_then(|c| c.as_object()) else {
                    continue;
                };
                let g = |k: &str| c.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let price = ModelPrice {
                    input: g("input") / 1e6,
                    output: g("output") / 1e6,
                    cache_create: g("cache_write") / 1e6,
                    cache_read: g("cache_read") / 1e6,
                    // models.dev publishes a single cache-write rate (the 5-minute
                    // one); `insert` derives the 1h rate where it applies.
                    cache_create_1h: 0.0,
                };
                entries.push((prov_name.as_str(), id.clone(), price));
            }
        }
        // insert() is first-writer-wins, so order entries best-first. models.dev
        // lists the same model under many providers (the first-party vendor plus
        // resellers / gateways / clouds), and some reseller entries omit
        // cache-token pricing entirely. Prefer, in order: the model's first-party
        // vendor; then entries that actually carry cache pricing (so a reseller
        // that omits it can't zero it out — Claude usage is mostly cache reads, so
        // dropping cache pricing undercounts cost several-fold); then bare ids over
        // "vendor/model" duplicates.
        entries.sort_by_key(|(prov, id, price)| {
            let has_cache = price.cache_create > 0.0 || price.cache_read > 0.0;
            (!is_first_party(prov, id), !has_cache, id.contains('/'))
        });
        for (_, id, price) in entries {
            self.insert(&id, price);
        }
    }

    // LiteLLM: { key: { input_cost_per_token, output_cost_per_token, ... } } — already per-token.
    fn ingest_litellm(&mut self, text: &str) {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        let Some(root) = json.as_object() else { return };
        let mut entries: Vec<(String, ModelPrice)> = Vec::new();
        for (id, m) in root {
            let Some(o) = m.as_object() else { continue };
            let g = |k: &str| o.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let price = ModelPrice {
                input: g("input_cost_per_token"),
                output: g("output_cost_per_token"),
                cache_create: g("cache_creation_input_token_cost"),
                cache_read: g("cache_read_input_token_cost"),
                cache_create_1h: g("cache_creation_input_token_cost_above_1hr"),
            };
            entries.push((id.clone(), price));
        }
        entries.sort_by_key(|(id, _)| id.contains('/'));
        for (id, price) in entries {
            self.insert(&id, price);
        }
    }

    /// Last-resort table, in USD per token: what `shared()` serves for the
    /// moments before the background loader has fetched the real tables, and
    /// what prices a first run with neither network nor cache. Kept to the
    /// current flagship ids of the vendors this app's users actually run, at
    /// their published list rates:
    ///   - Anthropic: https://platform.claude.com/docs/en/about-claude/pricing
    ///     (input / output / 5m write / 1h write / cache read)
    ///   - OpenAI: https://developers.openai.com/api/docs/pricing
    fn ingest_builtin(&mut self) {
        // MTok (as published) → per token
        const M: f64 = 1e-6;
        let mk = |i: f64, o: f64, cc5: f64, cc1: f64, cr: f64| ModelPrice {
            input: i * M,
            output: o * M,
            cache_create: cc5 * M,
            cache_read: cr * M,
            cache_create_1h: cc1 * M,
        };
        let b: &[(&str, ModelPrice)] = &[
            ("claude-opus-5-5", mk(4.0, 20.0, 5.0, 8.0, 0.2)),
            ("claude-opus-5", mk(5.0, 25.0, 6.25, 10.0, 0.5)),
            ("claude-opus-4-8", mk(5.0, 25.0, 6.25, 10.0, 0.5)),
            ("claude-opus-4-7", mk(5.0, 25.0, 6.25, 10.0, 0.5)),
            ("claude-sonnet-5", mk(2.0, 10.0, 2.5, 4.0, 0.2)),
            ("claude-sonnet-4-6", mk(3.0, 15.0, 3.75, 6.0, 0.3)),
            ("claude-sonnet-4-5", mk(3.0, 15.0, 3.75, 6.0, 0.3)),
            ("claude-haiku-4-5", mk(1.0, 5.0, 1.25, 2.0, 0.1)),
            ("claude-fable-5-1", mk(10.0, 50.0, 12.5, 20.0, 0.25)),
            ("claude-fable-5", mk(10.0, 50.0, 12.5, 20.0, 1.0)),
            ("claude-mythos-5-1", mk(10.0, 50.0, 12.5, 20.0, 0.25)),
            ("claude-mythos-5", mk(10.0, 50.0, 12.5, 20.0, 1.0)),
            // OpenAI does not bill a separate cache write for gpt-5.5, hence the
            // 0 — its cached input is the only cache line on the price list.
            ("gpt-6-astra", mk(10.0, 50.0, 12.5, 0.0, 1.0)),
            ("gpt-5.6-sol", mk(4.0, 20.0, 5.0, 0.0, 0.4)),
            ("gpt-5.6-terra", mk(2.0, 12.0, 2.5, 0.0, 0.2)),
            ("gpt-5.5", mk(5.0, 30.0, 0.0, 0.0, 0.5)),
            // opencode-go routed models the tables don't list (rates as the
            // provider's own registry publishes them, $/MTok; pi's own
            // per-message cost is computed from the same numbers).
            ("deepseek-v4.1-flash", mk(0.15, 0.6, 0.0, 0.0, 0.003)),
            ("muse-spark-1.3-contributor", mk(0.1, 0.2, 0.0, 0.0, 0.002)),
        ];
        for (id, price) in b {
            self.insert(id, price.clone());
        }
    }

    fn lookup(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(p) = self.exact.get(model) {
            return Some(p);
        }
        if let Some(p) = self.norm.get(&normalize_key(model)) {
            return Some(p);
        }
        // Last resort: the same model under a provider-namespaced id
        // ("anthropic.claude-opus-5", "global.openai.gpt-5.6-sol"). This runs only
        // after an exact/normalized miss, so a deployment-specific entry that the
        // tables actually list keeps winning.
        let canonical = canonical_id(model);
        if canonical != model {
            if let Some(p) = self.exact.get(&canonical) {
                return Some(p);
            }
            return self.norm.get(&normalize_key(&canonical));
        }
        None
    }

    /// Exact-or-normalized cost in USD. None = no pricing data for this model.
    /// Cache-creation tokens arrive split by lifetime because Anthropic bills a
    /// 1-hour cache write at 2x base input and a 5-minute one at 1.25x.
    pub fn cost(
        &self,
        model: &str,
        input: f64,
        output: f64,
        cache_create_5m: f64,
        cache_create_1h: f64,
        cache_read: f64,
    ) -> Option<f64> {
        let p = self.lookup(model)?;
        // A table that publishes only the 5-minute rate bills 1h writes at it,
        // rather than silently dropping them to zero.
        let cc1 = if p.cache_create_1h > 0.0 {
            p.cache_create_1h
        } else {
            p.cache_create
        };
        Some(
            input * p.input
                + output * p.output
                + cache_create_5m * p.cache_create
                + cache_create_1h * cc1
                + cache_read * p.cache_read,
        )
    }

    #[allow(dead_code)]
    pub fn known(&self, model: &str) -> bool {
        self.lookup(model).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Pricing {
        Pricing {
            exact: HashMap::new(),
            norm: HashMap::new(),
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() <= b.abs() * 1e-9 + 1e-18
    }

    // A reseller that omits cache-token pricing and sorts before the first-party
    // vendor (models.dev iterates providers in key order) must not shadow the
    // official entry — otherwise cache tokens, which dominate Claude usage, are
    // priced at zero and cost is undercounted several-fold.
    #[test]
    fn namespaced_and_quantized_ids_fall_back_to_the_bare_model() {
        let json = r#"{
            "anthropic": { "models": { "claude-opus-5": { "cost": { "input": 5, "output": 25, "cache_write": 6.25, "cache_read": 0.5 } } } },
            "openai": { "models": { "gpt-5.6-sol": { "cost": { "input": 4, "output": 20, "cache_write": 5, "cache_read": 0.4 } } } }
        }"#;
        let mut p = empty();
        p.ingest_modelsdev(json);
        // how third-party / provider-namespaced logs spell the same model —
        // including ids whose *model name* contains a version dot, where taking
        // the last dot segment would grab "6-sol"/"5" instead of the model
        for id in [
            "anthropic.claude-opus-5",
            "global.anthropic.claude-opus-5",
            "claude-opus-5@4bit",
            "global.openai.gpt-5.6-sol",
            "openai.gpt-5.6-sol",
            "us.openai.gpt-5.6-sol",
            "bedrock-mantle.openai.gpt-5.6-sol",
        ] {
            let price = p
                .lookup(id)
                .unwrap_or_else(|| panic!("{id} should resolve"));
            assert!(price.input > 0.0, "{id} resolved to a zero price");
        }
        // namespaces only come off when the head is a provider word: a bare
        // version dot must survive untouched
        assert_eq!(canonical_id("glm-5.1"), "glm-5.1");
        assert_eq!(canonical_id("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(canonical_id("global.openai.gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(canonical_id("qwen3.8-27b-mlx@4bit"), "qwen3.8-27b-mlx");
        // an id that genuinely isn't in the table stays unpriced (no fuzzy match)
        assert!(p.lookup("claude-opus-99").is_none());
        assert!(p.lookup("qwen3.8-27b-mlx@4bit").is_none());
    }

    #[test]
    fn first_party_entry_wins_over_cacheless_reseller() {
        let json = r#"{
            "abacus":    { "models": { "claude-x": { "cost": { "input": 5, "output": 25 } } } },
            "anthropic": { "models": { "claude-x": { "cost": { "input": 5, "output": 25, "cache_write": 6.25, "cache_read": 0.5 } } } }
        }"#;
        let mut p = empty();
        p.ingest_modelsdev(json);
        let price = p.lookup("claude-x").expect("claude-x should be priced");
        assert!(approx(price.input, 5e-6));
        assert!(approx(price.output, 25e-6));
        assert!(approx(price.cache_create, 6.25e-6));
        assert!(approx(price.cache_read, 0.5e-6));
    }

    // Same shape as the abacus case, but for a Chinese vendor: the cacheless
    // reseller sorts first alphabetically yet must not shadow the official
    // zhipuai entry.
    #[test]
    fn cn_vendor_entry_wins_over_cacheless_reseller() {
        let json = r#"{
            "abacus":  { "models": { "glm-x": { "cost": { "input": 1, "output": 3.2 } } } },
            "zhipuai": { "models": { "glm-x": { "cost": { "input": 1, "output": 3.2, "cache_write": 1.25, "cache_read": 0.2 } } } }
        }"#;
        let mut p = empty();
        p.ingest_modelsdev(json);
        let price = p.lookup("glm-x").expect("glm-x should be priced");
        assert!(approx(price.cache_create, 1.25e-6));
        assert!(approx(price.cache_read, 0.2e-6));
    }

    // Both international and -cn keys are first-party; subscription-plan keys
    // and resellers are not.
    #[test]
    fn first_party_vendor_mapping() {
        assert!(is_first_party("zai", "glm-5"));
        assert!(is_first_party("zhipuai", "GLM-5.1"));
        assert!(!is_first_party("zai-coding-plan", "glm-5"));
        assert!(is_first_party("alibaba", "qwen3-max"));
        assert!(is_first_party("alibaba-cn", "qwen3-max"));
        assert!(!is_first_party("alibaba-coding-plan", "qwen3-max"));
        assert!(is_first_party("moonshotai", "kimi-k2-thinking"));
        assert!(is_first_party("moonshotai-cn", "kimi-k2-thinking"));
        assert!(is_first_party("minimax", "MiniMax-M2.5"));
        assert!(is_first_party("minimax-cn", "MiniMax-M2.5"));
        assert!(!is_first_party("abacus", "MiniMax-M2.5"));
    }

    // With no first-party match, a complete price still beats a cache-less one.
    #[test]
    fn cache_bearing_entry_wins_when_no_first_party() {
        let json = r#"{
            "aaa": { "models": { "acme-1": { "cost": { "input": 2, "output": 4 } } } },
            "bbb": { "models": { "acme-1": { "cost": { "input": 2, "output": 4, "cache_write": 2.5, "cache_read": 0.2 } } } }
        }"#;
        let mut p = empty();
        p.ingest_modelsdev(json);
        let price = p.lookup("acme-1").expect("acme-1 should be priced");
        assert!(approx(price.cache_read, 0.2e-6));
        assert!(approx(price.cache_create, 2.5e-6));
    }

    // Anthropic bills a 1-hour cache write at 2x base input (a 5-minute write is
    // 1.25x, a cache read 0.1x). models.dev publishes only the 5-minute rate, so
    // the 1h rate is derived; without it every cache write in a Claude Code
    // session — which holds its prompt cache for an hour — is billed 60% short.
    #[test]
    fn modelsdev_derives_the_one_hour_cache_rate_for_claude() {
        let json = r#"{
            "anthropic": { "models": { "claude-opus-5-5": { "cost": { "input": 4, "output": 20, "cache_write": 5, "cache_read": 0.2 } } } },
            "openai":    { "models": { "gpt-5.6-sol":     { "cost": { "input": 4, "output": 20, "cache_write": 5, "cache_read": 0.4 } } } }
        }"#;
        let mut p = empty();
        p.ingest_modelsdev(json);
        // Opus 5.5 list rates: $4 in / $20 out / $5 5m write / $8 1h write / $0.20 read
        assert!(approx(
            p.cost("claude-opus-5-5", 0.0, 0.0, 0.0, 1e6, 0.0).unwrap(),
            8.0
        ));
        assert!(approx(
            p.cost("claude-opus-5-5", 0.0, 0.0, 1e6, 0.0, 0.0).unwrap(),
            5.0
        ));
        assert!(approx(
            p.cost("claude-opus-5-5", 1e6, 1e6, 0.0, 0.0, 1e6).unwrap(),
            24.2
        ));
        // A non-Anthropic model has no 1-hour cache, so its creation tokens keep
        // the single published rate instead of inventing a multiplier.
        assert!(approx(
            p.cost("gpt-5.6-sol", 0.0, 0.0, 0.0, 1e6, 0.0).unwrap(),
            5.0
        ));
    }

    // LiteLLM publishes the 1h rate outright (`..._above_1hr`), and a published
    // number beats the derivation.
    #[test]
    fn litellm_published_one_hour_rate_wins_over_the_derivation() {
        let json = r#"{
            "claude-x": { "input_cost_per_token": 5e-6, "output_cost_per_token": 2.5e-5,
                          "cache_creation_input_token_cost": 6.25e-6,
                          "cache_creation_input_token_cost_above_1hr": 1.1e-5,
                          "cache_read_input_token_cost": 5e-7 }
        }"#;
        let mut p = empty();
        p.ingest_litellm(json);
        assert!(approx(
            p.cost("claude-x", 0.0, 0.0, 0.0, 1e6, 0.0).unwrap(),
            11.0
        ));
        assert!(approx(
            p.cost("claude-x", 0.0, 0.0, 1e6, 0.0, 0.0).unwrap(),
            6.25
        ));
    }

    // The backstop table prices the moments right after launch and a first run with
    // neither network nor cache, so a flagship missing from it surfaces as an
    // unpriced model. Guards the Claude 5.x family and the OpenAI ids these users
    // actually run, at their published list rates.
    #[test]
    fn builtin_table_covers_the_current_flagships() {
        let p = Pricing::builtin_only();
        for id in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-mythos-5-1",
            "claude-mythos-5",
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.5",
        ] {
            let price = p
                .lookup(id)
                .unwrap_or_else(|| panic!("{id} is missing from the built-in table"));
            assert!(
                price.input > 0.0 && price.output > 0.0,
                "{id} has no base rate"
            );
        }
        // Anthropic's published table, end to end for the newest Opus
        let opus55 = p.lookup("claude-opus-5-5").unwrap();
        assert!(approx(opus55.input, 4e-6));
        assert!(approx(opus55.output, 20e-6));
        assert!(approx(opus55.cache_create, 5e-6));
        assert!(approx(opus55.cache_create_1h, 8e-6));
        assert!(approx(opus55.cache_read, 0.2e-6));
        // Sonnet 5 is the $2/$10 tier, not the older $3/$15 one
        let sonnet5 = p.lookup("claude-sonnet-5").unwrap();
        assert!(approx(sonnet5.input, 2e-6) && approx(sonnet5.output, 10e-6));
    }
}
