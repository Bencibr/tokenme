//! One conversation database → the events it bills.
//!
//! Three streamed queries and a merge: the conversation-level blob, the `steps`
//! rows that date a turn when the generation record does not, then every
//! `gen_metadata` row in `idx` order. Only the small per-turn summary is held,
//! so the largest local database (1.6 MB, 73 generations) costs the same shape
//! as one with a hundred thousand rows, and no blob outlives its own row.

use std::collections::{BTreeSet, HashMap};

use rusqlite::Connection;
use usage_core::{Meter, ReadCursor, ReadOutcome, SourceFile, TokenCounts, UsageEvent};

use crate::parser::{
    conversation_meta, counts_for, decode_generation, step_stamp, Conversation, Generation,
    Lifetime, Skip, Stages, StepStamp,
};
use crate::paths;
use crate::TOOL_ID;

/// One row per generation, in the only order the table defines.
const SELECT_GENS: &str = "SELECT idx, data FROM gen_metadata ORDER BY idx";
/// Model turns: the fallback source of a wall-clock stamp.
const SELECT_STEPS: &str = "SELECT metadata FROM steps WHERE step_type = 15 AND metadata IS NOT NULL";
/// One row per conversation: its created-at and its workspace folder.
const SELECT_BLOB: &str = "SELECT data FROM trajectory_metadata_blob LIMIT 1";

/// Why one row produced no event, with the byte offset where the decoder knew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRow {
    pub source: String,
    pub idx: i64,
    pub reason: &'static str,
    /// Where the wire scan stopped, for `truncated` and `out-of-range`.
    pub offset: Option<usize>,
}

/// Per-field counters that keep the field mapping auditable against the wire.
///
/// `read` fills one on every pass, so the numbers a review needs come out of the
/// same code path the app runs rather than a second decoder that could disagree.
#[derive(Debug, Default, Clone)]
pub struct Audit {
    /// Databases opened successfully.
    pub dbs: usize,
    /// Files that yielded nothing: neither open could serve them, or they hold no
    /// `gen_metadata` table at all.
    pub unusable: usize,
    /// `gen_metadata` rows seen.
    pub rows: usize,
    /// Rows that decoded into, or merged into, a billable turn.
    pub decoded: usize,
    /// Rows folded into an already-seen responseId.
    pub merged: usize,
    /// `#17` attempt boxes billed as a call of their own — the ones whose
    /// responseId differs from the response they belong to.
    pub retries: usize,
    pub skipped: Vec<SkippedRow>,
    pub models: BTreeSet<String>,
    /// Sum of each raw stage over every decoded row, i.e. what the blobs say.
    pub raw: Stages,
    /// Sum of the stages as the emitted events carry them.
    pub counts: TokenCounts,
    /// Rows where `#4.3` did not equal `#4.9 + #4.10`.
    pub split_mismatches: usize,
    /// Turns with no `#19`, i.e. billable but unpriced.
    pub unmodelled: usize,
}

impl Audit {
    fn observe(&mut self, gen: &Generation) {
        let stages = &gen.stages;
        self.raw.prefix += stages.prefix;
        self.raw.input += stages.input;
        self.raw.total_output += stages.total_output;
        self.raw.cache_read += stages.cache_read;
        self.raw.out_a += stages.out_a;
        self.raw.out_b += stages.out_b;
        if stages.total_output > 0 && stages.total_output != stages.out_a + stages.out_b {
            self.split_mismatches += 1;
        }
        match &gen.model {
            Some(model) => {
                self.models.insert(model.clone());
            }
            None => self.unmodelled += 1,
        }
    }

    /// `Σ (#4.2 + #4.5 + #4.3)` over every decoded row — the left-hand side of
    /// the totals identity the smoke test prints against [`Audit::counts`].
    /// Exact by construction: a row with any stage above `1e12` is declined in
    /// [`crate::parser`] before it can reach a `u64` → `f64` conversion.
    pub fn raw_grand_total(&self) -> f64 {
        (self.raw.input + self.raw.cache_read + self.raw.total_output) as f64
    }

    /// `Σ input + cache_read + output` over the events actually emitted.
    pub fn event_grand_total(&self) -> f64 {
        self.counts.input + self.counts.cache_read + self.counts.output
    }
}

/// The window over one database's rows, keyed by dedupe identity.
#[derive(Default)]
struct Window {
    next_order: usize,
    turns: HashMap<String, Bucket>,
}

struct Bucket {
    order: usize,
    stages: Stages,
    model: Option<String>,
    ts_ms: i64,
}

