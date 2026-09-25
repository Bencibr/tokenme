//! Token prices. Primary source: <https://models.dev/api.json>. Backstop: a
//! trimmed snapshot bundled at build time, so the first launch with no network
//! still prices the common models.
//!
//! All prices are USD per **one million** tokens, matching models.dev units.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::TokenCounts;

const MODELSDEV_URL: &str = "https://models.dev/api.json";
const BUNDLED: &str = include_str!("../snapshots/pricing_fallback.json");
const FRESH_MS: i64 = 24 * 60 * 60 * 1000;

/// Providers whose price wins when several list the same bare model id, so a
/// reseller's copy never beats the vendor's own entry.
const VENDOR_PRIORITY: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "google-vertex",
    "amazon-bedrock",
    "xai",
    "deepseek",
    "zhipuai",
    "moonshotai",
    "qwen",
    "minimax",
    "stepfun",
    "mistralai",
    "groq",
    "openrouter",
];

/// USD per credit for the tools that meter in credits instead of tokens.
///
/// A credit is a vendor's own unit, but it has a published money value, and the
/// panel needs one column that is comparable across sources. Qoder's docs
/// (`docs.qoder.com/zh/account/pricing`, read 2026-09-24) price Pro at
/// "20 USD/月 · 每月 2,000 Credits" and Pro+ at "60 USD/月 · 6,000 Credits" — two
/// independent rows that both give 0.01 USD per credit, which is the anchor used
/// here; the Ultra tier ("200 USD/月 · 20,000 Credits") is the third row that
/// lands on the same 0.01. The "20 美元 / 1,500 Credits" add-on pack (0.0133) is
/// deliberately not used: that is a top-up price, not the plan price. Qoder
/// publishes no credits-to-tokens ratio at all, so nothing here invents one.
///
/// Money computed this way is an **estimate of a subscription allowance**, not a
/// bill: the vendor converts tokens into credits per model and says the rates may
/// change, so callers surface it as such via [`crate::Summary::credit_cost`].
pub const CREDIT_USD: &[(&str, f64, &str)] = &[("qoder", 0.01, "docs.qoder.com/zh/account/pricing · Pro $20 = 2,000 Credits (2026-09-24)")];

/// `(usd_per_credit, provenance)` when this tool's credits carry a published price.
pub fn credit_rate(tool: &str) -> Option<(f64, &'static str)> {
    CREDIT_USD
        .iter()
        .find(|(id, _, _)| *id == tool)
        .map(|(_, rate, src)| (*rate, *src))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_creation: f64,
    pub cache_read: f64,
}

impl Price {
    /// models.dev shape: `cost: { input, output, cache_read, cache_write }`.
    fn from_modelsdev(v: &Value) -> Option<Price> {
        let c = v.get("cost")?;
        let num = |k: &str| c.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        Some(Price {
            input: num("input"),
            output: num("output"),
            cache_creation: num("cache_write").max(num("cache_creation")),
            cache_read: num("cache_read"),
        })
    }

    pub fn cost_of(&self, c: &TokenCounts) -> f64 {
        (c.input * self.input
            + c.cache_creation * self.cache_creation
            + c.cache_read * self.cache_read
            + c.output * self.output)
            / 1_000_000.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingSource {
    /// Fresh network fetch this run.
    ModelsDev,
    /// Local cache (fresh, or stale because the network was unavailable).
    Cache,
    /// Snapshot compiled into the binary.
    Bundled,
    /// Nothing usable at all; every cost is reported as unknown.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricingMeta {
    pub source: PricingSource,
    pub fetched_at_ms: i64,
    /// True when the payload is older than 24h and no fetch succeeded.
    pub stale: bool,
    pub key_count: usize,
    pub cache_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct Entry {
    price: Price,
    context: Option<i64>,
    provider: String,
}

/// What [`PricingMap::explain`] answers.
#[derive(Debug, Clone, Copy)]
pub struct Explanation<'a> {
    pub provider: &'a str,
    pub price: Price,
    /// Other listings for the same model, in the order the precedence rule would
    /// have taken them. Empty means the price was not a choice.
    pub alternatives: &'a [(String, Price)],
}

#[derive(Debug, Clone, Default)]
pub struct PricingOptions {
    pub offline: bool,
    pub cache_dir: Option<PathBuf>,
    /// Normalised model id → price. Consulted before any table, wins outright.
    pub overrides: HashMap<String, Price>,
}

impl PricingOptions {
    pub fn default_cache_dir() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("tokenme"))
    }
}

