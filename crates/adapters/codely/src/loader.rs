//! Walk + read for `~/.codely-cli/tmp`.
//!
//! Snapshots are `FileKind::Tree`: the CLI rewrites the whole object whenever a
//! turn lands, so `read` re-parses everything and idempotency comes from the
//! dedupe keys, never from a byte cursor. The cursor is still advanced to the
//! file size so the indexer's cheap `unchanged_since` check keeps working (and
//! a shrinking file is the one real `Cursor` error, exactly as in Cline).

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, SourceFile};

use crate::parser;
use crate::paths;

/// Byte pre-gate: a snapshot with no `tokens` object holds nothing billable.
const TOKENS_GATE: &[u8] = b"\"tokens\"";

pub(crate) fn probe() -> Option<DetectedSource> {
    let root = paths::tmp_root()?;
    let files = paths::chat_files();
    let first = files.first()?;
    Some(DetectedSource {
        id: crate::TOOL_ID.to_string(),
        display: crate::DISPLAY_NAME.to_string(),
        roots: vec![root],
        hint: Some(format!(
            "{} chat snapshots (starting {}…)",
            files.len(),
            first.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        )),
    })
}

pub(crate) fn discover(filter: &DateFilter) -> Vec<SourceFile> {
    paths::chat_files()
        .into_iter()
        .filter_map(source_file)
        .filter(|file| filter.until_ms.is_none_or(|until| file.mtime_ms <= until))
        .collect()
}

/// One snapshot as the indexer stats a source, or `None` when it vanished.
pub(crate) fn source_file(path: PathBuf) -> Option<SourceFile> {
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(SourceFile {
        path,
        kind: FileKind::Tree,
        size: meta.len(),
        mtime_ms: mtime_ms(&meta),
    })
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack.windows(needle.len()).any(|window| window == needle)
}

/// Re-read one snapshot. Unreadable → empty outcome with the unchanged cursor,
/// so the indexer retries it; `Err` is reserved for a cursor past EOF, which can
/// only mean the file shrank under a stale manifest.
pub(crate) fn read(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    if cursor.0 > file.size {
        return Err(Error::Cursor { path: file.path.clone(), cursor: cursor.0 });
    }
    let Ok(bytes) = std::fs::read(&file.path) else {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    };
    if bytes.is_empty() || !contains(&bytes, TOKENS_GATE) {
        return Ok(ReadOutcome { events: Vec::new(), cursor: ReadCursor(file.size) });
    }
    let text = String::from_utf8_lossy(&bytes);
    let (events, _) = parser::parse_transcript(&text, &file.key());
    Ok(ReadOutcome { events, cursor: ReadCursor(file.size) })
}

#[cfg(test)]
pub(crate) fn stats_for(file: &SourceFile) -> parser::Stats {
    let text = std::fs::read_to_string(&file.path).unwrap_or_default();
    let (_, stats) = parser::parse_transcript(&text, &file.key());
    stats
}
