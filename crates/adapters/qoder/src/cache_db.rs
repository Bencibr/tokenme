//! The Qoder **IDE**'s own store — the second source, and the only place this
//! tool's real per-call tokens exist:
//! `<App Support>/Qoder{,CN}/SharedClientCache/cache/db/local.db`, table
//! `chat_message` ([`usage_core::FileKind::Sqlite`], cursor = highest consumed
//! `rowid`).
//!
//! The transcript tree this adapter already reads reports `credits` and zeroes in
//! every token field; this database is the mirror image — `token_info` is a JSON
//! object with actual numbers, and no credits at all. Both hang off the one tool
//! id, and the [`Meter`] a row gets is what keeps them apart.
//!
//! # Field mapping, cross-verified by four MIT-licensed implementations
//!
//! `token_info` = `{prompt_tokens, cached_tokens, completion_tokens,
//! max_input_tokens}`; `model_info` = `{model_key}`.
//!
//! * `xiufengsun/TokenTracker:src/lib/rollout.js:5733-5751` (`QODER_USAGE_SQL`) and
//!   `:5851-5881` (`normalizeQoderTokens`): `input = max(0, prompt - cached)`,
//!   `cachedInput = min(prompt, cached)`, `cache_creation = 0`; the comment at
//!   `:5868-5870` reads *"Qoder's prompt_tokens already includes cached_tokens …
//!   otherwise cached context is counted twice"*.
//! * `vibe-cafe/vibe-usage:src/parsers/qoder.js:20-22` states it in the field list
//!   itself (*"prompt_tokens INCLUDES cached_tokens"*), and `:295-310` implements
//!   `inputTokens = prompt - cached` with `cached = Math.min(prompt, cached)`,
//!   skipping a row when `prompt + completion === 0`.
//! * `Javis603/token-monitor:src/shared/providers/qodercn/usage.js:48-53`
//!   (`QODER_CN_USAGE_SQL`) and `:259-284` (`normalizeQoderCnDbRow`):
//!   `input: Math.max(0, prompt - cached)`, `cacheRead: Math.min(prompt, cached)`,
//!   `cacheWrite: 0`, null when `prompt + output === 0`.
//! * `cclank/tokei:usage.30s.py:7782-7788` sums the three fields per day into
//!   separate `in`/`cached` buckets and folds them back the same way: `:1088-1089`
//!   names `qoder_ide` among the tools whose *"`cached` is a subset of `in`"*, and
//!   `:1143-1145` computes `{"in": inp - cached, "cr": cached}`. Its header comment
//!   at `:34` says the same ("inputTokens 含 cached").
//!
//! So **`prompt_tokens` includes `cached_tokens`**, and this module emits
//! `input = prompt - cached` with `cached → cache_read`, which is the only shape
//! that does not bill the cached prefix twice. All four clamp rather than trust, so
//! a row whose `cached_tokens` exceeds its `prompt_tokens` — row 3 of
//! `Javis603/token-monitor:tests/fixtures/qoder-cn-local.db`, which the fixture
//! below reproduces — still nets to a sane event instead of going negative.
//!
//! **`max_input_tokens` is not a token stage and is dropped on purpose.** None of
//! the four reads it as usage; it appears in exactly one of them, in `vibe-cafe`'s
//! field list above, and only as part of the shape. On this machine the same key
//! also turns up in `chat_record.extra` as
//! `ideModelConfigOverride.max_input_tokens = 200000` next to
//! `reasoning_effort: "high"`: it is the configured context ceiling for the call, a
//! knob and not a count. Anything in `TokenCounts` gets multiplied by a USD rate
//! downstream, so a 200 k window would be billed as 200 k tokens.
//!
//! # `token_info` is not always valid JSON
//!
//! Measured on this machine's db: `SELECT sum(json_extract(token_info,
//! '$.prompt_tokens')) FROM chat_message` fails outright with
//! `Error: stepping, malformed JSON`, because the table also holds cells such as
//! `not-json` and the empty string. So nothing here extracts JSON in SQL — one bad
//! cell would abort the whole batch and park the cursor forever. Every cell is
//! parsed per row in Rust instead, and one that does not parse counts as
//! [`Census::invalid_json`] and is skipped, exactly like the JSONL reader skips a
//! torn line. The `role`, `IS NOT NULL` and `NOT IN ('', '{}')` guards the three
//! JavaScript implementations put in their `WHERE` clauses (`rollout.js:5745-5748`,
//! `usage.js:47-49`, `qoder.js:203-213`) are therefore *also* applied in Rust here,
//! where each rejection can be counted separately.
//!
//! # Timestamps
//!
//! `gmt_create` is unix **milliseconds** on this machine, not seconds: the single
//! row reads `1785201010881`, and the same message in the transcript tree is
//! stamped `2026-07-28T01:10:10.881329Z` for the same session — ms is the only
//! reading that reconciles them. `tokei` agrees (`usage.30s.py:7732`, *"gmt_create
//! (毫秒时间戳)"*, with every one of its queries written as
//! `gmt_create/1000, 'unixepoch'`). The column carries no type-affinity guarantee
//! though, and `Javis603/token-monitor:src/shared/providers/qodercn/usage.js:52-66`
//! normalises four shapes out of real files, so [`row_ts_ms`] does the same:
//! milliseconds, seconds, RFC 3339 text, and a text number.
//!
//! # One tool id, and where cross-source double counting stops
//!
//! `usage-index`'s dedupe index is global and unqualified, so the key has to be the
//! vendor's own identity for a call in **both** stores. Measured on this machine's
//! transcript tree (83 `*.jsonl`, 103 MB; the tree is live, so the totals drift by
//! a few hundred between passes): of the usage-bearing assistant records — 7 372 on
//! one pass, 7 605 on the next — **every single one carries a non-empty string
//! `message.usage.request_id`, and they are all distinct** (7 372/7 372 and
//! 7 605/7 605). No `request_id` repeats, none maps to more than one `message.id`,
//! no single file holds a repeat, and the live test in `tests/adapter.rs` fails
//! loudly if two events ever come back with one key. That is a per-call identity,
//! so [`crate::parser`] keys those events `qoder#<request_id>`.
//!
//! In *this* store `request_id` names a **request** (one user turn), not one call,
//! and the database says so three ways: `chat_record.request_id` is that table's
//! `PRIMARY KEY` (one row per turn, carrying the `question` / `answer` / `mode`
//! columns of it); `chat_message` indexes `request_id` twice over as a plain
//! non-unique index; and `cclank/tokei:usage.30s.py:7787-7788` reports
//! `COUNT(DISTINCT request_id)` as calls beside `COUNT(*)` as messages, and only
//! needs `GROUP BY request_id HAVING COUNT(*) > 1` at `:7841-7847` because several
//! messages do share one request. This machine's copy of the table holds **1 row**
//! — a `user` message with no `token_info` — so *how many* token-bearing assistant
//! rows one request carries here is not measurable; on a machine that uses the
//! IDE's chat/Quest path it is more than one.
//!
//! Which is why one row is one event here, keyed `qoder#<request_id>` whenever the
//! pass sees exactly one token-bearing row for that request — the shape that can
//! meet a transcript record of the same call and fold into one indexed event — and
//! `qoder#<request_id>#<chat_message.id>` when a request carries several, so the
//! second and later calls of a turn can never be folded away into the first. A call
//! present in both stores is then indexed once; a turn that is not present in both
//! is counted in full either way. `chat_message.id` cannot bridge the stores itself:
//! the one message that exists in both on this machine is `bec33c26-…` here and
//! `334b6be4-…` in the transcript, so only `request_id` is shared at all.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Row as SqlRow};
use serde_json::Value;
use usage_core::{
    parse_ts_ms, Error, Meter, ReadCursor, ReadOutcome, SourceFile, TokenCounts, UsageEvent,
};

