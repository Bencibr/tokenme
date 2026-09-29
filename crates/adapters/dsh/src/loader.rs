//! Walk + read for the DSH session tree. Two data units, one source each:
//! every v3 `session.jsonl.zstd` is a whole-file unit (`FileKind::Tree`)
//! decompressed and re-parsed on change; every format-v4 session speaks
//! through its projection cache JSON instead (its stream stays header-only).

use std::path::PathBuf;

use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, SourceFile};
use walkdir::WalkDir;

use crate::{parser, paths, proj};


pub fn probe() -> Option<DetectedSource> {
    let sessions = paths::sessions_dir()?;
    let count = session_files(&sessions).chain(proj_files()).count();
    (count > 0).then(|| DetectedSource {
        id: crate::TOOL_ID.to_string(),
        display: crate::DISPLAY_NAME.to_string(),
        roots: vec![sessions],
        hint: Some(format!("{count} sessions")),
    })
}

pub fn discover(filter: &DateFilter) -> Vec<SourceFile> {
    let Some(sessions) = paths::sessions_dir() else { return Vec::new() };
    let mut streams: Vec<SourceFile> = session_files(&sessions)
        // v4 streams are header-only; their session speaks through its
        // projection below. Reading both would double-bill.
        .filter(|p| paths::is_v3_stream(p))
        .filter_map(|path| tree_file(path, filter))
        .collect();
    let mut projections: Vec<SourceFile> = proj_files()
        .filter_map(|path| tree_file(path, filter))
        .collect();
    let mut out = Vec::with_capacity(streams.len() + projections.len());
    out.append(&mut streams);
    out.append(&mut projections);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn tree_file(path: PathBuf, filter: &DateFilter) -> Option<SourceFile> {
    let meta = std::fs::metadata(&path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|age| age.as_millis() as i64)
        .unwrap_or(0);
    // Compressed bytes are an upper bound on the calls inside; the exact cut
    // happens on the event stream's own timestamps.
    if filter.until_ms.is_some_and(|until| mtime > until) {
        return None;
    }
    Some(SourceFile { path, kind: FileKind::Tree, size: meta.len(), mtime_ms: mtime })
}

pub fn read(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    if cursor.0 > file.size {
        return Err(Error::Cursor { path: file.path.clone(), cursor: cursor.0 });
    }
    if file.path.extension().and_then(|e| e.to_str()) == Some("json") {
        return read_projection(file);
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
            events.push(*event);
        }
    }
    Ok(ReadOutcome { events, cursor: ReadCursor(file.size) })
}

/// One cumulative event per projection, replaced on the stable `#proj` key
/// every time the file changes — the same replace-on-key contract the
/// hermes cumulative slices use.
fn read_projection(file: &SourceFile) -> Result<ReadOutcome, Error> {
    let empty = ReadOutcome { events: Vec::new(), cursor: ReadCursor(file.size) };
    let Ok(text) = std::fs::read_to_string(&file.path) else {
        return Ok(empty);
    };
    // `session-<id>.json` → `<id>`.
    let stem = file.path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let session = stem.strip_prefix("session-").unwrap_or(stem);
    let mut events = Vec::new();
    if let Some(event) = proj::parse(&text, session, file.mtime_ms, &file.key()) {
        events.push(*event);
    }
    Ok(ReadOutcome { events, cursor: ReadCursor(file.size) })
}

fn session_files(sessions: &PathBuf) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(sessions).into_iter().filter_map(|e| e.ok()).map(|e| e.into_path()).filter(|p| paths::is_session_file(p))
}

/// The projection files, discovered independently: the dir may not exist at
/// all on a macOS-only v3 machine.
fn proj_files() -> impl Iterator<Item = PathBuf> {
    let dir = paths::projcache_dir();
    dir.into_iter()
        .flat_map(WalkDir::new)
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
}