impl Window {
    /// Returns whether this row joined a turn already in the window.
    fn consume(&mut self, gen: &Generation, key: String, ts_ms: i64) -> bool {
        match self.turns.get_mut(&key) {
            Some(bucket) => {
                // Streamed partials of one response each report a snapshot of the
                // same call, so the largest value per stage is the finished count
                // and a sum would invent spend — the rule Claude Code needs too.
                bucket.stages = bucket.stages.max_each(gen.stages);
                if bucket.model.is_none() {
                    bucket.model = gen.model.clone();
                }
                true
            }
            None => {
                self.turns.insert(
                    key,
                    Bucket { order: self.next_order, stages: gen.stages, model: gen.model.clone(), ts_ms },
                );
                self.next_order += 1;
                false
            }
        }
    }

    fn finish(self, session: &str, project: Option<&str>, file: &SourceFile) -> Vec<UsageEvent> {
        let mut buckets: Vec<(String, Bucket)> = self.turns.into_iter().collect();
        // `idx` order is the only chronology the table defines.
        buckets.sort_by_key(|(_, bucket)| bucket.order);
        let mut events = Vec::with_capacity(buckets.len());
        for (key, bucket) in buckets {
            // Defensive: a row that merged away to nothing must not mint an empty
            // event beside its real neighbours.
            let Some(counts) = counts_for(&bucket.stages) else { continue };
            let mut event = UsageEvent::new(TOOL_ID, bucket.ts_ms, session.to_string());
            event.project = project.map(str::to_string);
            event.model = bucket.model;
            event.counts = counts;
            event.meter = Meter::Tokens;
            event.dedupe_key = Some(format!("{session}#{key}"));
            event.source = file.key();
            events.push(event);
        }
        events
    }
}

/// Reads one conversation database wholesale. Idempotent through `dedupe_key`,
/// which is why a `Tree` file is re-read in full whenever it changes instead of
/// being resumed from a rowid cursor.
pub(crate) fn read_db(file: &SourceFile, audit: &mut Audit) -> ReadOutcome {
    let session = session_of(file);
    let Some(db) = paths::open(&file.path) else {
        // Missing, locked or mid-write: report nothing and leave the file for the
        // next pass, rather than aborting the whole ingest run.
        audit.unusable += 1;
        return ReadOutcome::default();
    };
    audit.dbs += 1;
    let conn = db.conn();
    let conv = conversation(conn);
    let stamps = step_stamps(conn);
    let lifetime = Lifetime::new(conv.created_ms, scan_ms().max(file.mtime_ms));
    let mut window = Window::default();
    // A database without `gen_metadata` is not an Antigravity CLI database.
    if let Ok(mut stmt) = conn.prepare_cached(SELECT_GENS) {
        if let Ok(rows) = stmt.query_map([], take_row) {
            for row in rows {
                // A row that fails mid-query is `SQLITE_BUSY` from the CLI
                // checkpointing: stop with what we have; the next pass re-reads
                // wholesale and the dedupe keys make that harmless. A row whose
                // blob is simply NULL is not such a row, and must not end the
                // pass — it is a generation the CLI never wrote any metadata for.
                let Ok(row) = row else { break };
                let Some((idx, blob)) = row else { continue };
                audit.rows += 1;
                let decoded = match decode_generation(idx, &blob, lifetime) {
                    Ok(decoded) => decoded,
                    Err(skip) => {
                        let (reason, offset) = describe(&skip);
                        audit.skipped.push(SkippedRow { source: session.clone(), idx, reason, offset });
                        continue;
                    }
                };
                audit.decoded += 1;
                audit.retries += decoded.retries.len();
                for gen in std::iter::once(&decoded.generation).chain(&decoded.retries) {
                    audit.observe(gen);
                    // The responseId is the identity the source gives one API call, so
                    // it survives a rewrite of the row. Every billable local row
                    // carries one; a row without it still gets a *stable* key, because
                    // `idx` is the integer primary key, so re-reading cannot mint a
                    // second copy of the same turn.
                    let key = gen.response_id.clone().unwrap_or_else(|| format!("gen{}", gen.idx));
                    let ts_ms = timestamp_for(gen, &stamps, &conv, file);
                    if window.consume(gen, key, ts_ms) {
                        audit.merged += 1;
                    }
                }
            }
        }
    }

    let events = window.finish(&session, conv.project.as_deref(), file);
    for event in &events {
        audit.counts += &event.counts;
    }
    // A `Tree` file has no cursor: the whole file is the unit, and the indexer
    // stops after one call because this never advances.
    ReadOutcome { events, cursor: ReadCursor(0) }
}

fn take_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<(i64, Vec<u8>)>> {
    let idx: i64 = row.get::<_, Option<i64>>(0)?.unwrap_or(0);
    match row.get::<_, Option<Vec<u8>>>(1)? {
        Some(blob) if !blob.is_empty() => Ok(Some((idx, blob))),
        _ => Ok(None),
    }
}

