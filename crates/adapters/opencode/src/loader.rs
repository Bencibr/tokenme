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

/// Rows re-read behind the cursor on every pass. A row's rowid does not move when
/// the app finishes filling it, so a row the cursor has passed would otherwise
/// never be seen again. The bound this has to outspan is how many *other* rows
/// land while one is still open: measured over this machine's store, 8,455 rows
/// are updated after creation, the worst of them after **646** others were
/// written (a 23-minute-open row), so 2048 leaves a three-fold margin and still
/// costs one bounded `LIMIT` read per pass. A row that somehow falls outside it is
/// repaired by the next rebuild, which reads from zero.
const TAIL_ROWS: i64 = 2048;

/// `rowid` is the cursor: monotonic, and the only stable ordering the table has.
/// The role filter runs inside SQLite so user messages never reach Rust, and the
/// `CASE` guard matters: bare `json_extract` aborts the *whole query* the moment
/// one row holds torn JSON, which would silently stall ingestion at that rowid.
/// In the v2 table the role is a column, so no JSON guard is needed or possible.
fn select_for(table: &str, role_from_column: bool) -> String {
    let role = if role_from_column {
        "type = 'assistant'"
    } else {
        "CASE WHEN json_valid(data) THEN json_extract(data, '$.role') ELSE NULL END = 'assistant'"
    };
    format!(
        "SELECT rowid, id, session_id, time_created, time_updated, data FROM {table} \
         WHERE rowid > ?1 AND {role} ORDER BY rowid LIMIT ?2"
    )
}

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
    read_batch(path, cursor, source_key, product, BATCH_ROWS, TAIL_ROWS)
}

/// `tail` is how far behind the cursor a pass looks; `0` gives the plain
/// append-only walk, which is what the cursor tests assert against.
pub(crate) fn read_batch(
    path: &Path,
    cursor: ReadCursor,
    source_key: &str,
    product: &Product,
    batch: i64,
    tail: i64,
) -> ReadOutcome {
    match try_read(path, cursor, source_key, product, batch, tail) {
        Ok(out) => out,
        // A missing, locked or mid-migration database is not something the caller
        // can act on: report nothing this pass and leave the cursor alone. The
        // cached open rung — if one was trusted here — proved wrong, so forget it
        // and let the next pass re-run the full ladder.
        Err(_) => {
            paths::forget_rung(path);
            ReadOutcome { events: Vec::new(), cursor }
        }
    }
}

