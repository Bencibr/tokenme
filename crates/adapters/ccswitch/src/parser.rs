//! One `proxy_request_logs` row → one [`UsageEvent`].
//!
//! ## The `input_token_semantics` column, verbatim from CC Switch's own source
//!
//! `farion1231/cc-switch` @ `src-tauri/src/services/sql_helpers.rs:29-32` defines
//! the enum and `:25` the providers it applies to:
//!
//! ```text
//! 0 LEGACY  written before the column existed: `input_tokens` already contained
//!           the cache *read* prefix but not the cache *write* tokens
//! 1 TOTAL   `input_tokens` is the provider's total prompt (reads AND writes in)
//! 2 FRESH   `input_tokens` is already the uncached remainder (Anthropic shape)
//! ```
//!
//! Which convention a row uses is a property of the *routed* tool, not of the row:
//! `CACHE_INCLUSIVE_APP_TYPES = ["codex", "gemini", "grokbuild"]`
//! (`sql_helpers.rs:25`) store a total, everything else stores fresh, and
//! `proxy/usage/logger.rs:123-129` stamps exactly that at write time.
//!
//! [`net_input`] is a line-for-line port of `sql_helpers::fresh_input_sql`
//! (`:53-67`), including its two guards (`input >= cache_read + cache_creation`
//! for TOTAL, `input >= cache_read` for LEGACY): when a guard fails the vendor
//! leaves `input_tokens` untouched rather than clamping it, so we do the same and
//! count the row in [`crate::Census::semantics_guards_failed`] instead of silently
//! inventing a smaller bill.
//!
//! ## Money stays in the vendor's database
//!
//! `input_cost_usd` / `output_cost_usd` / `cache_read_cost_usd` /
//! `cache_creation_cost_usd` / `total_cost_usd` are read into
//! [`crate::Census::vendor_cost_usd`] for the pricing cross-check only and are
//! never attached to an event: tokenme prices every stage centrally from the
//! shared models.dev table, and a source's own arithmetic must not leak in or the
//! cross-check would be vacuous. They are also *not* comparable as-is — CC Switch
//! multiplies its total by `providers.cost_multiplier`
//! (`proxy/usage/calculator.rs`, applied at `services/usage_stats.rs:1902-1929`)
//! and prices on `pricing_model`, not on `model`.

use usage_core::{Meter, TokenCounts, UsageEvent};

/// `sql_helpers.rs:30` — pre-column rows, input already held the cache reads.
pub(crate) const SEMANTICS_LEGACY: i64 = 0;
/// `sql_helpers.rs:31` — input is the provider's whole prompt, reads and writes.
pub(crate) const SEMANTICS_TOTAL: i64 = 1;
/// `sql_helpers.rs:32` — input is already uncached; subtract nothing.
pub(crate) const SEMANTICS_FRESH: i64 = 2;

/// `sql_helpers.rs:25` (`CACHE_INCLUSIVE_APP_TYPES`), the single source of truth
/// the vendor's writer, backfill and reader all share.
pub(crate) const CACHE_INCLUSIVE_APP_TYPES: &[&str] = &["codex", "gemini", "grokbuild"];

/// A row as this adapter needs it. `data_source` is kept for the census even
/// though the gate has already excluded the mirrored values.
#[derive(Debug, Clone, Default)]
pub(crate) struct Row {
    pub rowid: i64,
    pub request_id: String,
    pub provider_id: String,
    pub app_type: String,
    pub data_source: String,
    pub model: String,
    pub request_model: String,
    pub session_id: String,
    pub created_at: i64,
    pub status_code: i64,
    pub semantics: i64,
    pub input: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub output: i64,
    pub vendor_cost_usd: f64,
    pub cost_multiplier: String,
}

/// Uncached prompt tokens, i.e. what the `input` stage of [`TokenCounts`] means.
///
/// Second element is `true` when a guard from `fresh_input_sql` failed, so the
/// value returned is the raw column rather than a netted one.
pub(crate) fn net_input(row: &Row) -> (i64, bool) {
    let (read, write, input) = (row.cache_read.max(0), row.cache_creation.max(0), row.input.max(0));
    // The vendor's `ELSE input_tokens`: a fresh-reporting tool is never netted,
    // whatever stamp its row carries.
    if !CACHE_INCLUSIVE_APP_TYPES.contains(&row.app_type.as_str()) || row.semantics == SEMANTICS_FRESH {
        return (input, false);
    }
    if row.semantics == SEMANTICS_TOTAL {
        return if input >= read + write { (input - read - write, false) } else { (input, true) };
    }
    if row.semantics == SEMANTICS_LEGACY {
        return if input >= read { (input - read, false) } else { (input, true) };
    }
    // A stamp this build does not know: report the column untouched and say so.
    (input, true)
}