use crate::paths;
use crate::{DEDUPE_PREFIX, TOOL_ID};

/// The table this source ingests. The rest of the 40-odd tables in this database
/// are context stores, not usage: a `pragma_table_info` sweep over every table here
/// surfaced only `chat_message.token_info`, `agent_memory.token_count` (which counts
/// embedding text) and `supabase_token.access_token` as name matches — the sweep
/// aborts on this file's `vec0` virtual tables, so treat that as suggestive rather
/// than exhaustive. `chat_record` and `chat_session` are read for attribution only.
pub(crate) const TABLE: &str = "chat_message";

/// Proves the table we ingest is queryable without scanning it. `EXISTS` always
/// answers with exactly one row, so an empty but valid database stays ours.
pub(crate) const USABLE_SQL: &str = "SELECT EXISTS(SELECT 1 FROM chat_message LIMIT 1)";

/// Rows per query. The table also holds every message body (`content`), so an
/// IDE-heavy machine can put six figures of rows here; paging keeps one pass flat.
pub(crate) const BATCH_ROWS: i64 = 2000;

/// Safety valve, so a database being rewritten under us cannot hold one pass open
/// forever: `BATCH_ROWS * MAX_BATCHES` rows, i.e. 400 k.
const MAX_BATCHES: usize = 200;

/// No `json_extract`, deliberately: see the module doc. `content`, `summary` and
/// `tool_result` are never selected — they are where the megabytes are, and no
/// event needs them.
const SELECT: &str = "SELECT rowid, id, session_id, request_id, role, token_info, model_info, \
     gmt_create FROM chat_message WHERE rowid > ?1 ORDER BY rowid LIMIT ?2";

/// The same questions, asked twice on purpose: `probe` runs [`stats`] for the hint
/// it shows at startup, and [`read`] fills these counters as it pages, so the hint
/// cannot drift from what the ingest path actually sees.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Census {
    /// Rows the cursor paged over, before any filter.
    pub rows: usize,
    /// `fetch_batch` calls it took — non-zero proof the batches were walked.
    pub batches: usize,
    /// Events emitted.
    pub events: usize,
    /// Rows whose `role` is not `assistant`: user prompts and tool results, which
    /// make up most of a chat table and carry no usage of their own.
    pub non_assistant: usize,
    /// Assistant rows with no `token_info` at all: `NULL`, `''` or `{}`.
    pub absent_token_info: usize,
    /// Assistant rows whose `token_info` is present but is not a JSON object.
    pub invalid_json: usize,
    /// Parsed rows with nothing on any stage: an aborted or usage-less call.
    pub zero_rows: usize,
    /// Rows folded into an earlier row of the same `chat_message.id`.
    pub merged_repeats: usize,
    /// `request_id`s that contributed more than one event, i.e. the turns whose
    /// keys had to carry a message id as well.
    pub multi_call_requests: usize,
    /// Rows with no `request_id` to key on, so the key fell back to the message.
    pub unnamed_rows: usize,
    /// Highest `rowid` visited, i.e. the new cursor.
    pub max_rowid: i64,
    pub totals: TokenCounts,
    /// `model_key` → events, so a smoke run can name the models behind the tokens.
    pub models: BTreeMap<String, usize>,
}

/// What this store holds, for `probe`'s hint. Every aggregate here is JSON-free, so
/// one malformed cell cannot make the census fail.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub rows: i64,
    /// Assistant rows carrying something that could be usage.
    pub usage_rows: i64,
    pub requests: i64,
    pub sessions: i64,
}

/// The SQL half of the guards, shared by [`stats`] and the test's recomputation.
const USAGE_ROWS: &str = "role = 'assistant' AND token_info IS NOT NULL \
     AND trim(CAST(token_info AS TEXT)) NOT IN ('', '{}')";

