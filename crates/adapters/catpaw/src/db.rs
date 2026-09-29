//! The one reader: `t_ui_messages` rows whose content JSON carries a
//! `tokenUsage` object, one event per billed model call.
//!
//! The row's `rowid` is the cursor ([`usage_core::FileKind::Sqlite`]). A
//! corrupted store is a supported state on long-running installs (this
//! machine's 2.5 GB copy is): the first unreadable page stops the pass and the
//! cursor stays at the last good row, so a healing store is picked up on a
//! later refresh without a rescan, and re-reads are idempotent because the
//! dedupe keys are stable. Nothing is ever invented to fill a gap.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use usage_core::{Error, ReadCursor, ReadOutcome, TokenCounts, UsageEvent};

pub(crate) const TOOL_ID: &str = "catpaw";

/// Row census for the smoke tests.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Census {
    pub rows: usize,
    pub events: usize,
    /// Rows skipped for having no `tokenUsage` (thinking / error / echo rows).
    pub plain_rows: usize,
    pub max_rowid: i64,
    pub totals: TokenCounts,
}

/// Read usage-bearing rows appended after `cursor`.
pub fn read(path: &Path, cursor: ReadCursor, source_key: &str) -> Result<(ReadOutcome, Census), Error> {
    let Ok(conn) = open(path) else {
        return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default()));
    };
    if !table_exists(&conn, "t_ui_messages") {
        return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default()));
    }
    let Ok(max_rowid) = scalar(&conn, "SELECT COALESCE(MAX(rowid), 0) FROM t_ui_messages") else {
        return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default()));
    };
    if i64::try_from(cursor.0).unwrap_or(i64::MAX) > max_rowid {
        return Err(Error::Cursor { path: path.to_path_buf(), cursor: cursor.0 });
    }
    // The conversation's own row carries the workspace path; a missing table
    // only costs the label, never the usage.
    let project_sub = if table_exists(&conn, "t_conversation") {
        "(SELECT c.project_path FROM t_conversation c WHERE c.conversation_id = m.conversation_id)"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT m.rowid AS rowid, m.conversation_id, m.message_id, m.content, m.create_time, \
         {project_sub} AS project \
         FROM t_ui_messages m WHERE m.rowid > ?1 ORDER BY m.rowid"
    );
    let mut events = Vec::new();
    let mut census = Census { max_rowid, ..Census::default() };
    let mut reached_end = true;
    {
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default()));
        };
        let key = source_key.to_string();
        let Ok(mut rows) = stmt.query([max_rowid_of(cursor)]) else {
            return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default()));
        };
        loop {
            match rows.next() {
                Err(_) => {
                    // First unreadable page: stop, keep what was parsed, and
                    // let the cursor stay behind so the healed rows re-read.
                    reached_end = false;
                    break;
                }
                Ok(None) => break,
                Ok(Some(row)) => {
                    census.rows += 1;
                    match row_to_event(row, &key) {
                        Ok((Some(event), counts)) => {
                            census.events += 1;
                            census.totals += &counts;
                            events.push(event);
                        }
                        Ok((None, counts)) => {
                            census.plain_rows += usize::from(counts.is_zero());
                        }
                        Err(_) => census.plain_rows += 1,
                    }
                }
            }
        }
    }
    let cursor = if reached_end {
        census.max_rowid = max_rowid;
        ReadCursor(u64::try_from(max_rowid).unwrap_or(u64::MAX))
    } else {
        cursor
    };
    Ok((ReadOutcome { events, cursor }, census))
}

/// `(event or skip, counts)` — the counts return even for a skipped row so the
/// census can account for what the row claimed.
fn row_to_event(row: &rusqlite::Row<'_>, source_key: &str) -> rusqlite::Result<(Option<UsageEvent>, TokenCounts)> {
    let rowid: i64 = row.get::<_, i64>("rowid")?;
    let conversation: Option<String> = row.get::<_, Option<String>>("conversation_id")?;
    let message_id: Option<String> = row.get::<_, Option<String>>("message_id")?;
    let content_raw: Option<String> = row.get::<_, Option<String>>("content")?;
    let create_time: Option<i64> = row.get::<_, Option<i64>>("create_time")?;
    let project: Option<String> = row.get::<_, Option<String>>("project")?;

    // A row without a wall-clock time has no place a period could be attributed
    // to, and usage without a period is noise; skip rather than synthesise.
    let Some(ts_ms) = create_time.filter(|v| *v > 0) else {
        return Ok((None, TokenCounts::default()));
    };
    let parsed = content_raw.as_deref().and_then(|c| serde_json::from_str::<Value>(c).ok());
    let Some(counts) = parsed.as_ref().and_then(token_usage) else {
        return Ok((None, TokenCounts::default()));
    };
    // `actualUseModelName` is CatPaw's own internal id ("100000000013"); the
    // price table knows no such id, so these rows honestly report as unpriced
    // until the vendor exposes a mapping.
    let model = parsed
        .as_ref()
        .and_then(|c| c.get("actualUseModelName"))
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty());
    let session = conversation
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| format!("catpaw-row{rowid}"));
    let mut event = UsageEvent::new(TOOL_ID, ts_ms, &session).with(counts);
    event.meter = usage_core::Meter::Tokens;
    event.model = model.map(str::to_string);
    event.project = basename(&project);
    // One row is one model call: the message id is the billed identity.
    event.dedupe_key = message_id.map(|mid| format!("{session}#{mid}"));
    event.source = format!("{source_key}#t_ui_messages#{rowid}");
    Ok((Some(event), counts))
}

/// `content.tokenUsage` — the exclusive convention: `prompt_tokens` excludes
/// `cacheReadTokens` (measured on real rows: prompt 366 + cacheRead 156_416 +
/// completion 163 with `total_tokens` 529 = 366 + 163). Absent / null / wrong
/// types read as zero, and an all-zero row is not a billable call.
fn token_usage(content: &Value) -> Option<TokenCounts> {
    let usage = content.get("tokenUsage")?.as_object()?;
    let num = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
    let counts = TokenCounts {
        input: num("prompt_tokens"),
        cache_creation: num("cacheWriteTokens"),
        cache_read: num("cacheReadTokens"),
        output: num("completion_tokens"),
        reasoning: 0.0,
        credits: 0.0,
    };
    (!counts.is_zero()).then_some(counts)
}

fn basename(path: &Option<String>) -> Option<String> {
    Some(Path::new(path.as_deref()?).file_name()?.to_str()?.to_string())
}

fn max_rowid_of(cursor: ReadCursor) -> i64 {
    i64::try_from(cursor.0).unwrap_or(i64::MAX)
}

fn open(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| Error::io(path, std::io::Error::other(e)))?;
    // The IDE keeps its own writer open; a lock means "busy", not "gone".
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    Ok(conn)
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .and_then(|mut s| s.query_row([name], |_| Ok(())).optional())
        .unwrap_or(None)
        .is_some()
}

fn scalar(conn: &Connection, sql: &str) -> Result<i64, Error> {
    let mut stmt = conn.prepare(sql).map_err(sqlite_err)?;
    let mut rows = stmt.query([]).map_err(sqlite_err)?;
    match rows.next().map_err(sqlite_err)? {
        Some(row) => row.get::<_, i64>(0).map_err(sqlite_err),
        None => Ok(0),
    }
}

fn sqlite_err(e: rusqlite::Error) -> Error {
    Error::Sqlite(e.to_string())
}

