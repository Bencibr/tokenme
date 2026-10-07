//! One session transcript → the calls it billed.
//!
//! The whole file is re-read whenever it changes, which is what [`FileKind::Tree`]
//! means in this codebase, and for MiniMax Code it is not a luxury: the history is
//! a *materialised* artifact rather than an append log. The vendor writes it through
//! a temporary file and renames it over the old one
//! (`…/history/canonical-history-materializer.ts`, `session-history-paths.ts`
//! `writeManifest`), and the catalog beside it records content-addressed revisions
//! — on this machine `history-catalog.json` holds
//! `{"activeRevision":"sha256:86c063a5…","generation":0,"artifacts":[{"fileName":
//! "messages.jsonl","byteLength":227734,"messageCount":40}]}`. Add the rewind
//! service (`messages/rewind/transaction.ts`) and the compaction repair
//! (`messages/stale-compaction-repair.ts`) and a byte cursor would happily resume
//! into a file whose earlier bytes are no longer the records they were. Re-reading
//! is only safe because every event carries an identity the index dedupes on.

use std::collections::HashMap;
use std::path::PathBuf;

use usage_core::{Meter, ReadCursor, ReadOutcome, SourceFile, UsageEvent};

use crate::parser::{self, Parsed};
use crate::paths;
use crate::TOOL_ID;

/// Reads one transcript from the start. The cursor is passed through untouched: a
/// `Tree` source has none, and the indexer stops after a single call.
pub(crate) fn read_file(file: &SourceFile, cursor: ReadCursor) -> ReadOutcome {
    let workspaces = paths::workspaces();
    match std::fs::read(&file.path) {
        Err(_) => ReadOutcome { events: Vec::new(), cursor },
        Ok(bytes) => ReadOutcome { events: events(&bytes, file, &workspaces), cursor },
    }
}

/// Complete lines only: a trailing fragment is a record the app is still writing,
/// and half a JSON object would be dropped as malformed anyway.
fn events(bytes: &[u8], file: &SourceFile, workspaces: &HashMap<String, String>) -> Vec<UsageEvent> {
    let finished = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(last) => last + 1,
        None => return Vec::new(),
    };
    let session = paths::session_id(&file.path);
    // The app labels a session by the workspace it ran in, which its own store
    // resolves from the session id. No row, no label — never a lost event.
    let project = workspaces.get(&session).cloned();
    let mut out = Vec::new();
    for (ordinal, raw) in bytes[..finished].split(|b| *b == b'\n').enumerate() {
        if raw.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(raw) else { continue };
        let Some(parsed) = parser::parse_line(text) else { continue };
        out.push(event(parsed, file, &session, project.as_deref(), ordinal));
    }
    out
}

fn event(
    parsed: Parsed,
    file: &SourceFile,
    session: &str,
    project: Option<&str>,
    ordinal: usize,
) -> UsageEvent {
    // A record with no stamp of its own is filed under the file's last write, the
    // same rule every other adapter uses: epoch 0 would invent a 1970 bucket.
    let ts_ms = parsed.time_ms.filter(|ms| *ms > 0).unwrap_or(file.mtime_ms).max(0);
    let mut event = UsageEvent::new(TOOL_ID, ts_ms, session.to_string());
    event.project = project.map(str::to_string);
    event.model = parsed.model.clone();
    event.meter = Meter::Tokens;
    event.counts = parsed.counts;
    event.dedupe_key = Some(key(&parsed, file, session, ordinal));
    event.source = file.key();
    event
}

/// The identity a replay is absorbed by.
///
/// Three shapes, best evidence first: the record's own `message_id`; otherwise the
/// turn plus the stamp, the model and all four stages, which is the tuple a second
/// call would have to match by millisecond; otherwise, for a record with neither,
/// the position in the file.
///
/// Deliberately **not** in the first shape: the session. The product can hand a
/// running session to another device (`v2/session-handoff/`, and the host serves
/// `/api/v1/session-transfer/…`), and that copy arrives with a new session id
/// around the same messages. A session-prefixed key would bill the transferred
/// history twice; the message id alone collapses it.
fn key(parsed: &Parsed, file: &SourceFile, session: &str, ordinal: usize) -> String {
    if let Some(id) = parsed.message_id.as_deref() {
        return format!("{TOOL_ID}#{id}");
    }
    let c = &parsed.counts;
    match parsed.time_ms.filter(|ms| *ms > 0) {
        Some(ms) => {
            let [input, creation, read, output] = stages(c.input, c.cache_creation, c.cache_read, c.output);
            format!(
                "{TOOL_ID}#{}#{}#{ms}#{}#{input}#{creation}#{read}#{output}",
                session,
                parsed.turn_id.as_deref().unwrap_or("-"),
                parsed.model.as_deref().unwrap_or("-"),
            )
        }
        // A record with no stamp cannot be told apart from its twin by content, so
        // the only identity left is where it sits.
        None => format!("{TOOL_ID}#{session}#{}#{ordinal}", file.path.display()),
    }
}

/// A stage tuple that survives a float formatting change in the middle of a key
/// space already written into the index: an integer tuple is stable.
fn stages(input: f64, cache_creation: f64, cache_read: f64, output: f64) -> [u64; 4] {
    [
        input.max(0.0).round() as u64,
        cache_creation.max(0.0).round() as u64,
        cache_read.max(0.0).round() as u64,
        output.max(0.0).round() as u64,
    ]
}

/// `discover`'s per-file step: a transcript that cannot be stat'd is not a source.
pub(crate) fn source_file(path: PathBuf) -> Option<SourceFile> {
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(SourceFile { path, kind: usage_core::FileKind::Tree, size: meta.len(), mtime_ms })
}
