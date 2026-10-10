//! `projects/**/<session>.jsonl` — the app's own transcripts, one line per event.
//!
//! Every LLM call writes a line carrying `providerData.rawUsage`, the vendor's
//! unmodified billing payload:
//!
//! ```text
//! {"id":"call-1","sessionId":"a1b2c3d4…","cwd":"/Users/me/workspace/bug-hunter",
//!  "timestamp":1789700000000,"type":"function_call",
//!  "providerData":{"model":"deepseek-v4.1-flash","conversationRequestId":"22c4c5…",
//!    "rawUsage":{"prompt_tokens":31402,"completion_tokens":323,
//!      "prompt_cache_hit_tokens":8320,"prompt_cache_write_tokens":0,
//!      "completion_thinking_tokens":183,"credit":0, …}}}
//! ```
//!
//! The four stages are mutually exclusive after netting, which is what
//! [`usage_core::TokenCounts::total`] assumes: `prompt_tokens` **already contains**
//! the cache hit (`prompt_cache_hit_tokens + prompt_cache_miss_tokens = prompt_tokens`
//! in 8,720 of 8,720 lines here), so the hit and the write come off the front.
//! `completion_thinking_tokens` is a sub-split of `completion_tokens`, same as
//! Codex's `reasoning_output_tokens`, so it rides along without entering `total`.
//!
//! Two field traps, both measured here and agreed by two independent third-party
//! readers (`ChanningYuan/usageBar`'s `WorkBuddyDetailScanner`, and the
//! prompt-cache decode in `skainguyen`-style dashboards):
//!
//! * top-level `cached_tokens` and `cache_read_input_tokens` are **0 for this
//!   provider** even when 8,320 tokens came out of the cache — only
//!   `prompt_cache_hit_tokens` carries the truth.
//! * `credit` is the vendor's own internal credit for that one call. Summed over
//!   a session it lands on `sessions.credit_json` exactly (314.54 both ways on
//!   this machine, 2026-09-29), which is why the transcripts replace the database
//!   rather than adding to it: reading both would bill the account twice.
//!
//! Sub-agent work lives one level deeper (`<session>/subagents/agent-*.jsonl`)
//! under the owning session's directory, with its own `sessionId` that appears
//! nowhere in the database. It is real spend, so the events keep the **owner**
//! directory as their session — the app's session list stays the unit of account
//! — and the sub-agent cost lands on the session that spawned it.
//!
//! Idempotence: `id` is unique across the whole store (8,720 usage lines, 8,720
//! distinct ids here), so it is the dedupe key and the byte cursor makes the read
//! incremental. A line that is still being flushed is left for the next pass.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;
use usage_core::{Error, FileKind, Meter, ReadCursor, ReadOutcome, SourceFile, TokenCounts, UsageEvent};
use walkdir::WalkDir;

use crate::{DEDUPE_PREFIX, TOOL_ID};

/// Pre-filter: the store is mostly reasoning text and tool payloads, and only a
/// `rawUsage` line can ever bill.
const MARK: &str = "\"rawUsage\"";

/// Transcript files of one data root, oldest-path-first. `.file-rollback.ndjson`
/// sidecars are a different format and never carry usage.
pub fn discover(projects: &Path, since_ms: Option<i64>) -> Vec<SourceFile> {
    if !projects.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<SourceFile> = WalkDir::new(projects)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && entry.path().extension() == Some(std::ffi::OsStr::new("jsonl")))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            let mtime_ms = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as i64;
            Some(SourceFile { path: entry.into_path(), kind: FileKind::Jsonl, size: meta.len(), mtime_ms })
        })
        // Project dirs are keyed by workspace, not by date: mtime is the only
        // time signal available before a file is parsed.
        .filter(|file| since_ms.is_none_or(|since| file.mtime_ms >= since))
        .collect();
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    files
}

pub fn read(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    let Some(tail) = read_tail(file, cursor.0)? else {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    };
    let Some(last) = tail.iter().rposition(|byte| *byte == b'\n') else {
        // No complete line in the window: the app is mid-flush.
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    };
    let processed = last + 1;
    let session = owner_session(&file.path);
    let mut events = Vec::new();
    for raw in tail[..processed].split(|byte| *byte == b'\n') {
        if raw.is_empty() || !raw.windows(MARK.len()).any(|window| window == MARK.as_bytes()) {
            continue;
        }
        // Invalid UTF-8 is a torn write, same class as a JSON parse failure.
        let Ok(text) = std::str::from_utf8(raw) else { continue };
        let Ok(line) = serde_json::from_str::<Value>(text) else { continue };
        if let Some(event) = event_from_line(&line, session.as_deref(), file) {
            events.push(event);
        }
    }
    Ok(ReadOutcome { events, cursor: ReadCursor(cursor.0 + processed as u64) })
}

