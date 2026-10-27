//! Walk + read for the DSH session tree. One data unit: every session stream
//! (`session.jsonl.zstd`, or `session.v4.jsonl.zstd` for format v4) is a
//! whole-file unit (`FileKind::Tree`) decompressed and re-parsed on change.
//! Both formats bill per call from the stream itself — v4's
//! `assistant/message` usage made its projection cache redundant, and the
//! projection's lifetime-cumulative event mis-attributed every day after the
//! session's first (a session continued across days dragged its whole total
//! into the newest day).
//!
//! A resumed session is a NEW stream that replays its parent's events verbatim
//! (`"isSeeded": true` + `parentSession`, see the parser docs). When that
//! parent stream is still on disk it bills those calls itself, so the replay
//! is skipped; when it is gone the replay is the only copy and bills here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
    // A fork that replays a parent stream which still bills those calls itself:
    // every call up to (and including) the LAST `inherited` seed marker is the
    // replay. Those lines still seed the parser's session/model state (the
    // header is one of them), they just never become events.
    let replay_through = seeded_replay_end(&file.path, &decoded);
    let mut stream = parser::Stream::new();
    let mut events = Vec::new();
    for (index, line) in decoded.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let replayed = replay_through.is_some_and(|end| index <= end);
        // Inside the replay only the lines that carry parser state matter (the
        // session header and the model headers); every other line — including
        // transcript bodies of any size — is dropped without parsing.
        if replayed && !carries_parser_state(line) {
            continue;
        }
        let Ok(line) = std::str::from_utf8(line) else { continue };
        if let parser::Parsed::Event(event) = stream.line(line, &key) {
            if !replayed {
                events.push(*event);
            }
        }
    }
    Ok(ReadOutcome { events, cursor: ReadCursor(file.size) })
}

/// The two replayed lines the parser needs: the session header (identity,
/// project, format) and `request/header` (the model a call is billed to). The
/// `\"` after `session`/`request/header` is what keeps `session/end-seed` and
/// `session/title` out — they share the `type` prefix but carry no state.
const HEADER_PREFIX: &[u8] = b"{\"type\":\"session\"";
const MODEL_HEADER_PREFIX: &[u8] = b"{\"type\":\"request/header\"";
const END_SEED_PREFIX: &[u8] = b"{\"type\":\"session/end-seed\"";

fn carries_parser_state(line: &[u8]) -> bool {
    line.starts_with(HEADER_PREFIX) || line.starts_with(MODEL_HEADER_PREFIX)
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

/// The last line of the inherited replay, when this stream is a fork and its
/// parent still bills the same calls.
///
/// The writer marks a resumed session's header with `"isSeeded": true` and
/// `"parentSession": "<parent id>"`, then re-emits the parent's events verbatim
/// — so both files would bill the same calls. The parent keeps them when it is
/// still on disk (same workspace dir first, then the peer workspaces of every
/// root — measured 2026-10-27: all 170 forks had their parent in their own
/// workspace dir); a fork whose parent is gone is the only surviving copy of
/// that spend and bills normally.
///
/// A fork can carry SEVERAL `session/end-seed` markers with
/// `data.inherited: true` — one per resume of the fork — and the replay is
/// everything up to the last of them (measured 2026-10-27: fork `0f9b6ecd`'s 19
/// calls after its first marker were all present in the parent's stream, and
/// `35e1d11f` had 466 of 487). The type prefix screens marker lines out of the
/// transcript for free; only the survivors are parsed.
fn seeded_replay_end(stream_path: &Path, decoded: &[u8]) -> Option<usize> {
    let parent = seeded_parent(decoded)?;
    let mut end = None;
    for (index, line) in decoded.split(|b| *b == b'\n').enumerate() {
        if !line.starts_with(END_SEED_PREFIX) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        if v.pointer("/data/inherited").and_then(serde_json::Value::as_bool) == Some(true) {
            end = Some(index);
        }
    }
    end.filter(|_| parent_stream_path(stream_path, &parent).is_some())
}

/// `parentSession` off a v4 header carrying `"isSeeded": true`. The header is
/// the stream's first line; a torn or older stream answers `None`.
fn seeded_parent(decoded: &[u8]) -> Option<String> {
    let first = decoded.split(|b| *b == b'\n').find(|line| !line.is_empty())?;
    let v: serde_json::Value = serde_json::from_slice(first).ok()?;
    if v.get("type").and_then(serde_json::Value::as_str) != Some("session") {
        return None;
    }
    if v.get("isSeeded").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    v.get("parentSession")
        .and_then(serde_json::Value::as_str)
        // The id is spliced into a path below, so anything a directory name
        // could not carry (or that could climb out of one) is not a session.
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .map(str::to_string)
}

/// Where the parent's stream sits: `<workspace>/<session dir>/session[.v4].jsonl.zstd`.
/// `stream_path` is the child's own stream, so its workspace dir is two levels up.
/// The writer names a session dir either `session-<id>` (the desktop app) or the
/// bare id (its continuable subagents), so both spellings are tried.
fn parent_stream_path(stream_path: &Path, parent: &str) -> Option<PathBuf> {
    let workspace = stream_path.parent()?.parent()?;
    if let Some(found) = session_stream_in(workspace, parent) {
        return Some(found);
    }
    // A resumed session can be re-homed to another workspace directory; the
    // peer workspaces of every configured root are the cheap second guess.
    for root in paths::sessions_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || dir == workspace {
                continue;
            }
            if let Some(found) = session_stream_in(&dir, parent) {
                return Some(found);
            }
        }
    }
    None
}

fn session_stream_in(workspace: &Path, parent: &str) -> Option<PathBuf> {
    for dir in [workspace.join(format!("session-{parent}")), workspace.join(parent)] {
        for name in ["session.v4.jsonl.zstd", "session.jsonl.zstd"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
