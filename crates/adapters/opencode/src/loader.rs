//! Batched, read-only ingestion of OpenCode's `message` table.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, Row};
use usage_core::{Meter, ReadCursor, ReadOutcome, UsageEvent};

use crate::parser;
use crate::paths;
use crate::paths::Product;

/// Rows per `read` call. The indexer loops until the cursor stops advancing, so
/// this only bounds how long one query runs and how much a pass holds in memory.
pub(crate) const BATCH_ROWS: i64 = 2000;

/// `rowid` is the cursor: monotonic, and the only stable ordering the table has.
/// The role filter runs inside SQLite so user messages never reach Rust, and the
/// `CASE` guard matters: bare `json_extract` aborts the *whole query* the moment
/// one row holds torn JSON, which would silently stall ingestion at that rowid.
const SELECT: &str = "SELECT rowid, id, session_id, time_created, time_updated, data FROM message \
     WHERE rowid > ?1 \
       AND CASE WHEN json_valid(data) THEN json_extract(data, '$.role') ELSE NULL END = 'assistant' \
     ORDER BY rowid LIMIT ?2";

/// `session.directory` is the project label; `project.name` covers a session row
/// that was pruned while its project survived.
const SELECT_SESSION: &str = "SELECT s.directory, p.name FROM session s \
     LEFT JOIN project p ON p.id = s.project_id WHERE s.id = ?1";

/// One raw assistant row, decoupled from the statement so the session lookup can
/// use the same connection without nesting two live cursors.
struct RowData {
    rowid: i64,
    id: String,
    session_id: String,
    time_created: i64,
    time_updated: i64,
    data: String,
}

fn take_row(row: &Row<'_>) -> rusqlite::Result<RowData> {
    Ok(RowData {
        rowid: row.get(0)?,
        id: row.get(1)?,
        session_id: row.get(2)?,
        time_created: row.get(3)?,
        time_updated: row.get(4)?,
        data: row.get(5)?,
    })
}

/// Reads one batch of assistant rows after `cursor`, stamping every event with
/// `product.id` so sibling forks report as themselves and not as OpenCode.
pub(crate) fn read_messages(path: &Path, cursor: ReadCursor, source_key: &str, product: &Product) -> ReadOutcome {
    read_batch(path, cursor, source_key, product, BATCH_ROWS)
}

pub(crate) fn read_batch(path: &Path, cursor: ReadCursor, source_key: &str, product: &Product, batch: i64) -> ReadOutcome {
    match try_read(path, cursor, source_key, product, batch) {
        Ok(out) => out,
        // A missing, locked or mid-migration database is not something the caller
        // can act on: report nothing this pass and leave the cursor alone.
        Err(_) => ReadOutcome { events: Vec::new(), cursor },
    }
}

fn try_read(path: &Path, cursor: ReadCursor, source_key: &str, product: &Product, batch: i64) -> rusqlite::Result<ReadOutcome> {
    let conn = paths::open_readonly(path, paths::USABLE_SQL)?;
    // A schema migration rebuilds the table with fresh rowids; a stored cursor
    // above the table's own high-water mark then starves ingestion forever
    // (measured 2026-09-29: five days of usage invisible after OpenCode's
    // 09-24 migration). Restart from zero — every event carries the message's
    // primary key as its dedupe key, so the replay is absorbed idempotently.
    let max_rowid: i64 = conn
        .query_row("SELECT COALESCE(MAX(rowid), 0) FROM message", [], |r| r.get(0))
        .unwrap_or(0);
    let cursor = if cursor.0 as i64 > max_rowid { ReadCursor(0) } else { cursor };
    let rows = fetch_batch(&conn, cursor.0 as i64, batch).unwrap_or_default();

    let mut session_stmt = conn.prepare_cached(SELECT_SESSION).ok();
    // One query per distinct session per batch, not one per row.
    let mut dirs: HashMap<String, Option<String>> = HashMap::new();
    let mut events = Vec::with_capacity(rows.len());
    // Cursor advances to the last row that actually decoded, so a torn batch
    // resumes there instead of replaying rows already turned into events.
    // However, OpenCode writes a placeholder assistant row with zero tokens and
    // fills it a few seconds later (time_updated >> time_created). If we advance
    // past a recent zero row, its later update is forever lost because the rowid
    // never moves. So a recent zero is treated as transient: we park the cursor
    // before it and retry on the next pass.
    let mut last_rowid = cursor.0;
    let now_ms = chrono::Local::now().timestamp_millis();

    for r in rows {
        let Some(parsed) = parser::parse_message(&r.data) else {
            if is_recent_transient_zero(&r.data, r.time_created, r.time_updated, now_ms) {
                break;
            }
            last_rowid = r.rowid.max(0) as u64;
            continue;
        };
        last_rowid = r.rowid.max(0) as u64;
        let mut ev = UsageEvent::new(product.id, normalise_ms(r.time_created), r.session_id.clone());
        ev.project = project_of(session_stmt.as_mut(), &r.session_id, &mut dirs, parsed.cwd.as_deref());
        ev.model = parsed.model;
        ev.meter = Meter::Tokens;
        ev.counts = parsed.counts;
        // Every message has a primary key, so a replayed batch still dedupes even
        // if the cursor had to be reset.
        ev.dedupe_key = Some(r.id);
        ev.source = source_key.to_string();
        events.push(ev);
    }

    Ok(ReadOutcome { events, cursor: ReadCursor(last_rowid) })
}