fn read_tail(file: &SourceFile, start: u64) -> Result<Option<Vec<u8>>, Error> {
    let Ok(mut handle) = File::open(&file.path) else { return Ok(None) };
    let Ok(len) = handle.metadata().map(|meta| meta.len()) else { return Ok(None) };
    if start > len {
        // Truncated or rotated underneath our cursor: the indexer owns the
        // decision to purge and re-read this file.
        return Err(Error::Cursor { path: file.path.clone(), cursor: start });
    }
    if start == len {
        return Ok(Some(Vec::new()));
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if handle.seek(SeekFrom::Start(start)).is_err() || handle.read_to_end(&mut buf).is_err() {
        // A short read would hand back a torn record; drop the window whole.
        return Ok(None);
    }
    Ok(Some(buf))
}

/// The database session a transcript belongs to: `<root>/<sid>.jsonl` names
/// itself, and `<root>/<sid>/subagents/agent-*.jsonl` sits under its parent.
fn owner_session(path: &Path) -> Option<String> {
    let name = path.file_stem()?.to_string_lossy().into_owned();
    if name.starts_with("agent-") {
        let parent = path.parent()?.parent()?.file_name()?;
        return Some(parent.to_string_lossy().into_owned());
    }
    Some(name)
}

/// One usage-bearing line → one billable call, or `None` when the line carries
/// nothing to charge or no id to dedupe on.
fn event_from_line(line: &Value, session: Option<&str>, file: &SourceFile) -> Option<UsageEvent> {
    let data = line.get("providerData")?;
    let usage = data.get("rawUsage")?;
    let id = text(line, "id")?;
    let ts_ms = line
        .get("timestamp")
        .and_then(Value::as_i64)
        .filter(|ts| *ts > 0)?;
    let counts = stages(usage)?;
    if counts.is_zero() {
        // A call that reported no tokens and no credit is workflow bookkeeping,
        // not usage.
        return None;
    }
    // The line's own sessionId is a sub-agent's private id; the owner directory
    // is what the app lists and what the database keys on.
    let session = session.unwrap_or(id.as_str());
    let mut event = UsageEvent::new(TOOL_ID, ts_ms, session).with(counts);
    event.meter = Meter::Tokens;
    event.model = text(data, "model")
        .or_else(|| text(data, "requestModelId"))
        .filter(|model| !model.is_empty());
    event.project = text(line, "cwd").map(basename);
    event.dedupe_key = Some(format!("{DEDUPE_PREFIX}{id}"));
    event.source = file.key();
    Some(event)
}

/// `/Users/me/workspace/bug-hunter` → `bug-hunter`; the panel shows basenames.
fn basename(cwd: String) -> String {
    cwd.rsplit('/').next().filter(|part| !part.is_empty()).map(str::to_string).unwrap_or(cwd)
}

/// The four mutually exclusive stages, netted out of the vendor's totals.
fn stages(usage: &Value) -> Option<TokenCounts> {
    let prompt = usage.get("prompt_tokens").and_then(number).unwrap_or(0.0).max(0.0);
    let output = usage.get("completion_tokens").and_then(number).unwrap_or(0.0).max(0.0);
    // Only `prompt_cache_hit_tokens` is trustworthy for this provider; the
    // Anthropic-named twins of these fields sit next to it at 0.
    let hit = usage.get("prompt_cache_hit_tokens").and_then(number).unwrap_or(0.0).max(0.0);
    let write = usage
        .get("prompt_cache_write_tokens")
        .and_then(number)
        .or_else(|| usage.get("cache_creation_input_tokens").and_then(number))
        .unwrap_or(0.0)
        .max(0.0);
    let reasoning = usage
        .get("completion_thinking_tokens")
        .and_then(number)
        .or_else(|| {
            usage
                .get("completion_tokens_details")
                .and_then(|details| details.get("reasoning_tokens"))
                .and_then(number)
        })
        .unwrap_or(0.0)
        .max(0.0);
    if prompt == 0.0 && output == 0.0 {
        return None;
    }
    // `prompt` already contains both; clamping keeps a provider that changes its
    // convention from producing a negative uncached prompt.
    let cache_read = hit.min(prompt);
    let cache_creation = write.min(prompt - cache_read);
    Some(TokenCounts {
        input: (prompt - cache_read - cache_creation).max(0.0),
        cache_creation,
        cache_read,
        output,
        reasoning,
        credits: usage.get("credit").and_then(number).unwrap_or(0.0).max(0.0),
    })
}

/// A number that may arrive as either JSON type — `credit` is `0` in most lines
/// and `6.78` in billed ones.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Roots to walk: the desktop app's data dir plus the CLI/CodeBuddy-shaped one,
/// which the third-party readers default to. Both exist on some machines and
/// only one on others.
pub fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in crate::paths::config_roots() {
        let projects = root.join("projects");
        if projects.is_dir() && !roots.contains(&projects) {
            roots.push(projects);
        }
    }
    roots
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const CALL: &str = r#"{"id":"call-1","parentId":"p","timestamp":1789700000000,"type":"function_call","sessionId":"a1b2c3d4","cwd":"/Users/me/workspace/bug-hunter","providerData":{"model":"deepseek-v4.1-flash","conversationRequestId":"round-1","rawUsage":{"prompt_tokens":31402,"completion_tokens":323,"total_tokens":31725,"prompt_cache_hit_tokens":8320,"prompt_cache_miss_tokens":23082,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"prompt_cache_write_tokens":0,"completion_thinking_tokens":183,"credit":6.78,"cached_tokens":0}}}"#;

    /// A transcript tree shaped like the vendor's: two workspaces, a session with
    /// a sub-agent directory, and the rollback sidecar that must not be read.
    pub(crate) fn fixture(dir: &Path) -> Vec<SourceFile> {
        let projects = dir.join("projects");
        let ws = projects.join("Users-sp-workspace-bug-hunter");
        let sub = ws.join("a1b2c3d4").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            ws.join("a1b2c3d4.jsonl"),
            format!(
                "{CALL}\n{}\n",
                r#"{"id":"zero","timestamp":1789700001000,"providerData":{"rawUsage":{"prompt_tokens":0,"completion_tokens":0,"credit":0}}}"#.to_string()
            ),
        )
        .unwrap();
        std::fs::write(ws.join("a1b2c3d4.file-rollback.ndjson"), b"not transcripts\n").unwrap();
        std::fs::write(
            sub.join("agent-002d75e2.jsonl"),
            format!(
                "{}\n",
                r#"{"id":"sub-1","timestamp":1789700100000,"sessionId":"ee5544d1","providerData":{"model":"hy4-preview-f","rawUsage":{"prompt_tokens":1000,"completion_tokens":10,"prompt_cache_hit_tokens":0,"credit":1.5}}}"#
            ),
        )
        .unwrap();
        discover(&projects, None)
    }

    fn source(dir: &Path, name: &str) -> SourceFile {
        fixture(dir)
            .into_iter()
            .find(|file| file.path.file_name().unwrap() == name)
            .unwrap()
    }

    #[test]
    fn one_line_is_one_call_with_netted_stages_and_its_own_credit() {
        let dir = tempfile::tempdir().unwrap();
        let file = source(dir.path(), "a1b2c3d4.jsonl");
        let outcome = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 1, "the zero-usage line bills nothing");
        let event = &outcome.events[0];
        assert_eq!(event.meter, Meter::Tokens);
        // 31,402 prompt with 8,320 of it served from cache.
        assert_eq!(
            (event.counts.input, event.counts.cache_read, event.counts.cache_creation, event.counts.output),
            (23082.0, 8320.0, 0.0, 323.0)
        );
        assert_eq!(event.counts.total(), 31725.0, "= the vendor's own total_tokens");
        assert_eq!(event.counts.reasoning, 183.0, "a sub-split of output, outside total");
        assert_eq!(event.counts.credits, 6.78);
        assert_eq!(event.model.as_deref(), Some("deepseek-v4.1-flash"));
        assert_eq!(event.project.as_deref(), Some("bug-hunter"));
        assert_eq!(event.dedupe_key.as_deref(), Some("workbuddy#call-1"));
        assert_eq!(event.ts_ms, 1789700000000, "the call's own time, not the session's close");
        assert_eq!(outcome.cursor.0, file.size, "the whole window is consumed up to the last newline");
    }

    #[test]
    fn sub_agent_spend_lands_on_the_session_that_spawned_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = source(dir.path(), "agent-002d75e2.jsonl");
        let outcome = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].session, "a1b2c3d4", "the owner directory, not the sub-agent's private id");
        assert_eq!(outcome.events[0].counts.credits, 1.5);
    }

    #[test]
    fn a_second_pass_sees_only_the_appended_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = source(dir.path(), "a1b2c3d4.jsonl");
        let first = read(&file, ReadCursor(0)).unwrap();
        let appended = format!("{}\n", CALL.replace("call-1", "call-2"));
        use std::io::Write as _;
        let mut handle = std::fs::OpenOptions::new().append(true).open(&file.path).unwrap();
        handle.write_all(appended.as_bytes()).unwrap();
        drop(handle);
        let (size, _) = crate::paths::stat_file(&file.path).unwrap();
        file.size = size;
        let second = read(&file, first.cursor).unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].dedupe_key.as_deref(), Some("workbuddy#call-2"));
        let restated = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(restated.events.len(), 2, "a re-read re-emits, and the ingest dedupe swallows the twin");
    }

    #[test]
    fn a_partial_line_waits_for_the_flush_and_a_shrunk_file_reports_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects/U-x/abc.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"id\":\"half\",\"rawUsage\"").unwrap();
        let (size, mtime_ms) = crate::paths::stat_file(&path).unwrap();
        let file = SourceFile { path, kind: FileKind::Jsonl, size, mtime_ms };
        let outcome = read(&file, ReadCursor(0)).unwrap();
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.cursor.0, 0, "the cursor does not move past an incomplete record");
        let gone = read(&file, ReadCursor(size + 1)).unwrap_err();
        assert!(matches!(gone, Error::Cursor { .. }), "a log shorter than its cursor is the indexer's problem");
    }
}
