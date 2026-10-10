//! Walk + read for the DSH session tree. One data unit: every session stream
//! (`session.jsonl.zstd`, or `session.v4.jsonl.zstd` for format v4) is a
//! whole-file unit (`FileKind::Tree`) decompressed and re-parsed on change.
//! Both formats bill per call from the stream itself — v4's
//! `assistant/message` usage made its projection cache redundant, and the
//! projection's lifetime-cumulative event mis-attributed every day after the
//! session's first (a session continued across days dragged its whole total
//! into the newest day).

use std::collections::HashSet;
use std::path::PathBuf;

use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, SourceFile};
use walkdir::WalkDir;

use crate::{parser, paths};


pub fn probe() -> Option<DetectedSource> {
    let roots = paths::sessions_roots();
    // One entry per session across every root: a mirrored session (same
    // workspace slug + session dir in two roots) counts once.
    let count = session_identities(&roots).len();
    (count > 0).then(|| DetectedSource {
        id: crate::TOOL_ID.to_string(),
        display: crate::DISPLAY_NAME.to_string(),
        roots,
        hint: Some(format!("{count} sessions")),
    })
}

pub fn discover(filter: &DateFilter) -> Vec<SourceFile> {
    let roots = paths::sessions_roots();
    if roots.is_empty() {
        return Vec::new();
    }
    // Every session bills from its stream, v3 and v4 alike. Mirrored trees
    // (the same identity in two roots) are read from the first root that
    // carries them, priority order.
    let identities = session_identities(&roots);
    let mut streams: Vec<SourceFile> = Vec::new();
    for identity in &identities {
        if let Some(f) = tree_file(identity.path.clone(), filter) {
            streams.push(f);
        }
    }
    streams.sort_by(|a, b| a.path.cmp(&b.path));
    streams
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

/// One session per identity: the workspace slug + session dir relative to its
/// root. Mirrored trees repeat identities, and the first root in priority
/// order wins — a mirrored session is read exactly once no matter how many
/// roots carry it.
struct Identity {
    path: PathBuf,
}

fn session_identities(roots: &[PathBuf]) -> Vec<Identity> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        for path in WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| paths::is_session_file(p))
        {
            let Ok(rel) = path.strip_prefix(root) else { continue };
            let key = rel.to_string_lossy().into_owned();
            if seen.insert(key) {
                out.push(Identity { path });
            }
        }
    }
    out
}