/// `count(*)`, the usage-bearing subset, and how many requests and sessions they
/// span — in one scan. `None` when the database is missing, locked, or not ours.
pub(crate) fn stats(path: &Path) -> Option<Stats> {
    let conn = paths::open_readonly(path, USABLE_SQL).ok()?;
    conn.query_row(
        &format!(
            "SELECT count(*), SUM(CASE WHEN {USAGE_ROWS} THEN 1 ELSE 0 END), \
             COUNT(DISTINCT request_id), COUNT(DISTINCT session_id) FROM {TABLE}"
        ),
        [],
        |r| {
            Ok(Stats {
                rows: r.get(0)?,
                usage_rows: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                requests: r.get(2)?,
                sessions: r.get(3)?,
            })
        },
    )
    .ok()
}

/// Read every `chat_message` row appended after `cursor`.
///
/// Deliberately infallible: Qoder owns this database and keeps it open while the
/// IDE runs, so a lock, a schema this build has not seen or a half-written journal
/// all mean "nothing this pass", never "abort the other sources". The cursor comes
/// back **unchanged** whenever the database cannot be opened or queried at all, so
/// a locked file costs a pass and not the ledger.
pub(crate) fn read(file: &SourceFile, cursor: ReadCursor) -> Result<(ReadOutcome, Census), Error> {
    read_with(file, cursor, BATCH_ROWS)
}

fn read_with(file: &SourceFile, cursor: ReadCursor, batch: i64) -> Result<(ReadOutcome, Census), Error> {
    let conn = match paths::open_readonly(&file.path, USABLE_SQL) {
        Ok(conn) => conn,
        Err(_) => return Ok((untouched(cursor), Census::default())),
    };
    let projects = session_projects(&conn);
    let mut census = Census::default();
    let mut seen: Vec<Bucket> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut here = i64::try_from(cursor.0).unwrap_or(i64::MAX);
    if let Some(max) = max_rowid(&conn) {
        // A row space that shrank underneath us — the IDE prunes this cache freely
        // — is business as usual, unlike a truncated JSONL log: clamp, and the next
        // append is picked up normally. No `Error::Cursor` from this source.
        here = here.min(max);
    }

    for _ in 0..MAX_BATCHES {
        let rows = match fetch_batch(&conn, here, batch) {
            Ok(rows) => rows,
            // Busy, locked, or a column this build has not added yet: keep what was
            // already decoded and park the cursor where we got to.
            Err(_) => break,
        };
        census.batches += 1;
        let short = (rows.len() as i64) < batch;
        for row in rows {
            census.rows += 1;
            here = here.max(row.rowid);
            take_row(row, &projects, file, &mut seen, &mut by_id, &mut census);
        }
        census.max_rowid = here;
        if short {
            break;
        }
    }

    let events = finish(seen, file.key().as_str(), &mut census);
    census.events = events.len();
    Ok((ReadOutcome { events, cursor: ReadCursor(u64::try_from(here).unwrap_or(u64::MAX)) }, census))
}

fn untouched(cursor: ReadCursor) -> ReadOutcome {
    ReadOutcome { events: Vec::new(), cursor }
}

/// One assistant row that claimed to carry usage, held until the pass ends so
/// repeats of a `chat_message.id` can merge and a request's row count can be known.
struct Bucket {
    /// `chat_message.id`, the table's primary key; `row<rowid>` when it is absent.
    message: String,
    request: String,
    session: String,
    project: Option<String>,
    model: Option<String>,
    ts_ms: i64,
    counts: TokenCounts,
    /// The stage sum, used to pick between repeats of one message.
    weight: f64,
}

impl Bucket {
    fn from_row(row: &CacheRow, counts: TokenCounts, project: Option<String>, fallback_ts: i64) -> Self {
        // A row with neither a session nor a request still needs a session string,
        // because `UsageEvent::session` is not optional.
        let session = if !row.session.is_empty() {
            row.session.clone()
        } else if !row.request.is_empty() {
            row.request.clone()
        } else {
            format!("qoder-cache-row{}", row.rowid)
        };
        Self {
            message: row.message.clone(),
            request: row.request.clone(),
            session,
            project,
            model: row.model.clone(),
            ts_ms: row.ts_ms.unwrap_or(fallback_ts),
            weight: counts.total(),
            counts,
        }
    }
}

/// The per-row filters the three JavaScript implementations put in their `WHERE`.
fn take_row(
    row: CacheRow,
    projects: &HashMap<String, String>,
    file: &SourceFile,
    seen: &mut Vec<Bucket>,
    by_id: &mut HashMap<String, usize>,
    census: &mut Census,
) {
    if row.role != "assistant" {
        census.non_assistant += 1;
        return;
    }
    let Some(raw) = row.token_info.as_deref().map(str::trim) else {
        census.absent_token_info += 1;
        return;
    };
    if raw.is_empty() || raw == "{}" {
        census.absent_token_info += 1;
        return;
    }
    // One unparseable cell is one skipped row. That substitution *is* the point of
    // parsing here rather than with `json_extract`.
    let Some(usage) = serde_json::from_str::<Value>(raw).ok().filter(Value::is_object) else {
        census.invalid_json += 1;
        return;
    };
    let counts = netted(&usage);
    if counts.is_zero() {
        census.zero_rows += 1;
        return;
    }
    let project = projects.get(&row.session).cloned();
    match by_id.get(&row.message) {
        Some(&slot) => {
            // The same message written twice (an updated row keeps its primary key,
            // and a moved one can be re-delivered inside a window): the call was
            // billed once, so keep the largest snapshot rather than summing it.
            census.merged_repeats += 1;
            let bucket = &mut seen[slot];
            if counts.total() > bucket.weight {
                bucket.counts = counts;
                bucket.weight = counts.total();
            }
            if bucket.model.is_none() {
                bucket.model = row.model.clone();
            }
            if bucket.project.is_none() {
                bucket.project = project;
            }
        }
        None => {
            seen.push(Bucket::from_row(&row, counts, project, file.mtime_ms));
            by_id.insert(row.message, seen.len() - 1);
        }
    }
}

