//! The billable rows of Hermes's `state.db`.
//!
//! One cumulative row per `(session, model, billing_provider, base_url, mode,
//! task)` in `session_model_usage`: an upsert keeps raising the token counters
//! while a session runs, so a row is the whole-to-date consumption of that
//! slice, not a delta. The events handed up are those cumulative totals with a
//! stable dedupe key — the indexer replaces on the key, so re-reading a row is
//! idempotent and the cursor may replay it freely.
//!
//! The dedupe key carries **all six** primary-key columns, in the PK's own
//! order. The index's dedupe index is one global unique index over `dedupe_key`
//! that replaces on grow (`usage_index::ingest`), so a key coarser than the
//! source's PK silently folds two distinct slices into one row: the second
//! slice's smaller total loses to the first and its tokens are dropped for
//! good. Hermes keeps that pair in the wild — the same session, model and
//! provider under `https://host/api/v4` and `https://host/api/v4/`, or under
//! two `billing_mode` values — so the key must be as fine as the PK, no
//! narrower.
//!
//! Widening the key also re-draws the key space: rows indexed under the old
//! four-field key stay stranded beside the new six-field ones and the next
//! pass re-emits the slice at full value, so the store needs its read cursor
//! dropped (or its hermes rows purged) once — the same one-shot the index
//! already does for `repair:cumulative-rescan-1`. That repair lives in
//! `usage-index`, not here.
//!
//! The cursor is the highest `last_seen` consumed (milliseconds). An SQLite
//! `UPDATE` keeps the rowid, so the rowid cursor the append-only sources use
//! would never re-see a live row's growth; `last_seen` bumps on every call and
//! is the column the tool itself orders recency by. A zero-token row (session
//! opened, nothing answered) is skipped but still advances the cursor — when
//! its tokens land, `last_seen` moves past the mark and the row is caught then.
//!
//! `reasoning_tokens` is a sub-breakdown of `output_tokens` — the same identity
//! the zcode adapter asserts for this vendor family — so it maps onto
//! `TokenCounts.reasoning` and never enters `total()` twice.
//!
//! Read-only against a database the gateway owns and writes while it runs:
//! open read-only and treat a lock as "no rows this pass", never an error.

use rusqlite::{Connection, OpenFlags};

use crate::DEDUPE_PREFIX;
use usage_core::{Error, Meter, ReadCursor, ReadOutcome, SourceFile, UsageEvent};

/// Rows touched in one pass; per-session slices stay small next to a
/// transcript row count.
const BATCH: i64 = 512;

const QUERY: &str = "\
SELECT u.session_id, u.model, u.billing_provider, u.billing_base_url, u.billing_mode, u.task, \
       u.api_call_count, u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens, \
       u.reasoning_tokens, u.last_seen, s.cwd \
FROM session_model_usage u LEFT JOIN sessions s ON s.id = u.session_id \
WHERE u.last_seen > ?1 \
ORDER BY u.last_seen LIMIT ?2";

/// Cheap probe census: one aggregate query, run at startup only.
pub fn count_usage(path: &std::path::Path) -> Option<i64> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    conn.query_row("SELECT count(*) FROM session_model_usage", [], |r| r.get(0)).ok()
}

pub fn read(file: &SourceFile, cursor: ReadCursor) -> Result<(ReadOutcome, u64), Error> {
    let conn = match Connection::open_with_flags(
        &file.path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        // A locked or mid-write database is a skip, not a failure: the next
        // pass sees the file as changed and retries.
        Err(_) => return Ok((ReadOutcome { events: Vec::new(), cursor }, cursor.0)),
    };
    let mut stmt = match conn.prepare(QUERY) {
        Ok(stmt) => stmt,
        Err(_) => return Ok((ReadOutcome { events: Vec::new(), cursor }, cursor.0)),
    };
    // The cursor counts milliseconds; `last_seen` is REAL seconds.
    let since_s = cursor.0 as f64 / 1000.0;
    let rows = stmt
        .query_map(rusqlite::params![since_s, BATCH], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, f64>(12)?,
                row.get::<_, Option<String>>(13)?,
            ))
        })
        .map_err(|source| Error::Io { path: file.path.clone(), source: std::io::Error::other(source) })?;

    let mut events = Vec::new();
    let mut high = cursor.0;
    for row in rows {
        let (
            session_id,
            model,
            billing_provider,
            billing_base_url,
            billing_mode,
            task,
            _api_calls,
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
            last_seen,
            cwd,
        ) = row.map_err(|source| Error::Io { path: file.path.clone(), source: std::io::Error::other(source) })?;
        let ts_ms = (last_seen.max(0.0) * 1000.0) as u64;
        high = high.max(ts_ms);
        // A session that opened but never answered: nothing to bill yet, and
        // when that changes `last_seen` moves past the cursor. The token
        // columns are the billable truth — a call count without tokens is
        // still an empty slice.
        if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
            continue;
        }
        let mut event = UsageEvent::new(crate::TOOL_ID, ts_ms as i64, &session_id);
        event.counts.input = input as f64;
        event.counts.output = output as f64;
        event.counts.cache_read = cache_read as f64;
        event.counts.cache_creation = cache_write as f64;
        event.counts.reasoning = reasoning as f64;
        event.meter = Meter::Tokens;
        event.model = Some(model.clone());
        event.project = cwd.as_deref().filter(|c| !c.is_empty()).map(project_name);
        event.dedupe_key = Some(format!(
            "{DEDUPE_PREFIX}{}#{}#{}#{}#{}#{}",
            key_segment(&session_id),
            key_segment(&model),
            key_segment(&billing_provider),
            key_segment(&billing_base_url),
            key_segment(&billing_mode),
            key_segment(&task),
        ));
        event.source = file.key();
        events.push(event);
    }
    Ok((ReadOutcome { events, cursor: ReadCursor(high) }, high))
}

