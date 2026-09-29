//! Walk + read for both ZCode layouts, with exactly one of them active.
//!
//! `discover` hands the indexer the sqlite db when it exists and the rollout
//! JSONL files only when it does not: every rollout record also lives in
//! `model_usage`, so emitting from both would double-bill the overlap (the id
//! spaces differ, so the dedupe index cannot catch it — see [`crate::paths`]).

use std::io::{BufRead, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, SourceFile};
use walkdir::WalkDir;

use crate::parser;
use crate::paths;

/// Line buffer ceiling: a rollout record embeds a whole request/response and has
/// been seen at 300 KB, so the reader must never assume a shorter one.
const READ_BUF: usize = 256 * 1024;

pub fn probe() -> Option<DetectedSource> {
    let cli = paths::cli_dir()?;
    // Short-circuits: one stat, else the first entry of one directory.
    if let Some(db) = paths::db_path() {
        return Some(DetectedSource {
            id: crate::TOOL_ID.to_string(),
            display: crate::DISPLAY_NAME.to_string(),
            roots: vec![cli],
            hint: Some(format!("model_usage db ({})", human_size(&db))),
        });
    }
    let rollout = paths::rollout_dir()?;
    let first = rollout_entries(&rollout).next()?;
    Some(DetectedSource {
        id: crate::TOOL_ID.to_string(),
        display: crate::DISPLAY_NAME.to_string(),
        roots: vec![cli],
        hint: Some(format!("rollout ({}…)", first.file_name().and_then(|n| n.to_str()).unwrap_or("?"))),
    })
}

pub fn discover(filter: &DateFilter) -> Vec<SourceFile> {
    let mut out = Vec::new();
    if let Some(db) = paths::db_path() {
        // The db is one append-only file for every session, so `DateFilter` can
        // only prune it by mtime; the exact cut happens on the event stream.
        push_file(&mut out, &db, FileKind::Sqlite, filter);
        return out;
    }
    let Some(rollout) = paths::rollout_dir() else { return out };
    for path in rollout_entries(&rollout) {
        push_file(&mut out, &path, FileKind::Jsonl, filter);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub fn read(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    match file.kind {
        FileKind::Sqlite => crate::db::read(&file.path, cursor, &file.key()).map(|(outcome, _)| outcome),
        _ => read_rollout(file, cursor),
    }
}

/// Sequential JSONL read of one rollout file: `[cursor, last complete line]`.
fn read_rollout(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    let key = file.key();
    let mut file_handle = match std::fs::File::open(&file.path) {
        Ok(f) => f,
        Err(_) => return Ok(ReadOutcome { events: Vec::new(), cursor }),
    };
    let len = match file_handle.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Ok(ReadOutcome { events: Vec::new(), cursor }),
    };
    if cursor.0 > len {
        return Err(Error::Cursor { path: file.path.clone(), cursor: cursor.0 });
    }
    if cursor.0 == len {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    }
    if file_handle.seek(SeekFrom::Start(cursor.0)).is_err() {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    }
    let fallback = paths::session_from_file_name(&file.path);
    let mut reader = BufReader::with_capacity(READ_BUF, (&mut file_handle).take(len - cursor.0));
    let mut events = Vec::new();
    let mut consumed = 0_u64;
    let mut buf = Vec::with_capacity(16 * 1024);
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if buf.last() != Some(&b'\n') {
            // Torn tail: the writer is mid-line. Stop before it and re-read it
            // next pass, so the cursor only ever lands on a line boundary.
            break;
        }
        consumed += n as u64;
        buf.pop();
        // Records average 200 KB, so the pre-gate runs on bytes: only usage lines
        // carry this key, and only they get copied to str and parsed.
        if !contains(&buf, b"providerMetadata") {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        if let Some(event) = parser::event_from_line(&text, fallback.as_deref(), &key) {
            events.push(event);
        }
    }
    Ok(ReadOutcome {
        events,
        cursor: ReadCursor(cursor.0 + consumed),
    })
}

/// Substring test on bytes, so a non-usage line is never copied or parsed.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

fn rollout_entries(dir: &Path) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(dir)
        .max_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .map(|e| e.into_path())
        .filter(|p| paths::is_rollout_file(p))
}