/// Turn the merged rows into events, choosing the identity each one is indexed by.
fn finish(seen: Vec<Bucket>, source_key: &str, census: &mut Census) -> Vec<UsageEvent> {
    let mut per_request: HashMap<String, usize> = HashMap::new();
    for bucket in &seen {
        if !bucket.request.is_empty() {
            *per_request.entry(bucket.request.clone()).or_default() += 1;
        }
    }
    census.multi_call_requests = per_request.values().filter(|n| **n > 1).count();
    let mut events = Vec::with_capacity(seen.len());
    for bucket in seen {
        let mut event = UsageEvent::new(TOOL_ID, bucket.ts_ms, bucket.session);
        event.project = bucket.project;
        event.model = bucket.model.clone();
        event.counts = bucket.counts;
        // Per call, with the model on the same row: this is the source that can be
        // priced the normal way, unlike the credit-metered transcript tree.
        event.meter = Meter::Tokens;
        event.dedupe_key = Some(if bucket.request.is_empty() {
            census.unnamed_rows += 1;
            format!("{DEDUPE_PREFIX}{}", bucket.message)
        } else if per_request.get(bucket.request.as_str()).copied().unwrap_or(0) > 1 {
            format!("{DEDUPE_PREFIX}{}#{}", bucket.request, bucket.message)
        } else {
            format!("{DEDUPE_PREFIX}{}", bucket.request)
        });
        event.source = source_key.to_string();
        *census.models.entry(bucket.model.unwrap_or_else(|| "?".into())).or_default() += 1;
        census.totals += &bucket.counts;
        events.push(event);
    }
    events
}

/// `prompt_tokens` is cache-**inclusive** (module doc), so the cached prefix comes
/// off it into its own stage and `total()` never bills it twice. `cache_creation`
/// stays 0 — Qoder writes no cache-write stage — and `max_input_tokens` stays out of
/// here entirely.
fn netted(usage: &Value) -> TokenCounts {
    let prompt = count(usage.get("prompt_tokens"));
    let cached = count(usage.get("cached_tokens")).min(prompt);
    TokenCounts {
        input: (prompt - cached).max(0.0),
        cache_creation: 0.0,
        cache_read: cached,
        output: count(usage.get("completion_tokens")),
        reasoning: 0.0,
        credits: 0.0,
    }
}

/// Absent, negative, stringified or non-finite is 0 — never a negative stage, and
/// never a reason to throw the row away.
fn count(value: Option<&Value>) -> f64 {
    value
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(0.0)
}

/// `model_info.model_key`, verbatim. The tiers (`auto`, `quest-ultimate`) and the
/// internal aliases (`qfmodel`, `qmodel_38max`) both stay as the vendor wrote them,
/// exactly as on the transcript side: renaming one would attach a price Qoder never
/// charged. Broken or absent `model_info` leaves the model unknown, which is the
/// honest state and keeps it off the priced path.
fn model_key(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let key = value.get("model_key").and_then(Value::as_str)?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// `gmt_create` in whichever of its four shapes the row happens to hold (module doc).
fn row_ts_ms(raw: SqlValue) -> Option<i64> {
    let promote = |n: f64| {
        (n.is_finite() && n > 0.0).then_some(if n < 1e12 { (n * 1000.0) as i64 } else { n as i64 })
    };
    match raw {
        SqlValue::Integer(n) => promote(n as f64),
        SqlValue::Real(f) => promote(f),
        SqlValue::Text(s) => parse_ts_ms(&s),
        SqlValue::Null | SqlValue::Blob(_) => None,
    }
}

/// `session_id` → workspace path, from `chat_session.project_uri`.
///
/// A best-effort second query, never joined into [`SELECT`]: older CN builds have no
/// `chat_session` table at all (`vibe-cafe/vibe-usage:src/parsers/qoder.js:225-227`
/// degrades the same way), and a `JOIN` would then cost the usage rows with it.
/// `project_uri` is the same string the transcript records as `cwd` — measured:
/// `/Users/dev/Documents/Qoder/2026-07-28/chat-1` appears as both — so the two
/// sources label one workspace identically.
fn session_projects(conn: &Connection) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(mut stmt) = conn.prepare_cached(
        "SELECT session_id, COALESCE(NULLIF(project_uri, ''), NULLIF(project_name, '')) \
         FROM chat_session WHERE session_id IS NOT NULL",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
    }) else {
        return out;
    };
    for row in rows.flatten() {
        if let (Some(id), Some(project)) = row {
            out.insert(id, project);
        }
    }
    out
}

/// One row of [`SELECT`], owned so the statement can close before any parsing.
struct CacheRow {
    rowid: i64,
    /// `chat_message.id`, or `row<rowid>` when the primary key is missing.
    message: String,
    session: String,
    request: String,
    role: String,
    token_info: Option<String>,
    /// Already resolved out of `model_info`, so a row that arrives twice does not
    /// parse the same JSON twice.
    model: Option<String>,
    ts_ms: Option<i64>,
}

fn fetch_batch(conn: &Connection, after: i64, limit: i64) -> rusqlite::Result<Vec<CacheRow>> {
    let mut stmt = conn.prepare_cached(SELECT)?;
    let rows = stmt.query_map(rusqlite::params![after, limit], take_cell)?;
    rows.collect()
}

