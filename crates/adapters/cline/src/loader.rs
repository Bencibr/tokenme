//! Walk + read for `~/.cline/data/sessions`.
//!
//! Transcripts are `FileKind::Tree`: Cline rewrites the whole object in place, so
//! `read` re-parses everything and idempotency comes from the dedupe keys, not
//! from the byte cursor. The cursor is still advanced to the file size so the
//! indexer's `unchanged_since` cheap check keeps working.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, SourceFile};
use walkdir::WalkDir;

use crate::parser::{self, Stats};
use crate::paths;

/// `<sessions>/<ts>_<id>/<file>.messages.json` — anything deeper is not Cline's.
const MAX_DEPTH: usize = 3;

/// Byte pre-gate: a transcript without a single `metrics` object holds nothing
/// billable, so it is never copied to str or turned into a DOM (sessions with
/// only user rows exist on this machine).
const METRICS_GATE: &[u8] = b"\"metrics\"";

pub fn probe() -> Option<DetectedSource> {
    let root = paths::sessions_root()?;
    // Short-circuits on the first readable transcript: this runs at startup.
    let first = transcripts(&root).next()?;
    Some(DetectedSource {
        id: crate::TOOL_ID.to_string(),
        display: crate::DISPLAY_NAME.to_string(),
        roots: vec![root],
        hint: Some(format!("sessions dir ({}…)", first.file_name().and_then(|n| n.to_str()).unwrap_or("?"))),
    })
}

fn transcripts(root: &Path) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(root)
        .follow_links(false)
        .max_depth(MAX_DEPTH)
        .into_iter()
        // depth 0 is the caller's root, which on macOS may itself be a `.tmp…`
        // directory — only descendants get pruned.
        .filter_entry(|e| e.depth() == 0 || !is_hidden(e.path()))
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| paths::is_messages_file(p))
}