#[derive(Debug, Clone)]
pub struct PricingMap {
    table: HashMap<String, Entry>,
    /// Every other price that was on offer for a key the precedence rule decided
    /// between, so a number can be traced to the vendor it came from instead of
    /// being taken on faith. 587 of models.dev's keys carry more than one price.
    alternatives: HashMap<String, Losers>,
    meta: PricingMeta,
    overrides: HashMap<String, Price>,
}

impl Default for PricingMap {
    fn default() -> Self {
        Self::load(&PricingOptions { offline: true, cache_dir: None, overrides: Default::default() })
    }
}

/// `anthropic/claude-sonnet-4-6`, `Claude Sonnet 4.6`, `glm-5.1` and `glm-5p1`
/// must all reach the same row.
pub fn normalize_key(s: &str) -> String {
    let mut t: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    // Drop routing prefixes: `openrouter/anthropic/claude-x`, `bedrock/anthropic.claude-x`.
    while let Some(pos) = t.find('/') {
        t.replace_range(..pos + 1, "");
    }
    // `vendor:llama-3.3-70b` aliases carry the model after the colon.
    if let Some((_, rest)) = t.split_once(':') {
        if !rest.is_empty() {
            t = rest.to_string();
        }
    }
    t.replace('.', "p").replace('_', "-")
}

/// Every listing that lost the precedence rule for one model key, still in rank
/// order so `[0]` is "the next pick".
type Losers = Vec<(String, Price)>;

/// One entry per model key, chosen by [`provider_rank`], plus the listings it beat.
fn assemble(parsed: HashMap<String, Vec<Entry>>) -> (HashMap<String, Entry>, HashMap<String, Losers>) {
    let mut table: HashMap<String, Entry> = HashMap::new();
    let mut alternatives: HashMap<String, Losers> = HashMap::new();
    for (key, entries) in parsed {
        let mut entries = entries;
        entries.sort_by_key(|e| (provider_rank(&e.provider), e.provider.clone()));
        let winner = entries.remove(0);
        let others: Losers =
            entries.into_iter().filter(|e| e.price != winner.price).map(|e| (e.provider, e.price)).collect();
        if !others.is_empty() {
            alternatives.insert(key.clone(), others);
        }
        table.entry(key).or_insert(winner);
    }
    (table, alternatives)
}

fn provider_rank(provider: &str) -> usize {
    let p = provider.to_ascii_lowercase();
    VENDOR_PRIORITY
        .iter()
        .position(|v| *v == p)
        // Coding-plan clones (`zhipuai-coding-plan`) rank just behind the base vendor.
        .or_else(|| VENDOR_PRIORITY.iter().position(|v| p.starts_with(v)))
        .unwrap_or(VENDOR_PRIORITY.len())
}

