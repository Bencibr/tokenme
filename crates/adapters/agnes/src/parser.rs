//! One `usage_ledger` row (or, on the fallback path, one `llm/**.jsonl` usage
//! record) → one [`UsageEvent`].
//!
//! ## Stage mapping: `input_tokens` already contains the cached prefix
//! Proved against the vendor's own arithmetic on the live 554 rows:
//!
//! | check | rows holding |
//! |---|---|
//! | `input_tokens + output_tokens = total_tokens` | **554 / 554** |
//! | `cache_read_tokens <= input_tokens` | **554 / 554** (0 crossings) |
//! | `(input_tokens − cache_read − cache_write) + output = total_tokens` | 5 / 554, i.e. exactly the 5 rows where `cache_read_tokens IS NULL` |
//!
//! So `input_tokens` is a *total* prompt (OpenAI `prompt_tokens` shape), not the
//! Anthropic-shaped uncached remainder, and the cached part is a subset of it:
//! Σ `input_tokens` 95,618,331, of which Σ `cache_read_tokens` 83,684,352 (87.5%)
//! came out of the cache, plus Σ `output_tokens` 568,292 = Σ `total_tokens`
//! 96,186,623. [`TokenCounts`] stages are mutually exclusive, so the cached prefix
//! is netted back out by [`net_input`] and `total()` then reproduces the vendor's
//! own `total_tokens` row for row — taking `input_tokens` verbatim would bill 83.7 M
//! tokens, 87% of this machine's recorded history, twice.
//!
//! The same identity holds on the fallback tree, which is the same payload before
//! normalisation: `{"input_tokens": 23607, "output_tokens": 844,
//! "total_tokens": 24451, "cache_read_input_tokens": 11776,
//! "cache_write_input_tokens": null}` → 23,607 + 844 = 24,451. Both readers net the
//! same way; [`stages`] is the one place it happens.
//!
//! `cache_write_tokens` is NULL in all 554 rows, so `cache_creation` is 0 here — but
//! it is subtracted from `input_tokens` too, because a total prompt column would
//! have to hold a written cache the same way it holds a read one.
//!
//! ## `is_compaction`: imported, because AgnesCode bills it
//! 553 rows carry `is_compaction = 0` and **1** row carries `1` (id 86, session
//! `20260806_1`, 96,430 input + 2,556 output = 98,986 total, `cache_read_tokens`
//! NULL). It is a real model call and the vendor says so three times over: it
//! answers with its own model id (`agnes-2.0-flash`, where the other 553 rows are
//! `agnes-2.5-flash`), it obeys the same `input + output = total` identity, and its
//! tokens are inside `sessions.accumulated_input_tokens` — 92,232,918 for that
//! session, which is the Σ of *all* 487 of its ledger rows including row 86 (the
//! equality holds for all 4 sessions, to the token). A compaction summary is a paid
//! prompt, so filtering it would under-report. [`crate::Census::compaction_rows`]
//! counts them so a smoke run can see the mix.
//!
//! ## Money: never imported
//! `cost` and `cost_source` are NULL in **554 / 554** rows: the ledger keeps the
//! columns and fills in nothing. They are read into the census to prove that a
//! non-NULL value still changes nothing, and never onto an event — tokenme prices
//! every stage centrally from the shared models.dev table.
//!
//! ## Ids
//! `usage_ledger.id` is `INTEGER PRIMARY KEY AUTOINCREMENT`, i.e. a stable per-call
//! id that *is* the rowid (measured: ids 1..=554 contiguous, and
//! `sqlite_sequence.usage_ledger = 554` — `AUTOINCREMENT` never reuses a retired id,
//! so a deleted session's rows cannot come back as someone else's). Hence
//! `dedupes_by_id: true` with the key `<session>#<id>`, no invented hash. The
//! fallback uses `<session>#<request_id>` instead (a 32-hex id from the log's own
//! `meta` record), which is precisely why only one of the two trees may ever be read.

use std::path::Path;

use usage_core::{Meter, TokenCounts, UsageEvent};

use crate::paths::{self, LogIdentity};
use crate::TOOL_ID;

