//! Walk + read for the DSH session tree. Two data units, one source each:
//! every v3 `session.jsonl.zstd` is a whole-file unit (`FileKind::Tree`)
//! decompressed and re-parsed on change; every format-v4 session speaks
//! through its projection cache JSON instead (its stream stays header-only).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, SourceFile};
use walkdir::WalkDir;

use crate::{parser, paths, proj};


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
    // A v3 stream carries its session's per-call events; a v4 stream is
    // header-only and its session speaks through the projection below. The
    // projections of v3 sessions (which exist on the macOS layout) are never
    // read — a projection joins only when its own v4 stream exists, so a
    // session is billed from exactly one side on every platform. Mirrored
    // trees (the same identity in two roots) are read from the first root
    // that carries them, priority order.
    let identities = session_identities(&roots);
    let mut v4_sessions: HashSet<String> = HashSet::new();
    let mut streams: Vec<SourceFile> = Vec::new();
    for identity in &identities {
        match paths::is_v4_stream(&identity.path) {
            true => {
                // The id is the session DIRECTORY's name minus the `session-`
                // prefix — exactly what the projection file stem carries after
                // its own strip. The two sides must normalize identically or
                // the join silently drops every projection (this asymmetry
                // shipped once and hid all of Windows' v4 usage).
                if let Some(dir) = identity.path.parent().and_then(Path::file_name).and_then(|d| d.to_str()) {
                    let id = dir.strip_prefix("session-").unwrap_or(dir);
                    v4_sessions.insert(id.to_string());
                }
            }
            false => {
                if let Some(f) = tree_file(identity.path.clone(), filter) {
                    streams.push(f);
                }
            }
        }
    }
    let mut seen_projections: HashSet<String> = HashSet::new();
    let mut projections: Vec<SourceFile> = Vec::new();
    for dir in paths::projcache_dirs() {
        for path in WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let id = stem.strip_prefix("session-").unwrap_or(stem);
            // One projection per session id even when two roots mirror it.
            if !v4_sessions.contains(id) || !seen_projections.insert(id.to_string()) {
                continue;
            }
            if let Some(f) = tree_file(path, filter) {
                projections.push(f);
            }
        }
    }
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