/// `Option<>` and `unwrap_or` on every column even where the vendor declares them
/// `NOT NULL`: a restored or half-migrated copy can hand back anything, and one
/// unexpected value must not abort the ingest pass for every other source.
fn take_cell(row: &SqlRow<'_>) -> rusqlite::Result<CacheRow> {
    let text = |i: usize| {
        row.get::<_, Option<String>>(i)
            .unwrap_or(None)
            .filter(|s| !s.is_empty())
    };
    let rowid = row.get::<_, Option<i64>>(0)?.unwrap_or(0);
    Ok(CacheRow {
        rowid,
        message: text(1).unwrap_or_else(|| format!("row{rowid}")),
        session: text(2).unwrap_or_default(),
        request: text(3).unwrap_or_default(),
        role: text(4).unwrap_or_default(),
        token_info: text(5),
        model: text(6).as_deref().and_then(model_key),
        ts_ms: row.get::<_, Option<SqlValue>>(7).ok().flatten().and_then(row_ts_ms),
    })
}

/// `max(rowid)` is a btree seek, not a scan.
fn max_rowid(conn: &Connection) -> Option<i64> {
    conn.query_row(&format!("SELECT max(rowid) FROM {TABLE}"), [], |r| r.get::<_, Option<i64>>(0))
        .ok()
        .flatten()
}

#[cfg(test)]
pub(crate) mod tests {
    use rusqlite::Connection;

    use super::*;