fn push_file(out: &mut Vec<SourceFile>, path: &Path, kind: FileKind, filter: &DateFilter) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if !meta.is_file() {
        return;
    }
    let mtime = mtime_ms(&meta);
    // Both trees carry per-call timestamps inside the records, so mtime is only a
    // cheap upper bound: a file written before `until_ms` cannot hold a newer call.
    if filter.until_ms.is_some_and(|until| mtime > until) {
        return;
    }
    out.push(SourceFile {
        path: path.to_path_buf(),
        kind,
        size: meta.len(),
        mtime_ms: mtime,
    });
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn human_size(path: &Path) -> String {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::lock_env;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn jsonl_file(path: PathBuf) -> SourceFile {
        let meta = std::fs::metadata(&path).unwrap();
        SourceFile { path, kind: FileKind::Jsonl, size: meta.len(), mtime_ms: mtime_ms(&meta) }
    }

    #[test]
    fn rollout_read_advances_exactly_to_the_last_newline() {
        let file = jsonl_file(fixture("rollout-real.jsonl"));
        let out = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 8, "10 lines: 8 calls, 1 non-usage record, 1 torn line");
        let body = std::fs::read_to_string(fixture("rollout-real.jsonl")).unwrap();
        let torn = body.rsplit('\n').next().unwrap();
        assert!(!torn.is_empty() && !torn.trim_end().ends_with('}'), "the fixture must end mid-record: {torn:?}");
        assert_eq!(out.cursor, ReadCursor(body.len() as u64 - torn.len() as u64), "the torn tail stays unread; the cursor stops on a line boundary");
        assert_eq!(out.events[0].session, "sess_7de7ea43-96fe-4efe-9bdb-4afd85127203");
        assert_eq!(out.events[0].dedupe_key.as_deref(), Some("sess_7de7ea43-96fe-4efe-9bdb-4afd85127203#b0df2753-2722-4c18-a743-748c9c3fbdca#1"));
        // Resuming at the returned cursor learns nothing new and moves nowhere.
        let again = read(&file, out.cursor).unwrap();
        assert!(again.events.is_empty());
        assert_eq!(again.cursor, out.cursor);
        // A cursor past EOF is the one error; a torn tail is not.
        // The cursor is checked against the file's real length, not the possibly
        // stale manifest size, so a grown file never trips it.
        assert!(matches!(read(&file, ReadCursor(file.size + 1)), Err(Error::Cursor { .. })));
        let gone = SourceFile { path: fixture("not-here.jsonl"), ..file.clone() };
        let frozen = read(&gone, ReadCursor(42)).unwrap();
        assert!(frozen.events.is_empty() && frozen.cursor == ReadCursor(42));
    }

    #[test]
    fn every_line_of_a_file_is_read_even_without_a_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-io-sess_no_newline.jsonl");
        let body = std::fs::read_to_string(fixture("rollout-real.jsonl")).unwrap();
        let complete = body.lines().take(2).collect::<Vec<_>>().join("\n");
        std::fs::write(&path, &complete).unwrap();
        let file = jsonl_file(path.clone());
        let out = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 1, "line 1 is complete, line 2 has no newline yet");
        assert_eq!(out.cursor, ReadCursor(complete.find('\n').unwrap() as u64 + 1));
        // The writer finishes the line; the next pass picks it up.
        std::fs::write(&path, format!("{complete}\n")).unwrap();
        let grown = SourceFile { size: complete.len() as u64 + 1, ..file.clone() };
        let out2 = read(&grown, out.cursor).unwrap();
        assert_eq!(out2.events.len(), 1, "the second line is read once it is complete");
        assert_ne!(out2.events[0].dedupe_key, out.events[0].dedupe_key);
        assert_eq!(out2.cursor, ReadCursor(grown.size));
    }

    #[test]
    fn session_id_falls_back_to_the_file_name_of_both_layouts() {
        let body: Vec<_> = std::fs::read_to_string(fixture("rollout-subagent.jsonl")).unwrap().lines().map(str::to_string).collect();
        // A record with its `sessionId` stripped still attributes to the session
        // the file is named after.
        let trimmed = body[0].replace("\"sessionId\":\"", "\"xx\":\"");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-io-sess_subagent_agent_bd60e736-e6f4-495f-bb9d-6458cdc9db73.jsonl");
        std::fs::write(&path, format!("{trimmed}\n")).unwrap();
        let out = read(&jsonl_file(path), ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].session, "sess_subagent_agent_bd60e736-e6f4-495f-bb9d-6458cdc9db73");
        assert!(out.events[0].source.ends_with("model-io-sess_subagent_agent_bd60e736-e6f4-495f-bb9d-6458cdc9db73.jsonl"), "source is the manifest key: {:?}", out.events[0].source);
    }

    /// Why exactly one tree may be active: the same call is in both, and the two
    /// id spaces do not overlap, so the dedupe index cannot catch the double count.
    #[test]
    fn the_db_and_the_rollout_agree_on_the_same_call() {
        let rollout = read(&jsonl_file(fixture("rollout-real.jsonl")), ReadCursor(0)).unwrap().events;
        let (outcome, census) = crate::db::read(&fixture("model_usage.sqlite"), ReadCursor(0), "db").unwrap();
        let db = outcome.events;
        let last = rollout.last().unwrap();
        assert_eq!((last.counts.input, last.counts.cache_read, last.counts.output), (392.0, 385_408.0, 54.0));
        let same = db
            .iter()
            .find(|e| e.session == last.session && (e.ts_ms - last.ts_ms).abs() < 5_000)
            .expect("the db carries the rollout's call too");
        assert_eq!(same.counts, last.counts, "one call, one bill, whichever tree is read");
        assert_ne!(same.dedupe_key, last.dedupe_key, "different id spaces: reading both trees would double-bill");
        assert_eq!(census.events, 8);
    }

    #[test]
    fn the_db_is_the_active_source_and_the_rollout_is_the_fallback() {
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("cli");
        std::fs::create_dir_all(cli.join("rollout")).unwrap();
        std::fs::create_dir_all(cli.join("db")).unwrap();
        std::fs::write(cli.join("rollout/model-io-sess_a.jsonl"), std::fs::read(fixture("rollout-real.jsonl")).unwrap()).unwrap();
        std::fs::write(cli.join("rollout/model-io-sess_subagent_agent_b.jsonl"), b"{}\n").unwrap();
        std::fs::write(cli.join("rollout/notes.txt"), b"{}\n").unwrap();
        std::env::set_var(paths::ENV_ZCODE_HOME, dir.path());
        let found = discover(&DateFilter::default());
        assert_eq!(found.len(), 2, "both rollout layouts, and nothing else: {found:?}");
        assert!(found.iter().all(|f| f.kind == FileKind::Jsonl));
        assert!(found[0].path.ends_with("model-io-sess_a.jsonl"));
        assert_eq!(found[0].size, std::fs::metadata(fixture("rollout-real.jsonl")).unwrap().len());
        let probe = probe().unwrap();
        assert_eq!(probe.id, "zcode");
        assert!(probe.hint.as_deref().is_some_and(|h| h.starts_with("rollout")), "no db yet: {:?}", probe.hint);
        assert!(discover(&DateFilter::new(None, Some(0))).is_empty(), "until_ms prunes by mtime");

        // Installing the db makes it the only source handed out.
        std::fs::copy(fixture("model_usage.sqlite"), cli.join("db/db.sqlite")).unwrap();
        let found = discover(&DateFilter::default());
        assert_eq!(found.len(), 1, "the rollout files are dropped: {found:?}");
        assert_eq!(found[0].kind, FileKind::Sqlite);
        let out = read(&found[0], ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 8);
        assert_eq!(out.cursor, ReadCursor(9), "the cursor is a rowid");
        assert_eq!(out.events[0].project.as_deref(), Some("WorkFile"), "the db is where the project label lives");
        std::env::remove_var(paths::ENV_ZCODE_HOME);
    }
}