/// One slice field as it appears inside the dedupe key.
///
/// The key must be an injective encoding of the six PK columns: `#` separates
/// them, so a value that contains `#` (a URL fragment, an id with a hash mark)
/// would let two different slices spell the same key, and `%` is this escape's
/// lead — escaping `%` itself (`a%23b` → `a%2523b`) is what keeps `a#b` and
/// `a%23b` apart instead of folding them together.
///
/// `:` is escaped for the sync layer's convention, not for a bug open today.
/// A dedupe key never becomes a `source`, so [`usage_core::origin_of`]'s
/// `linux:<origin>:<key>` split is not reached by it, and the only place a
/// dedupe key is wrapped — `sync_event_identity`'s `key:<dedupe_key>` — is
/// prefixed by a fixed literal and compared whole, never re-split at the first
/// colon (sync's own fallback keys, `mix:<origin>:<hash>`, carry colons and
/// rely on that). But `usage_core::origin_ok` documents `:` as reserved inside
/// dedupe keys as well, and `https://host/api/v4` is exactly the kind of value
/// this column holds, so a bare colon here is a landmine for the next parser
/// added on the other side. Escaping is reversible and costs nothing, so the
/// key keeps the guarantee instead of relying on it being unneeded.
///
/// `/`, `?`, `=` and friends stay literal: they are not separators anywhere in
/// the key or the sync protocol, and a readable base URL in a stored key is
/// worth debugging.
fn key_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            ':' => out.push_str("%3A"),
            other => out.push(other),
        }
    }
    out
}

/// `C:\Users\me\workspace\tokenme` → `tokenme`; the panel shows basenames, and
/// Hermes runs on both separators.
fn project_name(cwd: &str) -> String {
    cwd.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    /// The two tables the adapter reads, with the tool's own column set.
    pub const DDL: &str = "\
CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT); \
CREATE TABLE session_model_usage (session_id TEXT NOT NULL, model TEXT NOT NULL, \
 billing_provider TEXT NOT NULL DEFAULT '', billing_base_url TEXT NOT NULL DEFAULT '', \
 billing_mode TEXT NOT NULL DEFAULT '', task TEXT NOT NULL DEFAULT '', \
 api_call_count INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0, \
 output_tokens INTEGER NOT NULL DEFAULT 0, cache_read_tokens INTEGER NOT NULL DEFAULT 0, \
 cache_write_tokens INTEGER NOT NULL DEFAULT 0, reasoning_tokens INTEGER NOT NULL DEFAULT 0, \
 estimated_cost_usd REAL NOT NULL DEFAULT 0, actual_cost_usd REAL NOT NULL DEFAULT 0, \
 cost_status TEXT, cost_source TEXT, first_seen REAL, last_seen REAL, \
 PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode, task));";

    pub fn fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("state.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        path
    }

    pub fn insert_usage(db: &Path, session: &str, model: &str, task: &str, input: i64, output: i64, last_seen: f64) {
        insert_slice(db, session, model, "", "", "", task, input, output, last_seen);
    }

    /// Inserts one full `(session, model, provider, base_url, mode, task)`
    /// slice — the whole primary key, because two rows differing only in
    /// `billing_base_url` or `billing_mode` are two rows the old narrower key
    /// folded into one.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_slice(
        db: &Path,
        session: &str,
        model: &str,
        provider: &str,
        base_url: &str,
        mode: &str,
        task: &str,
        input: i64,
        output: i64,
        last_seen: f64,
    ) {
        let conn = Connection::open(db).unwrap();
        // INSERT OR REPLACE: the tool upserts the slice as a session runs, and
        // the composite PK is what a second call collides on.
        conn.execute(
            "INSERT OR REPLACE INTO session_model_usage \
             (session_id, model, billing_provider, billing_base_url, billing_mode, task, \
              api_call_count, input_tokens, output_tokens, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 3, ?7, ?8, ?9)",
            rusqlite::params![session, model, provider, base_url, mode, task, input, output, last_seen],
        )
        .unwrap();
    }

    #[test]
    fn a_segment_escape_is_reversible_and_keeps_the_separators_free() {
        // The separator itself cannot survive an escape: `a#b` and `a`+`b` in
        // two different fields must not spell one key.
        assert_eq!(key_segment("a#b"), "a%23b");
        assert_eq!(key_segment("a%23b"), "a%2523b", "the escape lead escapes itself");
        assert_eq!(key_segment("https://open.bigmodel.cn/api/paas/v4/"), "https%3A//open.bigmodel.cn/api/paas/v4/");
        assert_eq!(key_segment(""), "");
        assert_ne!(key_segment("a#b"), key_segment("a%23b"), "no two values share an encoding");
    }
}