    /// The vendor's own DDL, copied out of this machine's `local.db` with
    /// `.schema chat_message`, column order included. `id` is the primary key and
    /// is *not* an `INTEGER PRIMARY KEY`, so the rows keep an ordinary rowid — which
    /// is what [`SELECT`] pages over.
    const DDL: &str = "CREATE TABLE chat_message (
            id varchar(64) primary key,
            session_id VARCHAR(64),
            request_id VARCHAR(64),
            role       VARCHAR(64),
            content text,
            summary text,
            summary_modified INTEGER,
            summary_trigger INTEGER DEFAULT 0,
            tool_result text,
            token_info text,
            model_info text,
            extra text DEFAULT '',
            gmt_create INTEGER
        );
        CREATE INDEX message_id_session_id ON chat_message (session_id, request_id);
        CREATE INDEX message_id_request_id ON chat_message (request_id);";

    /// `chat_session` is optional in the wild, so the fixture has it to exercise the
    /// workspace attribution rather than assume it away.
    const SESSION_DDL: &str = "CREATE TABLE chat_session (
            session_id varchar(64) primary key,
            project_uri VARCHAR(256),
            project_name VARCHAR(128)
        );";

    /// One row shaped like the vendor writes it. `content` is filled because the
    /// real table always has a body there: it must never reach an event.
    #[derive(Debug, Clone)]
    pub(crate) struct Seed {
        pub id: &'static str,
        pub session: &'static str,
        pub request: &'static str,
        pub role: &'static str,
        pub token_info: Option<&'static str>,
        pub model_info: &'static str,
        pub gmt_create: Option<i64>,
    }

    impl Seed {
        fn call(id: &'static str, request: &'static str, token_info: &'static str) -> Self {
            Self {
                id,
                session: "sess-1",
                request,
                role: "assistant",
                token_info: Some(token_info),
                model_info: r#"{"model_key":"qmodel_38max"}"#,
                gmt_create: Some(1_785_201_010_881),
            }
        }
    }

    fn insert(conn: &Connection, s: &Seed) {
        conn.execute(
            "INSERT INTO chat_message (id, session_id, request_id, role, content, summary, \
             summary_modified, summary_trigger, tool_result, token_info, model_info, extra, \
             gmt_create) VALUES (?1,?2,?3,?4,?5,'',0,0,'',?6,?7,'',?8)",
            rusqlite::params![
                s.id,
                s.session,
                s.request,
                s.role,
                "a message body the reader must never look at",
                s.token_info,
                s.model_info,
                s.gmt_create
            ],
        )
        .unwrap_or_else(|e| panic!("seeding {} (request {}): {e}", s.id, s.request));
    }

    /// Every shape the live table and `Javis603`'s fixture both contain: a normal
    /// call, an inclusive row whose cached exceeds its prompt, garbage, `{}`, NULL,
    /// a `user` row, a repeated `request_id`, seconds instead of milliseconds, and
    /// the two ways a key can fall back.
    pub(crate) fn seeds() -> Vec<Seed> {
        vec![
            // 1 the ordinary case: 58 299 prompt of which 57 853 were a cache hit,
            //   plus the 200 000 that must never be read as a stage.
            Seed::call("msg-1", "req-1", r#"{"prompt_tokens":58299,"cached_tokens":57853,"completion_tokens":2812,"max_input_tokens":200000}"#),
            // 2 cached beyond the prompt: the shape that would go negative.
            Seed { model_info: r#"{"model_key":"auto"}"#, ..Seed::call("msg-2", "req-2", r#"{"prompt_tokens":10,"cached_tokens":20,"completion_tokens":5}"#) },
            // 3 not JSON at all. It sits *before* rows that must still be read.
            Seed::call("msg-3", "req-3", "not-json"),
            // 4 valid JSON, no usage in it.
            Seed::call("msg-4", "req-4", "{}"),
            // 5 SQL NULL.
            Seed { token_info: None, ..Seed::call("msg-5", "req-5", "") },
            // 6 the row that is the only one on this machine today: a user prompt.
            Seed {
                id: "msg-6",
                request: "req-6",
                role: "user",
                token_info: None,
                model_info: "",
                ..Seed::call("ignored", "ignored", "")
            },
            // 7/8 one request, two calls: the turn shape this store really has, and
            //   the reason a `request_id` alone cannot be the key for both rows.
            Seed::call("msg-7", "req-7", r#"{"prompt_tokens":900,"cached_tokens":100,"completion_tokens":30}"#),
            Seed::call("msg-8", "req-7", r#"{"prompt_tokens":1200,"cached_tokens":1000,"completion_tokens":40}"#),
            // 9 unix seconds, the other dialect `usage.js:52-66` normalises.
            Seed { gmt_create: Some(1_785_201_010), ..Seed::call("msg-10", "req-9", r#"{"prompt_tokens":20,"cached_tokens":5,"completion_tokens":2}"#) },
            // 10 no request_id and no session_id: both keys have to fall back.
            Seed { session: "", request: "", ..Seed::call("msg-11", "", r#"{"prompt_tokens":7,"cached_tokens":0,"completion_tokens":1}"#) },
            // 11 a `model_info` that is not JSON: the call is still real.
            Seed { model_info: "broken-json", ..Seed::call("msg-12", "req-10", r#"{"prompt_tokens":3,"cached_tokens":3,"completion_tokens":1}"#) },
            // 12 no timestamp at all: filed under the file's mtime, not epoch 0.
            Seed { gmt_create: None, ..Seed::call("msg-13", "req-11", r#"{"prompt_tokens":4,"cached_tokens":0,"completion_tokens":1}"#) },
            // 13 an aborted call: every stage 0.
            Seed::call("msg-14", "req-12", r#"{"prompt_tokens":0,"cached_tokens":0,"completion_tokens":0}"#),
        ]
    }

    /// Writes the fixture and hands back the [`SourceFile`] for it. `mtime_ms` is
    /// pinned rather than read, because one event's timestamp falls back to it.
    pub(crate) fn fixture(dir: &Path) -> SourceFile {
        let path = dir.join("local.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute_batch(SESSION_DDL).unwrap();
        conn.execute(
            "INSERT INTO chat_session VALUES ('sess-1', '/Users/demo/proj', 'proj')",
            [],
        )
        .unwrap();
        for s in seeds() {
            insert(&conn, &s);
        }
        drop(conn);
        let (size, _) = paths::stat_file(&path).unwrap();
        SourceFile {
            path,
            kind: usage_core::FileKind::Sqlite,
            size,
            mtime_ms: 1_785_201_010_881,
        }
    }

    fn read_all(file: &SourceFile, cursor: ReadCursor) -> (Vec<UsageEvent>, Census) {
        let (outcome, census) = read_with(file, cursor, BATCH_ROWS).unwrap();
        (outcome.events, census)
    }

    fn keys(events: &[UsageEvent]) -> Vec<String> {
        events.iter().filter_map(|e| e.dedupe_key.clone()).collect()
    }

    fn by_key<'a>(events: &'a [UsageEvent], key: &str) -> &'a UsageEvent {
        events
            .iter()
            .find(|e| e.dedupe_key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("no event for {key}: {:?}", keys(events)))
    }

    fn source_at(path: &Path) -> SourceFile {
        let (size, mtime_ms) = paths::stat_file(path).unwrap_or((0, 1));
        SourceFile { path: path.to_path_buf(), kind: usage_core::FileKind::Sqlite, size, mtime_ms }
    }

    #[test]
    fn one_row_is_one_call_and_cached_comes_off_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let (events, census) = read_all(&file, ReadCursor(0));
        // 8 of the 13 rows are billable calls. msg-3 (garbage), msg-4/5 (no
        // token_info), msg-6 (a user row) and msg-14 (all zero) are not.
        assert_eq!(census.rows, 13);
        assert_eq!(census.events, 8, "{:?}", keys(&events));
        assert_eq!(census.invalid_json, 1, "an unparseable cell is counted, not fatal");
        assert_eq!(census.absent_token_info, 2);
        assert_eq!(census.non_assistant, 1);
        assert_eq!(census.zero_rows, 1);
        assert_eq!(census.multi_call_requests, 1, "req-7 is the turn with two calls");
        assert_eq!(census.unnamed_rows, 1);
        assert_eq!(census.max_rowid, 13, "highest consumed rowid");

        let first = by_key(&events, "qoder#req-1");
        assert_eq!(first.counts.input, 58_299.0 - 57_853.0, "prompt is cache-INCLUSIVE");
        assert_eq!(first.counts.cache_read, 57_853.0);
        assert_eq!(first.counts.cache_creation, 0.0);
        assert_eq!(first.counts.output, 2_812.0);
        assert_eq!(first.counts.reasoning, 0.0);
        assert_eq!(first.counts.credits, 0.0, "this store bills no credits");
        assert_eq!(first.counts.total(), 58_299.0 + 2_812.0, "the cached prefix is billed once");
        assert_eq!(first.meter, Meter::Tokens);
        assert_eq!(first.model.as_deref(), Some("qmodel_38max"), "model_key, verbatim");
        assert_eq!(first.session, "sess-1");
        assert_eq!(first.project.as_deref(), Some("/Users/demo/proj"), "from chat_session");
        assert_eq!(first.ts_ms, 1_785_201_010_881, "gmt_create is already milliseconds");
        assert_eq!(first.tool, TOOL_ID);
        assert_eq!(first.source, file.key());

        // cached beyond the prompt clamps to the prompt instead of going negative.
        let over = by_key(&events, "qoder#req-2");
        assert_eq!(over.counts.input, 0.0);
        assert_eq!(over.counts.cache_read, 10.0);
        assert_eq!(over.counts.total(), 15.0);
        assert_eq!(over.model.as_deref(), Some("auto"), "a routing tier stays a tier");

        // Seconds and a missing timestamp both land somewhere sane.
        assert_eq!(by_key(&events, "qoder#req-9").ts_ms, 1_785_201_010_000);
        assert_eq!(by_key(&events, "qoder#req-11").ts_ms, file.mtime_ms);
        // A `model_info` that is not JSON leaves the model unknown, not invented.
        assert_eq!(by_key(&events, "qoder#req-10").model, None);
        let orphan = by_key(&events, "qoder#msg-11");
        assert_eq!(orphan.session, "qoder-cache-row10", "no session and no request to name it");
        assert_eq!(orphan.project, None);

        // The four mutually exclusive stages, over exactly the emitted rows: the
        // 200 000 `max_input_tokens` is nowhere in here.
        assert_eq!(
            census.totals,
            TokenCounts {
                input: 1_472.0,
                cache_creation: 0.0,
                cache_read: 58_971.0,
                output: 2_892.0,
                reasoning: 0.0,
                credits: 0.0
            }
        );
        let mut totals = TokenCounts::default();
        for event in &events {
            assert_eq!(event.counts.credits, 0.0);
            assert!(event.counts.input <= event.counts.total(), "{event:?}");
            totals += &event.counts;
        }
        assert_eq!(totals, census.totals);
        assert_eq!(census.models["qmodel_38max"], 6);
        assert_eq!(census.models["auto"], 1);
        assert_eq!(census.models["?"], 1);
    }

    #[test]
    fn a_turn_with_several_calls_indexes_each_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let (events, _) = read_all(&file, ReadCursor(0));
        let one = by_key(&events, "qoder#req-7#msg-7");
        let two = by_key(&events, "qoder#req-7#msg-8");
        assert_eq!(one.counts.input, 800.0);
        assert_eq!(two.counts.input, 200.0);
        assert_eq!(one.counts.total() + two.counts.total(), 2_170.0, "both calls survive");
        assert!(
            !events.iter().any(|e| e.dedupe_key.as_deref() == Some("qoder#req-7")),
            "a shared request id is never used as a single-call key: {:?}",
            keys(&events)
        );
        let mut seen = std::collections::HashSet::new();
        for key in keys(&events) {
            assert!(seen.insert(key.clone()), "duplicate dedupe key {key}");
        }
    }

    #[test]
    fn garbage_and_absent_cells_degrade_without_aborting_the_pass() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        // The poison sits at rowid 3, ahead of six rows that must still arrive: a
        // pass that aborted on it would lose them all.
        let (events, census) = read_all(&file, ReadCursor(2));
        assert_eq!(census.rows, 11);
        assert_eq!(census.invalid_json, 1);
        assert_eq!(census.absent_token_info, 2);
        assert_eq!(events.len(), 6, "{:?}", keys(&events));
        assert_eq!(census.max_rowid, 13, "the pass still ran to the end of the table");
        // The vendor's own SQL cannot do this, which is the whole lesson.
        let conn = Connection::open(&file.path).unwrap();
        assert!(
            conn.query_row(
                "SELECT sum(json_extract(token_info,'$.prompt_tokens')) FROM chat_message",
                [],
                |r| r.get::<_, f64>(0)
            )
            .is_err(),
            "json_extract must abort on this table, so the reader never uses it"
        );
    }

    #[test]
    fn the_cursor_is_the_highest_rowid_and_resuming_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let (outcome, census) = read_with(&file, ReadCursor(0), 4).unwrap();
        assert_eq!(census.rows, 13, "all of it, in four-row batches");
        assert_eq!(census.batches, 4, "3 full batches then the short one that stops it");
        assert_eq!(outcome.cursor, ReadCursor(13), "highest consumed rowid, not a row count");
        assert_eq!(census.events, 8);
        for _ in 0..2 {
            let again = read(&file, outcome.cursor).unwrap();
            assert!(again.0.events.is_empty(), "{:?}", again.0.events);
            assert_eq!(again.0.cursor, outcome.cursor, "idempotent, and the cursor holds");
            assert_eq!(
                again.1,
                Census { batches: 1, max_rowid: 13, ..Census::default() },
                "one short batch that visited nothing: no rows, no totals, same watermark"
            );
        }
        // A row space that shrank underneath us clamps instead of erroring.
        let beyond = read(&file, ReadCursor(9_999)).unwrap();
        assert_eq!(beyond.0.cursor, ReadCursor(13));
        assert!(beyond.0.events.is_empty());
    }

    #[test]
    fn a_row_appended_after_the_cursor_is_the_only_new_event() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let first = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(keys(&first.0.events).len(), 8);
        let conn = Connection::open(&file.path).unwrap();
        insert(
            &conn,
            &Seed::call("msg-15", "req-13", r#"{"prompt_tokens":50,"cached_tokens":20,"completion_tokens":8}"#),
        );
        drop(conn);
        let next = read(&file, first.0.cursor).unwrap();
        assert_eq!(keys(&next.0.events), vec!["qoder#req-13".to_string()]);
        assert_eq!(next.0.events[0].counts.input, 30.0);
        assert_eq!(next.0.cursor, ReadCursor(14), "one row, one rowid forward");
        assert_eq!(next.1.rows, 1);
        // And the rows already indexed are never re-emitted.
        assert!(read(&file, next.0.cursor).unwrap().0.events.is_empty());
    }

    #[test]
    fn a_locked_or_foreign_or_malformed_database_keeps_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db");
        for cursor in [ReadCursor(0), ReadCursor(11)] {
            let (outcome, census) = read(&source_at(&missing), cursor).unwrap();
            assert!(outcome.events.is_empty() && outcome.cursor == cursor, "{cursor:?}");
            assert_eq!(census, Census::default());
            assert_eq!(stats(&missing), None);
        }
        // A valid database that is not ours (Qoder keeps other stores beside it).
        let foreign = dir.path().join("other.db");
        let conn = Connection::open(&foreign).unwrap();
        conn.execute_batch("CREATE TABLE chat_sessions (id TEXT PRIMARY KEY);").unwrap();
        drop(conn);
        let (outcome, _) = read(&source_at(&foreign), ReadCursor(3)).unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(3), "not our table");
        assert_eq!(stats(&foreign), None);
        // Bytes that are not a database at all.
        let garbage = dir.path().join("garbage.db");
        std::fs::write(&garbage, b"not sqlite, not even close").unwrap();
        assert!(read(&source_at(&garbage), ReadCursor(0)).unwrap().0.events.is_empty());
        // Our table, empty: still ours, still nothing to emit.
        let empty = dir.path().join("empty.db");
        let conn = Connection::open(&empty).unwrap();
        conn.execute_batch(DDL).unwrap();
        drop(conn);
        let (outcome, census) = read(&source_at(&empty), ReadCursor(0)).unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(0));
        assert_eq!(census.rows, 0);
        assert_eq!(census.batches, 1, "one short batch, which is also the stop signal");
        assert_eq!(stats(&empty).unwrap(), Stats { rows: 0, usage_rows: 0, requests: 0, sessions: 0 });
    }

    #[test]
    fn stats_count_the_usage_subset_without_touching_json() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let stats = stats(&file.path).unwrap();
        assert_eq!(stats.rows, 13);
        // The SQL guard mirrors the Rust one — role, non-null, neither `''` nor `{}`
        // — so `not-json` and the all-zero row stay in and the three rejects do not.
        assert_eq!(stats.usage_rows, 10, "{stats:?}");
        assert_eq!(stats.requests, 12, "req-7 covers two rows, and '' is one value");
        assert_eq!(stats.sessions, 2, "sess-1, and the row with none");
    }

    /// `id` is the vendor's primary key, so two rows can never carry one message id;
    /// the merge only exists for a window that re-delivers a row, and it is the
    /// largest snapshot that has to win. Reached here directly for that reason.
    #[test]
    fn a_re_delivered_message_keeps_its_largest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let projects = HashMap::new();
        let mut seen = Vec::new();
        let mut by_id = HashMap::new();
        let mut census = Census::default();
        for (tokens, model) in [
            (r#"{"prompt_tokens":100,"cached_tokens":0,"completion_tokens":10}"#, None),
            (r#"{"prompt_tokens":700,"cached_tokens":600,"completion_tokens":90}"#, Some("gfmodel")),
        ] {
            take_row(
                CacheRow {
                    rowid: 1,
                    message: "msg-x".into(),
                    session: "sess-1".into(),
                    request: "req-x".into(),
                    role: "assistant".into(),
                    token_info: Some(tokens.to_string()),
                    model: model.map(str::to_string),
                    ts_ms: Some(1_785_201_010_881),
                },
                &projects,
                &file,
                &mut seen,
                &mut by_id,
                &mut census,
            );
        }
        assert_eq!(census.merged_repeats, 1);
        let census = &mut census;
        let events = finish(seen, "key", census);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].counts.total(), 790.0, "700 + 90, not 810 + 110");
        assert_eq!(events[0].counts.input, 100.0);
        assert_eq!(events[0].model.as_deref(), Some("gfmodel"), "the missing model is filled in");
        assert_eq!(events[0].dedupe_key.as_deref(), Some("qoder#req-x"));
    }

    #[test]
    fn every_timestamp_dialect_lands_on_the_same_moment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        let want = 1_785_201_010_000i64;
        for (id, raw) in [("a", 1_785_201_010_881i64), ("b", 1_785_201_010)] {
            insert(
                &conn,
                &Seed {
                    gmt_create: Some(raw),
                    ..Seed::call(id, id, r#"{"prompt_tokens":5,"cached_tokens":0,"completion_tokens":1}"#)
                },
            );
        }
        conn.execute(
            "INSERT INTO chat_message (id, request_id, role, token_info, model_info, gmt_create) \
             VALUES ('c','c','assistant','{\"prompt_tokens\":5,\"cached_tokens\":0,\"completion_tokens\":1}',\
             '{\"model_key\":\"qmodel\"}','1785201010')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message (id, request_id, role, token_info, model_info, gmt_create) \
             VALUES ('d','d','assistant','{\"prompt_tokens\":5,\"cached_tokens\":0,\"completion_tokens\":1}',\
             '{\"model_key\":\"qmodel\"}','2026-07-28T01:10:10.881Z')",
            [],
        )
        .unwrap();
        drop(conn);
        let (outcome, census) = read(&source_at(&path), ReadCursor(0)).unwrap();
        assert_eq!(census.rows, 4);
        assert_eq!(outcome.events[0].ts_ms, 1_785_201_010_881, "milliseconds stay");
        assert_eq!(outcome.events[1].ts_ms, want, "seconds are promoted");
        assert_eq!(outcome.events[2].ts_ms, want, "a text number is read as seconds");
        assert_eq!(outcome.events[3].ts_ms, 1_785_201_010_881, "RFC 3339 text is parsed");
    }

    #[test]
    fn the_netting_is_the_identity_the_vendor_prices_by() {
        // Σ of the four stages must equal `prompt + completion` on every row, which
        // is what all four implementations report as the call's total.
        for raw in [
            r#"{"prompt_tokens":58299,"cached_tokens":57853,"completion_tokens":2812}"#,
            r#"{"prompt_tokens":10,"cached_tokens":20,"completion_tokens":5}"#,
            r#"{"prompt_tokens":0,"cached_tokens":0,"completion_tokens":0}"#,
            r#"{"prompt_tokens":-9,"cached_tokens":-3,"completion_tokens":"12"}"#,
            r#"{"prompt_tokens":"1000","cached_tokens":null,"completion_tokens":2.5}"#,
            r#"{"max_input_tokens":200000}"#,
        ] {
            let usage: Value = serde_json::from_str(raw).unwrap();
            let counts = netted(&usage);
            let prompt = count(usage.get("prompt_tokens"));
            let completion = count(usage.get("completion_tokens"));
            assert_eq!(counts.total(), prompt + completion, "{raw}");
            assert_eq!(counts.cache_creation, 0.0);
            assert_eq!(counts.reasoning, 0.0);
            assert!(counts.input >= 0.0 && counts.cache_read >= 0.0, "{raw}");
            assert_eq!(counts.credits, 0.0);
        }
    }
}