/// A ledger row as this adapter reads it. Every column but `id` is nullable in the
/// vendor's `CREATE TABLE`, so every field here is an `Option`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Row {
    /// `id`, which is the rowid.
    pub id: i64,
    pub session_id: Option<String>,
    pub created_timestamp: Option<i64>,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    /// Money, census only. NULL in every row measured.
    pub cost: Option<f64>,
    pub cost_source: Option<String>,
    pub is_compaction: Option<i64>,
    /// `sessions.working_dir` via a `LEFT JOIN`; `None` when the row outlived its
    /// session or the table is missing.
    pub working_dir: Option<String>,
}

/// What a row says about the conventions this module assumes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowFlags {
    /// Every stage 0, so the row bills nothing.
    pub zero: bool,
    /// `input_tokens < cache_read + cache_write`: the inclusive convention that
    /// [`net_input`] rests on broke down (0 of 554 rows measured).
    pub inclusive_violation: bool,
    /// The netted stages do not reproduce the vendor's `total_tokens` (0 rows).
    pub total_mismatch: bool,
    /// `is_compaction` set.
    pub compaction: bool,
    /// `cost IS NOT NULL`, which must still not reach the event.
    pub cost_present: bool,
}

/// Uncached prompt tokens plus whether the guard held, i.e. the row's
/// `input_tokens` minus the part of it that a cache read or write already paid for.
pub(crate) fn net_input(row: &Row) -> (i64, bool) {
    let (read, write, input) = (
        row.cache_read_tokens.unwrap_or(0).max(0),
        row.cache_write_tokens.unwrap_or(0).max(0),
        row.input_tokens.unwrap_or(0).max(0),
    );
    if input < read + write {
        // A row this convention cannot explain: report the column untouched rather
        // than invent a negative prompt, and let the census count it.
        return (input, true);
    }
    (input - read - write, false)
}

/// The four mutually exclusive stages of a row.
pub(crate) fn stages(row: &Row) -> TokenCounts {
    let (input, _) = net_input(row);
    TokenCounts {
        input: input.max(0) as f64,
        cache_creation: row.cache_write_tokens.unwrap_or(0).max(0) as f64,
        cache_read: row.cache_read_tokens.unwrap_or(0).max(0) as f64,
        output: row.output_tokens.unwrap_or(0).max(0) as f64,
        // The ledger keeps no reasoning breakdown and `total_tokens` is exactly
        // input + output, so there is nothing to report here.
        reasoning: 0.0,
        credits: 0.0,
    }
}

/// A row's flags, including whether it bills at all.
pub(crate) fn flags(row: &Row, counts: &TokenCounts) -> RowFlags {
    let (_, violation) = net_input(row);
    RowFlags {
        zero: counts.is_zero(),
        inclusive_violation: violation,
        // `total_tokens` is nullable, and a NULL contradicts nothing.
        total_mismatch: row.total_tokens.is_some_and(|t| t.max(0) as f64 != counts.total()),
        compaction: row.is_compaction.unwrap_or(0) != 0,
        cost_present: row.cost.is_some(),
    }
}

/// One ledger row → one event, or `None` when the row bills nothing.
pub(crate) fn event_from_row(row: &Row, source_key: &str) -> Option<UsageEvent> {
    let counts = stages(row);
    let fallback = format!("agnes-row{}", row.id);
    let session = row.session_id.as_deref().filter(|s| !s.is_empty()).unwrap_or(&fallback);
    build_event(Call {
        session,
        // `id` is the ledger's own per-call id, so a row always has one.
        dedupe_key: Some(format!("{session}#{}", row.id)),
        ts_ms: seconds_to_ms(row.created_timestamp),
        model: row.model.as_deref().filter(|m| !m.is_empty()),
        project: basename(&row.working_dir),
        counts: &counts,
        source_key,
    })
}

/// One billed call, as both readers see it.
pub(crate) struct Call<'a> {
    pub session: &'a str,
    pub dedupe_key: Option<String>,
    /// `None`, or a non-positive time, and the call cannot be placed on a day.
    pub ts_ms: Option<i64>,
    pub model: Option<&'a str>,
    pub project: Option<String>,
    pub counts: &'a TokenCounts,
    pub source_key: &'a str,
}