pub fn discover(filter: &DateFilter) -> Vec<SourceFile> {
    let Some(root) = paths::sessions_root() else { return Vec::new() };
    let mut out = Vec::new();
    for path in transcripts(&root) {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() {
            continue;
        }
        // A transcript's timestamps live inside its records, so mtime is only a
        // cheap upper bound: a file last written before `until_ms` cannot hold a
        // newer call. `since_ms` cannot prune, because the accumulator keeps being
        // rewritten long after the older calls it contains.
        if filter.until_ms.is_some_and(|until| mtime_ms(&meta) > until) {
            continue;
        }
        out.push(SourceFile {
            path,
            kind: FileKind::Tree,
            size: meta.len(),
            mtime_ms: mtime_ms(&meta),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn is_hidden(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.') && n.len() > 1)
}

/// Re-read one transcript. Unreadable → empty outcome with the **unchanged**
/// cursor, so the indexer retries it on the next pass; `Err` is reserved for a
/// cursor past EOF, which can only mean the file shrank under a stale manifest.
pub fn read(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    if cursor.0 > file.size {
        return Err(Error::Cursor { path: file.path.clone(), cursor: cursor.0 });
    }
    // `Tree` means one whole-file parse per change; the DOM dies with this scope.
    let Ok(bytes) = std::fs::read(&file.path) else {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    };
    if bytes.is_empty() || !contains(&bytes, METRICS_GATE) {
        return Ok(ReadOutcome { events: Vec::new(), cursor: ReadCursor(file.size) });
    }
    let text = String::from_utf8_lossy(&bytes);
    let (events, _) = parse_transcript(&text, file);
    Ok(ReadOutcome { events, cursor: ReadCursor(file.size) })
}

/// Parse one transcript and hand back the wire census the smoke test diffs
/// against `metadata.usage`. Events are dropped here — `read` owns those.
#[cfg(test)]
pub fn stats_for(file: &SourceFile) -> Stats {
    let text = std::fs::read_to_string(&file.path).unwrap_or_default();
    let (_, stats) = parse_transcript(&text, file);
    stats
}

fn parse_transcript(text: &str, file: &SourceFile) -> (Vec<usage_core::UsageEvent>, Stats) {
    let session = paths::session_id(&file.path, None);
    let project = paths::project_label(&file.path);
    parser::parse_transcript(text, &session, project.as_deref(), &file.key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::TokenCounts;

    /// Env vars are process-global, so every test that points an adapter at a
    /// temp home holds this lock.
    #[allow(dead_code)]
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }


    fn fixture_file(name: &str) -> SourceFile {
        let path = fixture_path(name);
        let meta = std::fs::metadata(&path).unwrap();
        SourceFile { path, kind: FileKind::Tree, size: meta.len(), mtime_ms: mtime_ms(&meta) }
    }

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn tree_read_is_idempotent_and_freezes_on_unreadable() {
        let file = fixture_file("mixed-rows.messages.json");
        let first = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(first.events.len(), 5, "user / no-metrics / undated rows are not events");
        assert_eq!(first.cursor, ReadCursor(file.size));
        let again = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(again.events, first.events, "re-reading a rewritten transcript must not double it");
        assert_eq!(again.events[0].session, "3_ts_mixed", "the payload sessionId beats the file name");
        assert_eq!(again.events[0].dedupe_key.as_deref(), Some("3_ts_mixed#msg_ok"));
        assert_eq!(again.events[0].project.as_deref(), None, "the fixture dir has no session meta sibling");

        // A shrunken manifest with a stale cursor is the one real error.
        let shrunk = SourceFile { size: 4, ..file.clone() };
        assert!(matches!(read(&shrunk, ReadCursor(900)), Err(Error::Cursor { .. })));
        // Unreadable file: nothing learned, cursor frozen, no error.
        let missing = SourceFile { path: fixture_path("does-not-exist.messages.json"), ..file.clone() };
        let out = read(&missing, ReadCursor(7)).unwrap();
        assert!(out.events.is_empty());
        assert_eq!(out.cursor, ReadCursor(7));
    }

    #[test]
    fn a_transcript_without_metrics_is_gated_before_the_dom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("9_ts.messages.json");
        std::fs::write(&path, b"{\"sessionId\":\"9_ts\",\"messages\":[{\"id\":\"user_0\",\"role\":\"user\"}]}").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let file = SourceFile { path: path.clone(), kind: FileKind::Tree, size: meta.len(), mtime_ms: mtime_ms(&meta) };
        let out = read(&file, ReadCursor(0)).unwrap();
        assert!(out.events.is_empty(), "no metrics ⇒ nothing billable");
        assert_eq!(out.cursor, ReadCursor(file.size), "the file is still consumed");
        // The same file with one usage-bearing row crosses the gate.
        std::fs::write(&path, b"{\"messages\":[{\"id\":\"msg_1\",\"role\":\"assistant\",\"ts\":1788497041736,\"metrics\":{\"inputTokens\":9,\"outputTokens\":1,\"cacheReadTokens\":0,\"cacheWriteTokens\":0}}]}").unwrap();
        let grown = SourceFile { size: std::fs::metadata(&path).unwrap().len(), ..file };
        let out = read(&grown, ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].counts.input, 9.0);
        assert_eq!(out.events[0].ts_ms, 1_788_497_041_736);
    }

    #[test]
    fn a_rewritten_transcript_re_emits_the_same_rows() {
        // Two whole-file snapshots of the real transcript: the state before the
        // last call landed (95 assistant rows) and the file as it is now (96).
        // Every event of the first read must reappear unchanged, and only the one
        // new call may be added.
        let raw = std::fs::read_to_string(fixture_path("rewritten-twice.jsonl")).unwrap();
        let mut snapshots = raw.lines();
        let (snap1, snap2) = (snapshots.next().unwrap(), snapshots.next().unwrap());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("1787490736874_djat1")).unwrap();
        let live = dir.path().join("1787490736874_djat1.messages.json");
        std::fs::write(&live, snap1).unwrap();
        let meta = std::fs::metadata(&live).unwrap();
        let file = SourceFile { path: live, kind: FileKind::Tree, size: meta.len(), mtime_ms: mtime_ms(&meta) };
        let first = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(first.events.len(), 95);

        std::fs::write(&file.path, snap2).unwrap();
        let grown = SourceFile { size: snap2.len() as u64, ..file.clone() };
        let second = read(&grown, first.cursor).unwrap();
        assert_eq!(second.events.len(), 96, "the rewrite exposes exactly one new call");
        assert_eq!(second.events[..95], first.events[..], "already-seen rows keep their identity");
        let keys: std::collections::HashSet<_> = second.events.iter().map(|e| e.dedupe_key.clone()).collect();
        assert_eq!(keys.len(), 96, "no duplicated dedupe key: {:?}", second.events.iter().filter(|e| e.dedupe_key.is_none()).count());
        // Reading the same bytes again is a no-op, and the 1.9× double count only
        // happens if the session accumulator is read as well.
        let third = read(&grown, ReadCursor(0)).unwrap();
        assert_eq!(third.events, second.events);
        let totals: TokenCounts = second.events.iter().fold(TokenCounts::default(), |mut a, e| {
            a += &e.counts;
            a
        });
        assert_eq!(totals.cache_read, 11_093_248.0, "the cached prefix is counted once");
        assert_eq!(totals.input, 722_909.0, "…and only the uncached rest is billed as input");
        assert_eq!(totals.input + totals.cache_read, 11_816_157.0, "matches metadata.usage.inputTokens");
        assert_eq!(totals.cache_creation, 0.0);
        assert_eq!(totals.output, 56_286.0);
    }

    #[test]
    fn discover_walks_the_tree_without_touching_the_real_home() {
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("2_ts");
        std::fs::create_dir_all(sess.join(".hidden")).unwrap();
        std::fs::write(sess.join("2_ts.messages.json"), b"{\"messages\":[]}").unwrap();
        std::fs::write(sess.join(".hidden/3_ts.messages.json"), b"{\"messages\":[]}").unwrap();
        std::fs::write(sess.join("2_ts.json"), b"{}").unwrap();
        std::env::set_var(paths::ENV_SESSIONS_DIR, dir.path());
        let found = discover(&DateFilter::default());
        assert_eq!(found.len(), 1, "dot-dirs and meta files are skipped: {found:?}");
        assert_eq!(found[0].kind, FileKind::Tree);
        assert!(found[0].size > 0);
        assert_eq!(found[0].path.file_name().unwrap().to_str().unwrap(), "2_ts.messages.json");
        assert!(discover(&DateFilter::new(None, Some(0))).is_empty(), "until_ms prunes by mtime");
        assert!(probe().is_some(), "probe finds the same single transcript");
        std::env::remove_var(paths::ENV_SESSIONS_DIR);
        assert!(probe().is_some() || paths::sessions_root().is_none(), "probe still resolves the real profile after the override is dropped");
    }

    #[test]
    fn label_is_the_sibling_meta_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("1788497024441_2hl7c");
        std::fs::create_dir_all(&sess).unwrap();
        let msgs = sess.join("1788497024441_2hl7c.messages.json");
        std::fs::write(&msgs, std::fs::read(fixture_path("mixed-rows.messages.json")).unwrap()).unwrap();
        std::fs::write(sess.join("1788497024441_2hl7c.json"), br#"{"cwd":"/Users/me/workspace/tunnel","metadata":{"usage":{"inputTokens":999999}}}"#).unwrap();
        let meta = std::fs::metadata(&msgs).unwrap();
        let file = SourceFile { path: msgs, kind: FileKind::Tree, size: meta.len(), mtime_ms: mtime_ms(&meta) };
        let out = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 5);
        assert!(out.events.iter().all(|e| e.project.as_deref() == Some("tunnel")));
        assert!(out.events.iter().all(|e| e.session == "3_ts_mixed"), "the payload id wins over the directory name");
        assert_eq!(out.events.iter().map(|e| e.counts.input).sum::<f64>(), 6222.0 + 8000.0 + 3000.0 + 500.0 + 1200.0, "the meta accumulator is never mixed in");
    }
}