/// `None` for anything that isn't a believable price table — a CDN error page or
/// a rate-limit envelope parses as JSON, and caching it would blank out pricing
/// for a full day.
fn parse_models_payload(text: &str) -> Option<HashMap<String, Vec<Entry>>> {
    let root: Value = serde_json::from_str(text).ok()?;
    // models.dev answers with providers at the top level, no wrapper object.
    let providers = root.get("providers").cloned().unwrap_or_else(|| root.clone());
    let obj = providers.as_object()?;
    let mut out: HashMap<String, Vec<Entry>> = HashMap::new();
    let mut usable_providers = 0usize;
    for (provider, body) in obj {
        let Some(models) = body.get("models").and_then(Value::as_object) else { continue };
        if models.is_empty() {
            continue;
        }
        usable_providers += 1;
        for (id, model) in models {
            let Some(price) = Price::from_modelsdev(model) else { continue };
            let context = model
                .get("limit")
                .and_then(|l| l.get("context").or_else(|| l.get("output")))
                .and_then(Value::as_i64);
            let entry = Entry { price, context, provider: provider.clone() };
            // A slash-bearing model id (`deepseek/deepseek-v4-flash`) already loses
            // its prefix to `normalize_key`, so the qualified alias is the same key
            // — registering it twice would put one listing on record as two.
            let bare = normalize_key(id);
            let qualified = normalize_key(&format!("{provider}/{id}"));
            if qualified == bare {
                out.entry(bare).or_default().push(entry);
            } else {
                out.entry(bare).or_default().push(entry.clone());
                out.entry(qualified).or_default().push(entry);
            }
        }
    }
    if usable_providers < 10 {
        return None;
    }
    Some(out)
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn file_mtime_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

/// Atomically replace a file: write a sibling, then rename.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    crate::replace_file(&tmp, path)
}

struct Payload {
    parsed: Option<HashMap<String, Vec<Entry>>>,
    source: PricingSource,
    at_ms: i64,
    stale: bool,
}