fn describe(skip: &Skip) -> (&'static str, Option<usize>) {
    match skip {
        Skip::NoChatModel => ("no-chat-model", None),
        Skip::NotBillable => ("no-usage", None),
        Skip::Truncated(offset) => ("truncated", Some(*offset)),
        Skip::OutOfRange(offset) => ("out-of-range", Some(*offset)),
    }
}

/// The conversation id: the file stem, which is also what `trajectory_meta`
/// repeats in its `conversation_id` column.
fn session_of(file: &SourceFile) -> String {
    file.path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn conversation(conn: &Connection) -> Conversation {
    match conn.query_row(SELECT_BLOB, [], |row| row.get::<_, Option<Vec<u8>>>(0)) {
        Ok(Some(blob)) => conversation_meta(&blob),
        _ => Conversation::default(),
    }
}

/// Per-turn stamps from `steps`, indexed by both join keys the source writes.
fn step_stamps(conn: &Connection) -> Stamps {
    let mut out = Stamps::default();
    let Ok(mut stmt) = conn.prepare_cached(SELECT_STEPS) else { return out };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, Option<Vec<u8>>>(0)) else { return out };
    for row in rows {
        let Ok(Some(blob)) = row else { continue };
        if let Some(StepStamp { ts_ms, response_id, gen_idx }) = step_stamp(&blob) {
            if let Some(id) = response_id {
                out.by_response.insert(id, ts_ms);
            }
            if let Some(idx) = gen_idx {
                out.by_gen.insert(idx, ts_ms);
            }
        }
    }
    out
}

#[derive(Default)]
struct Stamps {
    by_response: HashMap<String, i64>,
    by_gen: HashMap<i64, i64>,
}

/// A turn's timestamp, most-evidenced first: its own explicit stamp, then the
/// `steps` row for the same responseId, then for the same gen index, then the
/// lifetime-gated inference, then the conversation's created-at, then the file's
/// mtime. Never 0, so no record lands in a 1970 bucket.
///
/// `steps` outranks the inference on purpose: a `steps` row is an explicitly
/// typed Timestamp read off a real table, whereas the inference is a guess about
/// eight bytes no source documents.
fn timestamp_for(gen: &Generation, stamps: &Stamps, conv: &Conversation, file: &SourceFile) -> i64 {
    let by_response = gen.response_id.as_deref().and_then(|id| stamps.by_response.get(id));
    let by_gen = stamps.by_gen.get(&gen.idx);
    gen.own_timestamp
        .or(by_response.copied())
        .or(by_gen.copied())
        .or(gen.inferred_timestamp)
        .or(conv.created_ms)
        .or((file.mtime_ms > 0).then_some(file.mtime_ms))
        .unwrap_or_else(scan_ms)
}

fn scan_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_label_is_the_databases_own_uuid() {
        let file = source_file("/h/.gemini/antigravity-cli/conversations/292391de-b992-4c.db");
        assert_eq!(session_of(&file), "292391de-b992-4c");
        assert_eq!(session_of(&source_file("/h/no-name")), "no-name");
    }

    #[test]
    fn skip_reasons_carry_the_offset_only_where_one_exists() {
        assert_eq!(describe(&Skip::NoChatModel), ("no-chat-model", None));
        assert_eq!(describe(&Skip::NotBillable), ("no-usage", None));
        assert_eq!(describe(&Skip::Truncated(41)), ("truncated", Some(41)));
        assert_eq!(describe(&Skip::OutOfRange(3)), ("out-of-range", Some(3)));
    }

    #[test]
    fn a_missing_table_or_database_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.db");
        let mut audit = Audit::default();
        let out = read_db(&source_file(&absent.to_string_lossy()), &mut audit);
        assert!(out.events.is_empty());
        assert_eq!(out.cursor, ReadCursor(0));
        assert_eq!(audit.unusable, 1);

        // A real SQLite file with none of these tables: readable, unbillable.
        let other = dir.path().join("other.db");
        Connection::open(&other).unwrap().execute_batch("CREATE TABLE unrelated (x)").unwrap();
        let mut audit = Audit::default();
        let out = read_db(&source_file(&other.to_string_lossy()), &mut audit);
        assert!(out.events.is_empty(), "no gen_metadata means no events");
        assert_eq!((audit.dbs, audit.unusable), (0, 1), "another tool's database is not ours");
    }

    fn source_file(path: &str) -> SourceFile {
        SourceFile {
            path: std::path::PathBuf::from(path),
            kind: usage_core::FileKind::Tree,
            size: 1,
            mtime_ms: 1_786_402_449_758,
        }
    }
}