/// The single constructor both readers go through, so the zero-usage gate, the
/// meter and the money rule cannot drift apart.
pub(crate) fn build_event(call: Call<'_>) -> Option<UsageEvent> {
    let Call { session, dedupe_key, ts_ms, model, project, counts, source_key } = call;
    let ts_ms = ts_ms.filter(|t| *t > 0);
    if counts.is_zero() || ts_ms.is_none() {
        // A call that reported no tokens is not billable, and a row without a
        // timestamp cannot be placed on a day at all.
        return None;
    }
    let mut event = UsageEvent::new(TOOL_ID, ts_ms?, session).with(*counts);
    event.meter = Meter::Tokens;
    event.model = model.map(str::to_string);
    event.project = project;
    event.dedupe_key = dedupe_key;
    event.source = source_key.to_string();
    Some(event)
}

/// `created_timestamp` is bare unix **seconds** (measured range
/// 1,786,011,974 … 1,788,349,044), promoted to ms with the same <1e10 cutoff
/// [`usage_core::parse_ts_ms`] uses, so a build that starts writing ms is read
/// correctly too.
pub(crate) fn seconds_to_ms(value: Option<i64>) -> Option<i64> {
    let v = value.filter(|v| *v > 0)?;
    Some(if v < 10_000_000_000 { v * 1000 } else { v })
}

/// Project label: the last component of the session's `working_dir`, which is what
/// every other adapter reports. AgnesCode runs throwaway sessions inside
/// `~/.agnes/temporary/<date>/<session>/work`, so those all label `work` and the
/// session id on the event is what separates them.
fn basename(path: &Option<String>) -> Option<String> {
    Some(Path::new(path.as_deref()?).file_name()?.to_str()?.to_string())
}

// ---------------------------------------------------------------------------
// Fallback: `state/logs/llm/<session>/<ms>-<request>-<purpose>.jsonl`
// ---------------------------------------------------------------------------

/// The per-file identity, accumulated from the head of a log so that the single
/// `usage` record near its end can be attributed.
#[derive(Debug, Default, Clone)]
pub(crate) struct Head {
    pub session: Option<String>,
    pub request_id: Option<String>,
    pub ts_ms: Option<i64>,
    pub model: Option<String>,
}

impl Head {
    /// Seed from the file name, which carries the session directory, the request
    /// start in ms and an 8-hex prefix of the request id (plus its purpose, which
    /// keeps the synthesised id unique across the two files of one millisecond). A
    /// later `meta` record still wins, because it holds the full 32-hex id.
    pub(crate) fn from_path(path: &Path) -> Self {
        match paths::log_identity(path) {
            Some(LogIdentity { session, ts_ms, request_prefix, purpose }) => Head {
                session: (!session.is_empty()).then_some(session),
                request_id: Some(format!("{request_prefix}-{purpose}")),
                ts_ms: Some(ts_ms),
                model: None,
            },
            None => Self::default(),
        }
    }
}

/// Fold one line of a fallback log into `head`, returning an event when the line
/// itself carried the usage object.
///
/// `None` covers every other record shape — the `meta` and `model_config` heads, the
/// 23,626 streamed `{"usage": null}` chunks, a malformed or torn line — plus a usage
/// record that reported no tokens or arrived without a timestamp.
pub(crate) fn fold_line(line: &str, head: &mut Head, source_key: &str) -> Option<UsageEvent> {
    // 23,626 of the 23,880 real lines are the streaming `{"data": …, "usage": null}`
    // shape, so they are dropped on a byte scan before anything is parsed.
    if !line.contains("input_tokens") && !line.contains("\"meta\"") && !line.contains("model_config") {
        return None;
    }
    let Ok(rec) = serde_json::from_str::<Record>(line) else {
        return None;
    };
    if let Some(meta) = rec.meta {
        if !meta.session_id.is_empty() {
            head.session = Some(meta.session_id);
        }
        head.request_id = meta.request_id.or_else(|| head.request_id.take());
        head.ts_ms = meta.timestamp_ms.or(head.ts_ms);
        return None;
    }
    if let Some(config) = rec.model_config {
        if config.model_name.is_some() {
            head.model = config.model_name;
        }
        return None;
    }
    // Only a top-level `usage` object bills: an echoed prompt inside `data` never
    // reaches this field.
    let usage = rec.usage?;
    let row = Row {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cache_read_tokens: usage.cache_read_input_tokens,
        cache_write_tokens: usage.cache_write_input_tokens,
        ..Default::default()
    };
    let counts = stages(&row);
    let session = head.session.clone().unwrap_or_else(|| "agnes-unknown".to_string());
    let key = head.request_id.clone().map(|rid| format!("{session}#{rid}"));
    build_event(Call {
        session: &session,
        dedupe_key: key,
        ts_ms: head.ts_ms,
        model: head.model.as_deref(),
        project: None,
        counts: &counts,
        source_key,
    })
}

