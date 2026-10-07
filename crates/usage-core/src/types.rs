use serde::{Deserialize, Serialize};

/// What a source actually meters. `Credits` sources (Qoder) report zero tokens
/// in their logs, so rendering them as token counts would be a lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Meter {
    Tokens,
    Credits,
}

/// Whether a record's token numbers describe one call or a running session total.
/// Summing a `Cumulative` record per-call inflates history by orders of magnitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageForm {
    PerCall,
    Cumulative,
}

/// Where the model id for a usage record comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAttr {
    /// Carried on the same record as the token counts.
    Inline,
    /// Lives on a different record (Codex `turn_context`) and is resolved by the
    /// adapter before the event leaves it.
    Joined,
    /// The source never records a model.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    Mcp,
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub kind: CallKind,
    pub name: String,
}

/// A quota sample as the source itself reported it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaSample {
    pub used_percent: f64,
    pub window_minutes: i64,
    pub resets_at_ms: i64,
    /// Bucket name when the source distinguishes several windows at once
    /// (e.g. a 5-hour limit and a weekly one).
    pub label: Option<String>,
    /// Stable identity of this window across polls, when the source has one.
    /// A saved ordering keys on this: labels carry live numbers (spent dollars,
    /// remaining credits) and a length derived from a reset instant shrinks on
    /// every poll, so neither can name a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// The four Anthropic-style token stages. They are mutually exclusive, so
/// `total` never double-counts; `reasoning` is a sub-breakdown of `output`
/// (OpenAI-style sources) and is never added on top.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: f64,
    pub cache_creation: f64,
    pub cache_read: f64,
    pub output: f64,
    pub reasoning: f64,
    pub credits: f64,
}

impl TokenCounts {
    pub fn total(&self) -> f64 {
        self.input + self.cache_creation + self.cache_read + self.output
    }

    /// Share of all prompt-side tokens that came out of the cache.
    pub fn cached_pct(&self) -> f64 {
        let prompt = self.input + self.cache_creation + self.cache_read;
        if prompt <= 0.0 {
            0.0
        } else {
            self.cache_read / prompt * 100.0
        }
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0.0 && self.credits == 0.0
    }

    pub fn scaled(&self, k: f64) -> TokenCounts {
        TokenCounts {
            input: self.input * k,
            cache_creation: self.cache_creation * k,
            cache_read: self.cache_read * k,
            output: self.output * k,
            reasoning: self.reasoning * k,
            credits: self.credits * k,
        }
    }
}

impl std::ops::AddAssign<&TokenCounts> for TokenCounts {
    fn add_assign(&mut self, other: &Self) {
        self.input += other.input;
        self.cache_creation += other.cache_creation;
        self.cache_read += other.cache_read;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.credits += other.credits;
    }
}

/// One billable turn, already normalised by its adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageEvent {
    /// Adapter id, e.g. `claude`.
    pub tool: String,
    /// Epoch ms of the turn.
    pub ts_ms: i64,
    pub session: String,
    /// Workspace/project label when the source knows it.
    pub project: Option<String>,
    pub model: Option<String>,
    pub counts: TokenCounts,
    pub meter: Meter,
    /// Stable identity for idempotent ingestion; `None` disables dedupe.
    pub dedupe_key: Option<String>,
    pub calls: Vec<Call>,
    pub quota: Option<QuotaSample>,
    /// Manifest key of the record's origin file, so a truncated log can purge
    /// its own stale events before being re-read.
    pub source: String,
}

impl UsageEvent {
    pub fn new(tool: impl Into<String>, ts_ms: i64, session: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            ts_ms,
            session: session.into(),
            project: None,
            model: None,
            counts: TokenCounts::default(),
            meter: Meter::Tokens,
            dedupe_key: None,
            calls: Vec::new(),
            quota: None,
            source: String::new(),
        }
    }

    pub fn with(mut self, counts: TokenCounts) -> Self {
        self.counts = counts;
        self
    }
}

/// The origin a row's `source` belongs to: `''` for this machine's own files,
/// the machine name for rows a bundle merge stamped as
/// `linux:<origin>:<their key>` (`usage_index::sync`). Origin names with a
/// colon are rejected at the sync boundary, so the first colon after the
/// prefix is always the separator. The SQL twin lives in the index's
/// `ORIGIN_SQL`; both must agree.
pub fn origin_of(source: &str) -> &str {
    match source.strip_prefix("linux:") {
        Some(rest) => rest.split_once(':').map(|(origin, _)| origin).unwrap_or(""),
        None => "",
    }
}

/// Whether `name` can be used as a sync origin. Deliberately narrow: `:`
/// separates origin from key inside `linux:<origin>:<key>` sources and dedupe
/// keys (and would break [`origin_of`] and its SQL twin), `/` and `\` would
/// turn `tokenme-<origin>.jsonl.gz` into a path, and control characters would
/// corrupt manifests and log lines. Everything a hostname can contain, and
/// anything a person would type into the panel's server dialog, still passes.
pub fn origin_ok(name: &str) -> bool {
    !name.trim().is_empty()
        && !name.chars().any(|c| c == ':' || c == '/' || c == '\\' || c.is_control())
}

/// Parse the timestamp dialects seen across sources: RFC 3339 with `Z` or an
/// offset, and bare unix seconds or milliseconds.
pub fn parse_ts_ms(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        let local = chrono::Local.from_local_datetime(&dt).single()?;
        return Some(local.timestamp_millis());
    }
    if let Ok(n) = s.parse::<i64>() {
        // Heuristic: 10 digits are seconds, 13 are milliseconds.
        return Some(if n < 10_000_000_000 { n * 1000 } else { n });
    }
    None
}

use chrono::TimeZone as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_stages_never_overlap() {
        let c = TokenCounts { input: 10.0, cache_creation: 20.0, cache_read: 70.0, output: 5.0, reasoning: 2.0, credits: 0.0 };
        assert_eq!(c.total(), 105.0);
        assert!((c.cached_pct() - 70.0).abs() < 1e-9);
    }

    #[test]
    fn parses_timestamp_dialects() {
        assert!(parse_ts_ms("2026-09-03T16:18:22.972Z").unwrap() > 0);
        assert_eq!(parse_ts_ms("1788452302"), Some(1788452302000));
        assert_eq!(parse_ts_ms("1788452302123"), Some(1788452302123));
        assert_eq!(parse_ts_ms(""), None);
        assert_eq!(parse_ts_ms("not-a-date"), None);
    }

    #[test]
    fn zero_counts_are_not_a_billable_event() {
        assert!(TokenCounts::default().is_zero());
        assert!(!TokenCounts { credits: 0.27, ..Default::default() }.is_zero());
    }

    #[test]
    fn origin_parses_and_validates_symmetrically() {
        assert_eq!(origin_of(""), "");
        assert_eq!(origin_of("/logs/x.jsonl"), "");
        assert_eq!(origin_of("linux:ops-box:/logs/x.jsonl"), "ops-box");
        // No colon after the prefix: not a stamped source, not an origin.
        assert_eq!(origin_of("linux:ops-box"), "");

        assert!(origin_ok("ops-box"));
        assert!(origin_ok("服务器 01")); // names a person would type
        assert!(!origin_ok(""));
        assert!(!origin_ok("   "));
        assert!(!origin_ok("a:b"));
        assert!(!origin_ok("a/b"));
        assert!(!origin_ok("a\\b"));
        assert!(!origin_ok("a\nb"));
    }
}