/// Everything a row contributes to `session`: the id of the thread that owns the
/// call, taken from the `request_id` namespace CC Switch builds per tool.
///
/// `proxy/usage/parser.rs:73-84` (`dedup_request_id`) shows the shape:
/// `<source>:<tool-side id>[:<call index>]`, e.g.
/// `codex_session:thread-v1:01a02e2d-…:461` (Codex thread id, then call index) or
/// `opencode_session:ses_f31d81ea…:msg_0ce2807c…`. The id we want is therefore the
/// *second-to-last* segment once the source tag is dropped — with two fallbacks:
/// the row's own `session_id` column (which is what the vendor keeps alongside it)
/// and finally a synthetic `ccswitch-row<rowid>`, because a row with no session is
/// still a billable call.
pub(crate) fn session_of(row: &Row) -> String {
    let rid = row.request_id.trim();
    if rid.contains(':') {
        let segments: Vec<&str> = rid.split(':').filter(|s| !s.trim().is_empty()).collect();
        // `pi_session:<hash>` / `session:msg_…` carry the id in the last segment;
        // `codex_session:thread-v1:<uuid>:461` in the one before the call index.
        let candidate = if segments.len() >= 3 {
            segments[segments.len() - 2]
        } else if segments.len() == 2 {
            segments[1]
        } else {
            ""
        };
        if !candidate.is_empty() && candidate != row.app_type {
            return candidate.to_string();
        }
    }
    let owned = row.session_id.trim();
    if !owned.is_empty() {
        return owned.to_string();
    }
    format!("ccswitch-row{}", row.rowid)
}

/// `<app_type>/<provider_id>`, e.g. `codex/_codex_session`.
///
/// The event's own `tool` stays `ccswitch` (the panel gets one additive gateway
/// row), so the routed client and the provider that actually served it survive
/// only here — `provider_id` is the `providers.id` primary key, and the
/// `_…_session` placeholders are the vendor's own stand-ins for imported traffic
/// (`session_usage_codex.rs:1574` and siblings).
pub(crate) fn project_of(row: &Row) -> String {
    let provider = row.provider_id.trim();
    if provider.is_empty() {
        return row.app_type.clone();
    }
    format!("{}/{}", row.app_type, provider)
}

/// `model`, falling back to `request_model` when it is NULL or the empty/`unknown`
/// placeholder the error rows carry.
pub(crate) fn model_of(row: &Row) -> Option<String> {
    [row.model.as_str(), row.request_model.as_str()]
        .into_iter()
        .map(str::trim)
        .find(|m| !m.is_empty() && *m != "unknown")
        .map(str::to_string)
}

/// `created_at` is unix **seconds** on every write path:
/// `chrono::Utc::now().timestamp()` for live proxy traffic
/// (`proxy/usage/logger.rs:121`) and `.timestamp()` / `as_secs()` for imported
/// session rows (`services/session_usage.rs:804-817`). Promoted with the same
/// <1e10 cutoff [`usage_core::parse_ts_ms`] uses, so a build that switched to ms
/// still lands on the right instant.
pub(crate) fn ts_of(row: &Row) -> Option<i64> {
    let v = row.created_at;
    if v <= 0 {
        return None;
    }
    Some(if v < 10_000_000_000 { v * 1000 } else { v })
}

/// The stable identity: the vendor's own `request_id TEXT PRIMARY KEY`
/// (`database/schema.rs:199`), which `proxy/usage/logger.rs:131-160` deliberately
/// keeps unique per call (a colliding id is re-keyed to
/// `<request_id>:collision:<sha256>` instead of overwriting the first row).
pub(crate) fn dedupe_key_of(row: &Row) -> String {
    let rid = row.request_id.trim();
    if rid.is_empty() {
        return format!("ccswitch#row{}", row.rowid);
    }
    format!("ccswitch#{rid}")
}

