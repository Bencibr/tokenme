//! Walk + read for the DSH session tree: every `session.jsonl.zstd` is one
//! whole-file unit (`FileKind::Tree`), decompressed and re-parsed on change.

use std::path::PathBuf;

use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, SourceFile};
use walkdir::WalkDir;

use crate::{parser, paths};


pub fn probe() -> Option<DetectedSource> {
    let sessions = paths::sessions_dir()?;
    let count = session_files(&sessions).count();
    (count > 0).then(|| DetectedSource {
        id: crate::TOOL_ID.to_string(),
        display: crate::DISPLAY_NAME.to_string(),
        roots: vec![sessions],
        hint: Some(format!("{count} sessions")),
    })
}

pub fn discover(filter: &DateFilter) -> Vec<SourceFile> {
    let Some(sessions) = paths::sessions_dir() else { return Vec::new() };
    let mut out: Vec<SourceFile> = session_files(&sessions)
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|age| age.as_millis() as i64)
                .unwrap_or(0);
            // Compressed bytes are an upper bound on the calls inside; the
            // exact cut happens on the event stream's own timestamps.
            if filter.until_ms.is_some_and(|until| mtime > until) {
                return None;
            }
            Some(SourceFile { path, kind: FileKind::Tree, size: meta.len(), mtime_ms: mtime })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub fn read(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    if cursor.0 > file.size {
        return Err(Error::Cursor { path: file.path.clone(), cursor: cursor.0 });
    }
    // `Tree` means one whole-file decompress per change; re-emitted events
    // re-enter the dedupe index under stable keys.
    let Ok(bytes) = std::fs::read(&file.path) else {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    };
    let Ok(decoded) = zstd::stream::decode_all(&bytes[..]) else {
        // A torn tail (the writer is mid-record) reads as empty this pass;
        // the next refresh re-reads the file once it grows.
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    };
    let key = file.key();
    let mut stream = parser::Stream::new();
    let mut events = Vec::new();
    for line in decoded.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(line) = std::str::from_utf8(line) else { continue };
        if let parser::Parsed::Event(event) = stream.line(line, &key) {
            events.push(event);
        }
    }
    Ok(ReadOutcome { events, cursor: ReadCursor(file.size) })
}

fn session_files(sessions: &PathBuf) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(sessions).into_iter().filter_map(|e| e.ok()).map(|e| e.into_path()).filter(|p| {
        paths::is_session_file(&p)
    })
}