fn try_read(
    path: &Path,
    cursor: ReadCursor,
    source_key: &str,
    product: &Product,
    batch: i64,
    tail: i64,
) -> rusqlite::Result<ReadOutcome> {
    let conn = paths::open_readonly(path, paths::USABLE_SQL)?;
    // Which of the dialect's record tables this store actually writes.
    let Some(dialect) = paths::dialect(&conn) else {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    };
    // Two signals say the stored cursor is not this table's: a schema migration
    // rebuilds the table with fresh rowids, so a cursor above its own high-water
    // mark can only belong to an older generation (measured 2026-09-29: five days
    // of usage invisible after OpenCode's 09-24 migration); and a cursor sitting
    // exactly on the sibling table's mark says the product flipped its write
    // target, because that is the mark a walk over the old table left behind.
    // Either way restart from zero — every event carries the message's primary
    // key as its dedupe key, so the replay is absorbed idempotently. While the
    // young table is still smaller than the mark it inherits, the indexer's
    // never-rewinds-cursor rule makes that replay repeat each pass; the price of
    // never losing a row is re-reading a few thousand of them.
    //
    // The sibling test additionally demands the chosen table's own mark to differ:
    // two tables that happen to be the same size are not a flip, and treating
    // that as one would replay the whole live table on every pass forever.
    let stale = cursor.0 as i64 > dialect.max_rowid
        || (dialect.sibling_max == Some(cursor.0 as i64) && dialect.max_rowid != cursor.0 as i64);
    let resume = if stale { 0 } else { cursor.0 as i64 };
    // Then re-read a bounded tail even when the cursor says nothing is new.
    // OpenCode fills an assistant row **in place**: the row is created with zero
    // tokens and finished seconds later at the same rowid, so a cursor that
    // already passed it never sees the usage. Measured here: rowid 8426 of an
    // 8607-row store was consumed as a placeholder, completed 2.1 minutes later,
    // and left 105,704 tokens unbilled until a full rebuild. Replaying the tail
    // lets the index's monotone `new_total > old_total` update repair the row.
    let start = if tail > 0 { resume.min(dialect.max_rowid.saturating_sub(tail)) } else { resume };
    let sql = select_for(dialect.table, dialect.role_from_column);
    // One call has to cover the whole tail: the walk only stops when the cursor
    // moves, so a batch cut off before the tail's end would re-read the *oldest*
    // tail rows on every pass and starve the newest ones — the exact opposite of
    // what the replay is for.
    let limit = if tail > 0 { batch.max(tail + 1) } else { batch };
    let rows = match fetch_batch(&conn, &sql, start, limit) {
        Ok(rows) => rows,
        // A batch that cannot be fetched under the (possibly cached) open rung
        // is the read failing, not the table being empty: forget the rung.
        Err(_) => {
            paths::forget_rung(path);
            Vec::new()
        }
    };

    let mut session_stmt = conn.prepare_cached(SELECT_SESSION).ok();
    // One query per distinct session per batch, not one per row.
    let mut dirs: HashMap<String, Option<String>> = HashMap::new();
    let mut events = Vec::with_capacity(rows.len());
    // Cursor advances to the last row that actually decoded, so a torn batch
    // resumes there instead of replaying rows already turned into events. A
    // recent zero row is still parked on — cheaper than the tail replay — but
    // the tail above is what guarantees the repair even if this pass never sees
    // the row again.
    let mut last_rowid = start.max(0) as u64;
    let now_ms = chrono::Local::now().timestamp_millis();

    for r in rows {
        let Some(parsed) = parser::parse_message(&r.data, dialect.role_from_column) else {
            if is_recent_transient_zero(&r.data, r.time_created, r.time_updated, now_ms, dialect.role_from_column)
            {
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
fn fetch_batch(conn: &Connection, sql: &str, after: i64, batch: i64) -> rusqlite::Result<Vec<RowData>> {
    let mut stmt = conn.prepare_cached(sql)?;
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

fn is_recent_transient_zero(
    data: &str,
    time_created: i64,
    time_updated: i64,
    now_ms: i64,
    role_from_column: bool,
) -> bool {
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
    // The v2 table settled the role in a column the query already filtered on;
    // the v1 shape keeps the string probe, which is redundant there too but is
    // the only role evidence a mis-shaped row leaves behind.
    role_from_column || data.contains("\"role\":\"assistant\"") || data.contains("\"role\": \"assistant\"")
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
            let out = read_batch(&path, cursor, "db", &crate::paths::OPENCODE, 2, 0);
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
        let recovered = read_batch(&path, ReadCursor(500), "db", &crate::paths::OPENCODE, 100, 0);
        assert_eq!(recovered.events.len(), 5, "the whole table replays in the same pass");
        let keys: Vec<String> = recovered.events.iter().map(|e| e.dedupe_key.clone().unwrap()).collect();
        assert_eq!(keys, vec!["msg1", "msg2", "msg3", "msg4", "msg5"]);
        assert_eq!(recovered.cursor, ReadCursor(5));
        // And the steady state afterwards is the ordinary append-only walk.
        let done = read_batch(&path, recovered.cursor, "db", &crate::paths::OPENCODE, 100, 0);
        assert!(done.events.is_empty());
        assert_eq!(done.cursor, ReadCursor(5));
    }

    /// A row's rowid does not move when the app finishes filling it, so a walk
    /// that only looks ahead of the cursor never sees the usage it gained.
    #[test]
    fn the_tail_rereads_a_row_the_cursor_already_passed() {
        let dir = tempfile::tempdir().unwrap();
        let path = five_row_db(dir.path());
        // Row 3 becomes a placeholder: consumed with nothing to bill, so the
        // cursor moves past it and no event is minted.
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE message SET data = '{\"role\":\"assistant\",\"tokens\":{\"total\":0}}' WHERE rowid = 3",
                [],
            )
            .unwrap();

        let before = read_batch(&path, ReadCursor(0), "db", &crate::paths::OPENCODE, 100, 0);
        assert_eq!(before.events.len(), 4, "the placeholder is not billable yet");
        assert_eq!(before.cursor, ReadCursor(5));

        // The app fills it in. Its rowid is where it always was.
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE message SET data = '{\"role\":\"assistant\",\"modelID\":\"m3\",\
                 \"tokens\":{\"total\":105704,\"input\":105628,\"output\":76,\"reasoning\":0,\
                 \"cache\":{\"write\":0,\"read\":0}}}' WHERE rowid = 3",
                [],
            )
            .unwrap();

        let ahead_only = read_batch(&path, before.cursor, "db", &crate::paths::OPENCODE, 100, 0);
        assert!(ahead_only.events.is_empty(), "which is the hole the tail exists to close");

        // A tail of 3 behind a max of 5 starts at rowid 2, so row 3 is read again.
        let healed = read_batch(&path, before.cursor, "db", &crate::paths::OPENCODE, 100, 3);
        let keys: Vec<String> = healed.events.iter().filter_map(|e| e.dedupe_key.clone()).collect();
        assert!(keys.contains(&"msg3".to_string()), "the finished row comes back: {keys:?}");
        assert_eq!(
            healed
                .events
                .iter()
                .find(|e| e.dedupe_key.as_deref() == Some("msg3"))
                .unwrap()
                .counts
                .total(),
            105_704.0
        );
        assert!(healed.cursor >= before.cursor, "a replay never rewinds the cursor");
    }

    /// The dialect's v2 record table is a different shape: the role is a `type`
    /// column and `data` carries none. A store writing it must be read, not
    /// reported as carrying nothing.
    #[test]
    fn the_v2_table_is_read_when_it_is_the_one_with_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id text PRIMARY KEY, session_id text NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
             CREATE TABLE session_message (id text PRIMARY KEY, session_id text NOT NULL, type text NOT NULL, seq integer NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);",
        )
        .unwrap();
        // `message` holds the frozen legacy generation; `session_message` is live.
        conn.execute("INSERT INTO message VALUES ('old','s',1787799862846,1787799862846,'{}')", [])
            .unwrap();
        for i in 1..=3 {
            conn
                .execute(
                    "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
                     VALUES (?1,'s','assistant',?2,1787799862846,1787799862846,\
                     '{\"modelID\":\"m\",\"tokens\":{\"total\":40,\"input\":30,\"output\":10,\"reasoning\":0,\"cache\":{\"write\":0,\"read\":0}}}')",
                    rusqlite::params![format!("v2-{i}"), i],
                )
                .unwrap();
        }
        drop(conn);

        let out = read_batch(&path, ReadCursor(0), "db", &crate::paths::OPENCODE, 100, 0);
        assert_eq!(out.events.len(), 3, "the table with rows is the table that bills");
        assert_eq!(out.events[0].dedupe_key.as_deref(), Some("v2-1"));
        assert_eq!(out.events[0].counts.total(), 40.0);
        assert_eq!(out.events[0].model.as_deref(), Some("m"));
    }
}