#[derive(Debug, serde::Deserialize)]
struct Record {
    #[serde(default)]
    meta: Option<Meta>,
    #[serde(default)]
    model_config: Option<ModelConfig>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// `{"meta": {…}}` — one per file, its first line.
#[derive(Debug, serde::Deserialize)]
struct Meta {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    timestamp_ms: Option<i64>,
}

/// `{"model_config": {…}}` — one per file, its second line.
#[derive(Debug, serde::Deserialize)]
struct ModelConfig {
    #[serde(default)]
    model_name: Option<String>,
}

/// The provider's usage object, same inclusive convention as the ledger.
#[derive(Debug, Default, serde::Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    total_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    cache_write_input_tokens: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(input: Option<i64>, read: Option<i64>, write: Option<i64>, output: Option<i64>, total: Option<i64>) -> Row {
        Row {
            id: 1,
            session_id: Some("20260902_1".to_string()),
            created_timestamp: Some(1_788_349_044),
            model: Some("agnes-2.5-flash".to_string()),
            input_tokens: input,
            output_tokens: output,
            total_tokens: total,
            cache_read_tokens: read,
            cache_write_tokens: write,
            cost: None,
            cost_source: None,
            is_compaction: Some(0),
            working_dir: Some("/Users/dev/workspace/fucai".to_string()),
        }
    }

    #[test]
    fn the_cached_prefix_is_netted_out_of_a_total_prompt() {
        // The live ledger's first row: 17,402 prompt of which 17,152 came out of the
        // cache, 169 out, 17,571 total.
        let r = row(Some(17402), Some(17152), None, Some(169), Some(17571));
        let counts = stages(&r);
        assert_eq!(counts, TokenCounts { input: 250.0, cache_creation: 0.0, cache_read: 17152.0, output: 169.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(counts.total(), 17571.0, "= the row's own total_tokens");
        assert_eq!(flags(&r, &counts), RowFlags::default(), "a plain billable row raises no flag");
        // A write cache is netted the same way and then reported as its own stage.
        let r = row(Some(1000), Some(600), Some(100), Some(50), Some(1050));
        let counts = stages(&r);
        assert_eq!((counts.input, counts.cache_read, counts.cache_creation, counts.output), (300.0, 600.0, 100.0, 50.0));
        assert_eq!(counts.total(), 1050.0);
        assert!(!flags(&r, &counts).total_mismatch);
        // …and a row whose own total disagrees is flagged, not silently re-totalled.
        let r = row(Some(1000), Some(600), Some(100), Some(50), Some(450));
        assert!(flags(&r, &stages(&r)).total_mismatch, "450 is not 1_050");
    }

    #[test]
    fn a_row_the_convention_cannot_explain_is_reported_not_invented() {
        // cache_read above input: netting would go negative.
        let r = row(Some(100), Some(400), None, Some(20), Some(500));
        let counts = stages(&r);
        assert_eq!(counts.input, 100.0, "the column is reported verbatim instead of clamped away");
        assert_eq!(counts.cache_read, 400.0);
        let f = flags(&r, &counts);
        assert!(f.inclusive_violation && !f.zero);
    }

    #[test]
    fn null_everywhere_still_decodes_and_a_zero_row_never_bills() {
        let r = Row { id: 9, ..Default::default() };
        let counts = stages(&r);
        assert_eq!(counts, TokenCounts::default());
        assert!(flags(&r, &counts).zero);
        assert!(event_from_row(&r, "key").is_none());
        // The five real rows with `cache_read_tokens IS NULL` still bill normally.
        let r = row(Some(58464), None, None, Some(146), Some(58610));
        let e = event_from_row(&r, "key").unwrap();
        assert_eq!((e.counts.input, e.counts.cache_read, e.counts.output), (58464.0, 0.0, 146.0));
        assert_eq!(e.counts.total(), 58610.0);
    }

    #[test]
    fn money_is_never_attached_even_when_the_vendor_stores_it() {
        let mut r = row(Some(1000), Some(600), None, Some(40), Some(1040));
        assert_eq!(event_from_row(&r, "key").unwrap().counts.credits, 0.0);
        r.cost = Some(3.75);
        r.cost_source = Some("vendor".to_string());
        let e = event_from_row(&r, "key").unwrap();
        assert_eq!(e.counts.credits, 0.0, "tokenme prices centrally from models.dev");
        assert!(flags(&r, &stages(&r)).cost_present, "…but the census still sees that it happened");
    }

    #[test]
    fn row_shape_lands_on_the_event() {
        // The one compaction row of the live ledger, id 86.
        let r = Row {
            id: 86,
            session_id: Some("20260806_1".to_string()),
            created_timestamp: Some(1_786_017_623),
            model: Some("agnes-2.0-flash".to_string()),
            input_tokens: Some(96430),
            output_tokens: Some(2556),
            total_tokens: Some(98986),
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost: None,
            cost_source: None,
            is_compaction: Some(1),
            working_dir: Some("/Users/dev/workspace/fucai".to_string()),
        };
        let e = event_from_row(&r, "db").unwrap();
        assert_eq!(e.tool, "agnes");
        assert_eq!(e.meter, Meter::Tokens);
        assert_eq!(e.session, "20260806_1");
        assert_eq!(e.model.as_deref(), Some("agnes-2.0-flash"), "verbatim, compaction included");
        assert_eq!(e.project.as_deref(), Some("fucai"), "basename of sessions.working_dir");
        assert_eq!(e.dedupe_key.as_deref(), Some("20260806_1#86"), "the ledger's own id");
        assert_eq!(e.ts_ms, 1_786_017_623_000, "seconds promoted to ms");
        assert_eq!(e.source, "db");
        assert!(flags(&r, &e.counts).compaction);
        assert!(e.calls.is_empty() && e.quota.is_none(), "the ledger carries neither");
    }

    #[test]
    fn missing_identity_falls_back_to_the_row_and_bad_timestamps_drop() {
        let r = Row { id: 3, input_tokens: Some(10), output_tokens: Some(2), total_tokens: Some(12), ..Default::default() };
        assert!(event_from_row(&r, "db").is_none(), "no created_timestamp ⇒ no place on a day");
        let r = Row { created_timestamp: Some(0), ..r.clone() };
        assert!(event_from_row(&r, "db").is_none(), "epoch 0 is a placeholder, not a time");
        let r = Row { session_id: Some(String::new()), created_timestamp: Some(1), ..r };
        let e = event_from_row(&r, "db").unwrap();
        assert_eq!(e.session, "agnes-row3", "an empty session still has to be attributable");
        assert_eq!(e.dedupe_key.as_deref(), Some("agnes-row3#3"));
        assert_eq!(e.ts_ms, 1000);
        assert_eq!(e.model, None, "an absent model stays unknown");
        assert_eq!(e.project, None, "no working_dir ⇒ no label");
        // Milliseconds are already promoted and must not be promoted twice.
        let r = Row { created_timestamp: Some(1_788_349_044_123), ..r };
        assert_eq!(event_from_row(&r, "db").unwrap().ts_ms, 1_788_349_044_123);
    }

    #[test]
    fn a_fallback_usage_line_bills_like_the_ledger_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("20260902_1").join("1788348486108-07179738-main.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut head = Head::from_path(&path);
        assert_eq!(head.session.as_deref(), Some("20260902_1"), "seeded from the directory name");
        assert_eq!(head.ts_ms, Some(1_788_348_486_108));

        // The real lines of one log file, in order.
        let meta = "{\"meta\": {\"session_id\": \"20260902_1\", \"purpose\": \"main\", \"request_id\": \"07179738700a42b59c3bf3fcb46628df\", \"timestamp_ms\": 1788348486108}}";
        assert!(fold_line(meta, &mut head, "f").is_none(), "a meta record is identity, not a call");
        assert_eq!(head.request_id.as_deref(), Some("07179738700a42b59c3bf3fcb46628df"), "the full id replaces the name's 8-hex prefix");
        let config = "{\"input\": {\"messages\": []}, \"model_config\": {\"model_name\": \"agnes-2.5-flash\", \"context_limit\": 512000, \"reasoning\": false}}";
        assert!(fold_line(config, &mut head, "f").is_none());
        assert_eq!(head.model.as_deref(), Some("agnes-2.5-flash"));
        assert!(fold_line("{\"data\": null, \"usage\": null}", &mut head, "f").is_none(), "the 23,626 stream lines carry nothing");
        let usage = "{\"data\": null, \"usage\": {\"input_tokens\": 23607, \"output_tokens\": 844, \"total_tokens\": 24451, \"cache_read_input_tokens\": 11776, \"cache_write_input_tokens\": null}}";
        let e = fold_line(usage, &mut head, "f").expect("the usage line must bill");
        // 23,607 − 11,776 cached: exactly the ledger's convention.
        assert_eq!(e.counts, TokenCounts { input: 11831.0, cache_creation: 0.0, cache_read: 11776.0, output: 844.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(e.counts.total(), 24451.0, "the vendor's own total_tokens");
        assert_eq!(e.dedupe_key.as_deref(), Some("20260902_1#07179738700a42b59c3bf3fcb46628df"), "the log's request id, not a ledger id");
        assert_eq!((e.model.as_deref(), e.session.as_str(), e.ts_ms), (Some("agnes-2.5-flash"), "20260902_1", 1_788_348_486_108));
        assert_eq!(e.project, None, "this tree carries no working_dir");
        assert_eq!(e.source, "f");
    }

    #[test]
    fn a_torn_or_foreign_fallback_line_never_bills() {
        let mut head = Head::default();
        for line in ["", "{", "\u{1f600}", "{\"meta\": 3}", "[]", "{\"error\": \"aborted\"}", "{\"usage\": {\"input_tokens\": \"x\"}}"] {
            assert!(fold_line(line, &mut head, "f").is_none(), "{line}");
        }
        assert!(head.session.is_none() && head.model.is_none());
        assert!(fold_line("{\"usage\": null}", &mut head, "f").is_none());
        assert!(fold_line("{\"data\": {\"messages\": [{\"usage\": {\"input_tokens\": 5}}]}}", &mut head, "f").is_none(), "an echoed usage is not this call's");
        // No meta at all: the file-name seed is what attributes the call.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("20260806_1").join("1786108363815-59a7ccfa-tool_pair_summary.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut head = Head::from_path(&path);
        let e = fold_line("{\"usage\": {\"input_tokens\": 1469, \"output_tokens\": 135}}", &mut head, "f").expect("a usage record must bill");
        assert_eq!(e.session, "20260806_1");
        assert_eq!(e.dedupe_key.as_deref(), Some("20260806_1#59a7ccfa-tool_pair_summary"), "the request-id prefix and purpose the name does carry");
        assert_eq!(e.ts_ms, 1_786_108_363_815);
        assert_eq!(e.model, None, "no model_config line was read");
        // A log with neither a name we understand nor a meta record: the call billed
        // nowhere is the one we cannot date.
        let mut blank = Head::default();
        assert!(fold_line("{\"usage\": {\"input_tokens\": 3}}", &mut blank, "f").is_none(), "tokens without a timestamp drop out rather than landing on day 0");
        // Dated but anonymous: a shared placeholder session and no dedupe key.
        let mut dated = Head { ts_ms: Some(1_786_108_363_815), ..Default::default() };
        let e = fold_line("{\"usage\": {\"input_tokens\": 3}}", &mut dated, "f").expect("a dated record bills");
        assert_eq!(e.session, "agnes-unknown");
        assert_eq!(e.dedupe_key, None, "no request id anywhere ⇒ no dedupe key, never an invented one");
    }
}