/// Read a fresh (<24h) cache, else conditional-GET models.dev, else any usable
/// cache, else the bundled snapshot.
fn load_payload(cache_dir: Option<&Path>, offline: bool) -> Payload {
    let body_path = cache_dir.map(|d| d.join("models.dev.json"));
    let etag_path = cache_dir.map(|d| d.join("models.dev.etag"));

    let read_cache = |stale: bool| -> Option<Payload> {
        let p = body_path.as_ref()?;
        let text = std::fs::read_to_string(p).ok()?;
        let parsed = parse_models_payload(&text)?;
        Some(Payload { parsed: Some(parsed), source: PricingSource::Cache, at_ms: file_mtime_ms(p).unwrap_or(0), stale })
    };

    if let Some(cached) = read_cache(false) {
        if now_ms() - cached.at_ms < FRESH_MS {
            return cached;
        }
    }
    if offline {
        return read_cache(true).unwrap_or_else(bundled);
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout_read(Duration::from_secs(10))
        .build();
    let mut req = agent.get(MODELSDEV_URL);
    if let Some(p) = etag_path.as_ref() {
        if let Ok(etag) = std::fs::read_to_string(p) {
            let etag = etag.trim().to_string();
            if !etag.is_empty() {
                req = req.set("If-None-Match", &etag);
            }
        }
    }
    match req.call() {
        Ok(resp) if resp.status() == 304 => {
            // Body unchanged: bump the cache mtime so we stop re-asking today.
            if let Some(cached) = read_cache(false) {
                if let Some(p) = body_path.as_ref() {
                    if let Ok(text) = std::fs::read_to_string(p) {
                        let _ = write_atomic(p, text.as_bytes());
                    }
                }
                return Payload { stale: false, at_ms: now_ms(), ..cached };
            }
            bundled()
        }
        Ok(resp) => {
            let etag = resp.header("etag").map(|s| s.to_string());
            let text = resp.into_string().unwrap_or_default();
            match parse_models_payload(&text) {
                Some(parsed) => {
                    if let Some(p) = body_path.as_ref() {
                        let _ = write_atomic(p, text.as_bytes());
                    }
                    if let (Some(p), Some(etag)) = (etag_path.as_ref(), etag.as_deref()) {
                        let _ = write_atomic(p, etag.as_bytes());
                    }
                    Payload { parsed: Some(parsed), source: PricingSource::ModelsDev, at_ms: now_ms(), stale: false }
                }
                // A bad response must not destroy a good cache.
                None => read_cache(true).unwrap_or_else(bundled),
            }
        }
        Err(_) => read_cache(true).unwrap_or_else(bundled),
    }
}

fn bundled() -> Payload {
    Payload {
        parsed: parse_models_payload(BUNDLED),
        source: PricingSource::Bundled,
        at_ms: 0,
        stale: true,
    }
}

impl PricingMap {
    pub fn load(opts: &PricingOptions) -> Self {
        let payload = load_payload(opts.cache_dir.as_deref(), opts.offline);
        let (table, alternatives) = assemble(payload.parsed.unwrap_or_default());
        Self {
            table,
            alternatives,
            meta: PricingMeta {
                source: payload.source,
                fetched_at_ms: payload.at_ms,
                stale: payload.stale,
                key_count: 0,
                cache_dir: opts.cache_dir.clone(),
            },
            overrides: opts.overrides.clone(),
        }
        .with_key_count()
    }

    fn with_key_count(mut self) -> Self {
        self.meta.key_count = self.table.len() + self.overrides.len();
        self
    }

    /// Drop the cache and re-download (the app's manual "刷新价格" action).
    pub fn refresh(opts: &PricingOptions) -> Self {
        if let Some(dir) = opts.cache_dir.as_ref() {
            let _ = std::fs::remove_file(dir.join("models.dev.json"));
            let _ = std::fs::remove_file(dir.join("models.dev.etag"));
        }
        Self::load(&PricingOptions { offline: false, ..opts.clone() })
    }

    pub fn meta(&self) -> &PricingMeta {
        &self.meta
    }

    pub fn key_count(&self) -> usize {
        self.meta.key_count
    }

    /// The provider whose price a model resolves to, plus the ones it did not.
    ///
    /// `deepseek-v4-flash` is listed by ~80 providers between $0 and $0.44/M, so
    /// "the price" is only meaningful together with "which listing won": the
    /// precedence is [`provider_rank`], and an empty alternatives list means there
    /// was nothing to choose between.
    pub fn explain(&self, model: &str) -> Option<Explanation<'_>> {
        for key in lookup_keys(model) {
            if let Some(price) = self.overrides.get(&key) {
                return Some(Explanation { provider: "override", price: *price, alternatives: &[] });
            }
            if let Some(e) = self.table.get(&key) {
                return Some(Explanation {
                    provider: &e.provider,
                    price: e.price,
                    alternatives: self.alternatives.get(&key).map(Vec::as_slice).unwrap_or(&[]),
                });
            }
        }
        let stripped = strip_suffix(model);
        if stripped != model {
            return self.explain(&stripped);
        }
        None
    }

    pub fn price_for(&self, model: &str) -> Option<Price> {
        for key in lookup_keys(model) {
            if let Some(p) = self.overrides.get(&key) {
                return Some(*p);
            }
            if let Some(e) = self.table.get(&key) {
                return Some(e.price);
            }
        }
        // Routing suffixes like `claude-sonnet-4-6[1m]` or `model (200k)`.
        let stripped = strip_suffix(model);
        if stripped != model {
            return self.price_for(&stripped);
        }
        None
    }

    pub fn context_window(&self, model: &str) -> Option<i64> {
        for key in lookup_keys(model) {
            if let Some(e) = self.table.get(&key) {
                if e.context.is_some() {
                    return e.context;
                }
            }
        }
        None
    }

    /// `None` means "no price known": callers still count the tokens and label
    /// the model unpriced rather than reporting a misleading $0.
    pub fn cost(&self, model: Option<&str>, counts: &TokenCounts) -> Option<f64> {
        let price = self.price_for(model?)?;
        Some(price.cost_of(counts))
    }

    pub fn price_of(&self, model: Option<&str>) -> Option<Price> {
        model.and_then(|m| self.price_for(m))
    }
}