/// `Ok(None)` means "visited, nothing billable": every stage 0, or no usable
/// timestamp. `Err` is never produced here.
pub(crate) fn build_event(row: &Row, source_key: &str) -> Option<UsageEvent> {
    let counts = token_counts(row);
    if counts.is_zero() {
        return None;
    }
    let ts_ms = ts_of(row)?;
    let mut event = UsageEvent::new(crate::TOOL_ID, ts_ms, session_of(row));
    event.project = Some(project_of(row));
    event.model = model_of(row);
    event.counts = counts;
    event.meter = Meter::Tokens;
    event.dedupe_key = Some(dedupe_key_of(row));
    // `rowid` inside `source` lets a pruned table purge exactly its own events:
    // the rollup job deletes rows older than 30 days (`usage_rollup.rs:62,177`).
    event.source = format!("{source_key}#{}#{}", crate::paths::TABLE, row.rowid);
    Some(event)
}

/// The four mutually-exclusive stages (`TokenCounts::total` must never
/// double-count the cached prefix).
///
/// `reasoning` stays 0: the vendor's `TokenUsage` (`proxy/usage/parser.rs:57-71`)
/// has no reasoning field at all — OpenAI-style `output_tokens` already includes
/// it, so reporting it would be a guess.
pub(crate) fn token_counts(row: &Row) -> TokenCounts {
    let (input, _) = net_input(row);
    TokenCounts {
        input: input.max(0) as f64,
        cache_creation: row.cache_creation.max(0) as f64,
        cache_read: row.cache_read.max(0) as f64,
        output: row.output.max(0) as f64,
        reasoning: 0.0,
        credits: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(app: &str, semantics: i64, input: i64, read: i64, write: i64) -> Row {
        Row {
            rowid: 1,
            request_id: "req-1".into(),
            provider_id: "_codex_session".into(),
            app_type: app.into(),
            data_source: "proxy".into(),
            model: "gpt-5.6-luna".into(),
            request_model: String::new(),
            session_id: "sess-1".into(),
            created_at: 1_790_165_270,
            status_code: 200,
            semantics,
            input,
            cache_read: read,
            cache_creation: write,
            output: 40,
            vendor_cost_usd: 0.0,
            cost_multiplier: "1.0".into(),
        }
    }

    #[test]
    fn total_rows_lose_both_cache_stages() {
        let r = row("codex", SEMANTICS_TOTAL, 1000, 700, 200);
        assert_eq!(net_input(&r), (100, false));
        assert_eq!(token_counts(&r).total(), 100.0 + 700.0 + 200.0 + 40.0, "cached prefix billed once");
    }

    #[test]
    fn legacy_rows_lose_only_the_cache_reads() {
        // v12 and earlier: reads folded in, writes not (usage_stats.rs:1921-1923).
        let r = row("codex", SEMANTICS_LEGACY, 1000, 700, 200);
        assert_eq!(net_input(&r), (300, false));
    }

    #[test]
    fn fresh_rows_and_non_inclusive_apps_are_never_deducted() {
        // The FRESH arm wins over the app_type test, …
        let r = row("codex", SEMANTICS_FRESH, 100, 700, 0);
        assert_eq!(net_input(&r), (100, false), "Claude-style input: 1941 next to 179264 cached");
        // … and a LEGACY stamp on a fresh-reporting tool (claude, opencode, pi)
        // must not be netted either: `fresh_input_sql` gates on app_type.
        let r = row("claude", SEMANTICS_LEGACY, 100, 700, 0);
        assert_eq!(net_input(&r), (100, false));
    }

    #[test]
    fn a_broken_guard_leaves_the_column_alone() {
        // input < cache_read: impossible for a well-formed row, and the vendor's
        // CASE falls through to `ELSE input_tokens` rather than clamping to 0.
        let r = row("codex", SEMANTICS_TOTAL, 100, 700, 0);
        assert_eq!(net_input(&r), (100, true));
        let r = row("codex", SEMANTICS_LEGACY, 100, 700, 0);
        assert_eq!(net_input(&r), (100, true));
    }

    #[test]
    fn session_prefers_the_id_named_inside_request_id() {
        let mut r = row("codex", SEMANTICS_LEGACY, 10, 0, 0);
        r.request_id = "codex_session:thread-v1:01a02e2d-a489-7200-8682-64f8b0f2923e:461".into();
        r.session_id = "01a02e2d-a489-7200-8682-64f8b0f2923e".into();
        assert_eq!(session_of(&r), "01a02e2d-a489-7200-8682-64f8b0f2923e", "thread id, not the call index");
        r.request_id = "opencode_session:ses_f31d81ea3ffecWpfHkX0AD9YXf:msg_0ce2807cf001oP0BJZpCTbm19H".into();
        assert_eq!(session_of(&r), "ses_f31d81ea3ffecWpfHkX0AD9YXf");
        r.request_id = "pi_session:3eb8ecde6578c492547c6067e718dee0".into();
        assert_eq!(session_of(&r), "3eb8ecde6578c492547c6067e718dee0");
        r.request_id = "session:msg_20260923200414981f2a3401ce4c46".into();
        r.session_id = "e533282f-a0a8-414a-b3b0-c7329fabad94".into();
        assert_eq!(session_of(&r), "msg_20260923200414981f2a3401ce4c46", "the vendor keys Claude rows by message id");
        r.request_id = "bare-id".into();
        assert_eq!(session_of(&r), "e533282f-a0a8-414a-b3b0-c7329fabad94", "falls back to session_id");
        r.session_id = String::new();
        assert_eq!(session_of(&r), "ccswitch-row1", "and finally to the rowid");
    }

    #[test]
    fn project_carries_the_routed_tool_and_provider() {
        let r = row("codex", SEMANTICS_LEGACY, 10, 0, 0);
        assert_eq!(project_of(&r), "codex/_codex_session");
        let mut r = r;
        r.provider_id = "  ".into();
        assert_eq!(project_of(&r), "codex");
    }

    #[test]
    fn model_falls_back_to_the_requested_alias() {
        // Routing rewrites the client alias into `request_model`, so an
        // `unknown`/empty `model` on a rewritten request is recoverable.
        let mut r = row("claude", SEMANTICS_FRESH, 10, 0, 0);
        r.model = "unknown".into();
        r.request_model = "glm-5.3-flash".into();
        assert_eq!(model_of(&r).as_deref(), Some("glm-5.3-flash"));
        r.request_model = String::new();
        assert_eq!(model_of(&r), None, "nothing known stays unknown");
        r.model = "gpt-5.6-luna".into();
        assert_eq!(model_of(&r).as_deref(), Some("gpt-5.6-luna"));
    }

    #[test]
    fn dedupe_key_is_the_vendor_primary_key_with_a_rowid_fallback() {
        let mut r = row("codex", SEMANTICS_LEGACY, 10, 0, 0);
        assert_eq!(dedupe_key_of(&r), "ccswitch#req-1");
        r.request_id = "  ".into();
        assert_eq!(dedupe_key_of(&r), "ccswitch#row1");
    }

    #[test]
    fn seconds_are_promoted_and_the_vendor_money_never_reaches_the_event() {
        let mut r = row("codex", SEMANTICS_LEGACY, 100, 0, 0);
        assert_eq!(ts_of(&r), Some(1_790_165_270_000));
        r.created_at = 1_790_165_270_000;
        assert_eq!(ts_of(&r), Some(1_790_165_270_000), "an ms build is not multiplied again");
        r.created_at = 0;
        assert_eq!(ts_of(&r), None);
        r.created_at = 1_790_165_270;
        r.vendor_cost_usd = 4.2;
        r.cost_multiplier = "3".into();
        let ev = build_event(&r, "key").unwrap();
        assert_eq!(ev.counts.credits, 0.0, "money is computed centrally, never imported");
        assert_eq!(ev.tool, "ccswitch", "the gateway is its own row in the panel");
        assert_eq!(ev.meter, Meter::Tokens);
        assert!(ev.quota.is_none());
        assert!(ev.calls.is_empty());
        assert_eq!(ev.source, "key#proxy_request_logs#1");
    }

    #[test]
    fn an_unbilled_row_produces_nothing() {
        // The vendor's own "no usage at all" test (usage_stats.rs:1882-1886).
        let mut r = row("codex", SEMANTICS_LEGACY, 0, 0, 0);
        r.output = 0;
        r.status_code = 429;
        assert!(build_event(&r, "key").is_none());
        let r = Row { input: -5, output: -2, ..row("codex", SEMANTICS_LEGACY, 0, 0, 0) };
        assert!(build_event(&r, "key").is_none(), "negative garbage is not billable either");
    }
}