/// `SQLITE_BUSY` mid-query (the app is checkpointing) yields nothing rather than
/// an error; the next pass retries from the same cursor.
fn fetch_batch(conn: &Connection, after: i64, batch: i64) -> rusqlite::Result<Vec<RowData>> {
    let mut stmt = conn.prepare_cached(SELECT)?;
    let rows = stmt.query_map(rusqlite::params![after, batch], take_row)?;
    let mut out = Vec::new();
    for r in rows {
        match r {
            Ok(r) => out.push(r),
            Err(_) => break,
        }
    }
    Ok(out)
}

fn normalise_ms(ts: i64) -> i64 {
    // `time_created` is epoch milliseconds; a source that ever writes seconds
    // would otherwise land its events in 1970.
    if ts < 10_000_000_000 {
        ts * 1000
    } else {
        ts
    }
}

const RECENT_MS: i64 = 10 * 60 * 1000;

fn is_recent_transient_zero(data: &str, time_created: i64, time_updated: i64, now_ms: i64) -> bool {
    // Only defer a row that is still "in flight": assistant with zero tokens but
    // created/updated within the last few minutes. An old zero (like 1723) is
    // permanently unbillable and must be skipped, otherwise the cursor would
    // never advance past it.
    // We don't re-parse tokens here beyond a cheap check; any parse failure on
    // a recent row is worth retrying because the writer may still be filling it.
    let recent = |t: i64| {
        let t = normalise_ms(t);
        t > 0 && now_ms.saturating_sub(t) < RECENT_MS
    };
    if !(recent(time_created) || recent(time_updated)) {
        return false;
    }
    // Heuristic: recent failure is likely a placeholder zero. We defer it.
    // Checking the role is redundant (SQL already filtered), but cheap.
    data.contains("\"role\":\"assistant\"") || data.contains("\"role\": \"assistant\"")
}

fn project_of(
    stmt: Option<&mut rusqlite::CachedStatement<'_>>,
    session_id: &str,
    dirs: &mut HashMap<String, Option<String>>,
    cwd_from_message: Option<&str>,
) -> Option<String> {
    let known = dirs.get(session_id).cloned();
    let directory = match known {
        Some(found) => found,
        None => {
            let found = match stmt {
                Some(s) => s
                    .query_row(rusqlite::params![session_id], |row| {
                        let dir: String = row.get(0)?;
                        let name: Option<String> = row.get(1)?;
                        Ok((dir, name))
                    })
                    .ok()
                    .and_then(|(dir, name)| if dir.trim().is_empty() { name } else { Some(dir) }),
                None => None,
            };
            dirs.insert(session_id.to_string(), found.clone());
            found
        }
    };
    directory.or_else(|| cwd_from_message.map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millisecond_and_second_timestamps_both_land_in_milliseconds() {
        assert_eq!(normalise_ms(1_787_799_862_846), 1_787_799_862_846);
        assert_eq!(normalise_ms(1_787_799_862), 1_787_799_862_000);
    }

    fn five_row_db(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("opencode.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id text PRIMARY KEY, session_id text NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);",
        )
        .unwrap();
        for i in 1..=5 {
            let data = format!(
                r#"{{"role":"assistant","modelID":"m{i}","tokens":{{"total":100,"input":90,"output":10,"reasoning":0,"cache":{{"write":0,"read":0}}}}}}"#
            );
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1,'s',?2,?2,?3)",
                rusqlite::params![format!("msg{i}"), 1_787_799_862_846i64 + i, data],
            )
            .unwrap();
        }
        path
    }

    /// The indexer loops `read` until the cursor stops advancing, so each call
    /// must consume exactly one batch and resume where it stopped.
    #[test]
    fn batches_walk_the_rowid_cursor_forward_without_replaying_or_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let path = five_row_db(dir.path());
        let mut seen: Vec<String> = Vec::new();
        let mut cursor = ReadCursor(0);
        for step in 0..4 {
            let out = read_batch(&path, cursor, "db", &crate::paths::OPENCODE, 2);
            for e in &out.events {
                seen.push(e.dedupe_key.clone().unwrap());
            }
            assert!(out.cursor > cursor || out.events.is_empty(), "cursor must move while rows remain");
            cursor = out.cursor;
            if step == 3 {
                assert!(out.events.is_empty(), "a fourth call has nothing left to give");
            }
        }
        assert_eq!(cursor, ReadCursor(5));
        assert_eq!(
            seen,
            vec!["msg1", "msg2", "msg3", "msg4", "msg5"],
            "one batch per call, in rowid order, no row twice"
        );
    }

    /// A schema migration rebuilds the table with rowids that start over: a
    /// cursor above the new high-water mark restarts from zero in the same
    /// pass instead of starving forever, and the replay still dedupes by
    /// message id.
    #[test]
    fn a_cursor_above_the_migrated_high_water_mark_restarts_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = five_row_db(dir.path());
        // The old generation's cursor: far above anything the new table holds.
        let recovered = read_batch(&path, ReadCursor(500), "db", &crate::paths::OPENCODE, 100);
        assert_eq!(recovered.events.len(), 5, "the whole table replays in the same pass");
        let keys: Vec<String> = recovered.events.iter().map(|e| e.dedupe_key.clone().unwrap()).collect();
        assert_eq!(keys, vec!["msg1", "msg2", "msg3", "msg4", "msg5"]);
        assert_eq!(recovered.cursor, ReadCursor(5));
        // And the steady state afterwards is the ordinary append-only walk.
        let done = read_batch(&path, recovered.cursor, "db", &crate::paths::OPENCODE, 100);
        assert!(done.events.is_empty());
        assert_eq!(done.cursor, ReadCursor(5));
    }
}