fn strip_suffix(model: &str) -> String {
    match model.find(['[', ' ', '(']) {
        Some(0) | None => model.to_string(),
        Some(i) => model[..i].to_string(),
    }
}

/// Candidate keys, most specific first.
fn lookup_keys(model: &str) -> Vec<String> {
    let mut out = vec![model.trim().to_ascii_lowercase(), normalize_key(model)];
    if let Some(pos) = model.rfind('/') {
        out.push(normalize_key(&model[pos + 1..]));
    }
    out.retain(|k| !k.is_empty());
    out.dedup();
    out
}

/// Parse `model=in/out/cache_creation/cache_read` overrides from the CLI.
pub fn parse_override(spec: &str) -> Option<(String, Price)> {
    let (model, nums) = spec.split_once('=')?;
    let mut it = nums.split('/');
    let n = |s: Option<&str>| s.and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let price = Price {
        input: n(it.next()),
        output: n(it.next()),
        cache_creation: n(it.next()),
        cache_read: n(it.next()),
    };
    if model.trim().is_empty() {
        return None;
    }
    Some((normalize_key(model), price))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map assembled from a caller-supplied payload, for the tests below.
    /// `parse_models_payload` refuses anything with fewer than ten providers (an
    /// error page is a JSON object too), so the padding is part of the harness.
    fn map_from(payload: &str) -> PricingMap {
        let mut root: serde_json::Value = serde_json::from_str(payload).unwrap();
        let providers = root.get_mut("providers").and_then(Value::as_object_mut).unwrap();
        for i in 0..12 {
            let name = format!("filler{i}");
            providers.insert(
                name.clone(),
                serde_json::json!({"models": {format!("model-{i}"): {"cost": {"input": 1.0, "output": 2.0}}}}),
            );
        }
        let (table, alternatives) = assemble(parse_models_payload(&root.to_string()).unwrap());
        let keys = table.len();
        PricingMap {
            table,
            alternatives,
            meta: PricingMeta {
                source: PricingSource::ModelsDev,
                fetched_at_ms: 0,
                stale: false,
                key_count: keys,
                cache_dir: None,
            },
            overrides: Default::default(),
        }
    }

    /// The precedence is a decision, so it has to be inspectable: `explain` says
    /// which listing won and what it beat.
    #[test]
    fn a_contested_price_is_traceable_to_the_provider_that_won() {
        let map = map_from(
            r#"{"providers":{
                 "sensenova":{"models":{"deepseek-v4-flash":{"cost":{"input":0.0,"output":0.0}}}},
                 "nano-gpt":{"models":{"deepseek/deepseek-v4-flash":{"cost":{"input":0.14,"output":0.28}}}},
                 "deepseek":{"models":{"deepseek-v4-flash":{"cost":{"input":0.15,"output":0.6,"cache_read":0.003}}}}
               }}"#,
        );
        let e = map.explain("deepseek-v4-flash").expect("priced");
        assert_eq!(e.provider, "deepseek", "the vendor's own listing wins the ranking");
        assert_eq!((e.price.input, e.price.output), (0.15, 0.6));
        assert_eq!(e.alternatives.len(), 2, "the two it beat are still on record: {:?}", e.alternatives);
        assert!(e.alternatives.iter().any(|(p, _)| p == "sensenova"), "including the free clone");
        // A prefixed alias routes to the same entry; the prefix does not pick the price.
        assert_eq!(map.explain("nano-gpt/deepseek/deepseek-v4-flash").map(|x| x.provider), Some("deepseek"));
        assert!(map.explain("no-such-model-at-all").is_none());
    }

    #[test]
    fn a_free_clone_never_outprices_a_vendor_that_charges() {
        let map = map_from(
            r#"{"providers":{
                 "aaa-free-plan":{"models":{"solo-model-x":{"cost":{"input":0.0,"output":0.0}}}},
                 "groq":{"models":{"solo-model-x":{"cost":{"input":0.5,"output":0.9}}}}
               }}"#,
        );
        let e = map.explain("solo-model-x").expect("priced");
        assert_eq!(e.provider, "groq", "a ranked vendor beats an alphabetically earlier free clone");
        assert_eq!((e.price.input, e.price.output), (0.5, 0.9));
    }

    /// The one case where 0 is the answer: nothing else lists the model.
    #[test]
    fn a_free_only_model_stays_free() {
        let map = map_from(r#"{"providers":{"only-plan":{"models":{"free-only-z":{"cost":{"input":0.0,"output":0.0}}}}}}"#);
        let e = map.explain("free-only-z").expect("priced");
        assert_eq!(e.provider, "only-plan");
        assert!(e.alternatives.is_empty(), "nothing was contested, so nothing is hidden");
        assert_eq!(e.price.input, 0.0);
    }

    #[test]
    fn vendor_prefixes_and_dots_collapse() {
        assert_eq!(normalize_key("openrouter/anthropic/claude-sonnet-4.6"), "claude-sonnet-4p6");
        assert_eq!(normalize_key("glm-5.1"), normalize_key("glm-5p1"));
        assert_eq!(normalize_key("Claude Sonnet 4.6"), "claudesonnet4p6");
    }

    #[test]
    fn four_stages_are_priced_separately() {
        let p = Price { input: 3.0, output: 15.0, cache_creation: 3.75, cache_read: 0.3 };
        let c = TokenCounts {
            input: 1_000_000.0,
            cache_creation: 1_000_000.0,
            cache_read: 1_000_000.0,
            output: 1_000_000.0,
            ..Default::default()
        };
        assert!((p.cost_of(&c) - 22.05).abs() < 1e-9);
    }

    #[test]
    fn bundled_snapshot_prices_anthropic_offline() {
        let map = PricingMap::load(&PricingOptions { offline: true, cache_dir: None, overrides: Default::default() });
        let p = map.price_for("claude-sonnet-4-6").expect("bundled snapshot must price sonnet");
        assert!(p.input > 0.0 && p.output > p.input);
        assert_eq!(map.meta().source, PricingSource::Bundled);
        assert!(map.key_count() > 1000, "snapshot too small: {}", map.key_count());
    }

    #[test]
    fn routing_suffix_and_provider_form_resolve_to_one_price() {
        let map = PricingMap::load(&PricingOptions { offline: true, cache_dir: None, overrides: Default::default() });
        let bare = map.price_for("claude-sonnet-4-6");
        assert_eq!(map.price_for("anthropic/claude-sonnet-4-6"), bare);
        assert_eq!(map.price_for("claude-sonnet-4-6[1m]"), bare);
    }

    #[test]
    fn error_envelope_is_not_a_price_table() {
        assert!(parse_models_payload(r#"{"error":"rate limited"}"#).is_none());
        assert!(parse_models_payload("not json").is_none());
        assert!(parse_models_payload(r#"{"anthropic":{"models":{}}}"#).is_none());
    }

    #[test]
    fn unknown_model_is_unpriced_not_free() {
        let map = PricingMap::load(&PricingOptions { offline: true, cache_dir: None, overrides: Default::default() });
        assert!(map.price_for("definitely-not-a-model-xyz").is_none());
        assert!(map.cost(Some("definitely-not-a-model-xyz"), &TokenCounts { input: 100.0, ..Default::default() }).is_none());
    }

    #[test]
    fn override_spec_parses() {
        let (k, p) = parse_override("glm-5.2=2/8/2.5/0.4").unwrap();
        assert_eq!(k, "glm-5p2");
        assert_eq!(p.cache_creation, 2.5);
        assert_eq!(p.cache_read, 0.4);
        assert!(parse_override("bogus").is_none());
    }
}
