//! One `wire.jsonl` → the events it bills.
//!
//! The whole file is re-read whenever it changes, which is what [`FileKind::Tree`]
//! means in this codebase: Kimi Code *rewrites* its wire log
//! (`wire/wireService.ts:398`, `this.log.rewrite(scope, 'wire.jsonl', rewritten)`)
//! when compaction retracts content, so a byte cursor would resume into a file
//! whose earlier bytes are no longer the records they were. Re-reading is only
//! safe because every event carries an identity the index dedupes on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use usage_core::{Meter, ReadCursor, ReadOutcome, SourceFile, UsageEvent};

use crate::parser::{self, Parsed};
use crate::paths;
use crate::TOOL_ID;

/// Reads one wire file from the start. The cursor is passed through untouched: a
/// `Tree` source has none, and the indexer stops after a single call.
pub(crate) fn read_file(file: &SourceFile, cursor: ReadCursor) -> ReadOutcome {
    let workspaces = workspace_roots();
    match std::fs::read(&file.path) {
        Err(_) => ReadOutcome { events: Vec::new(), cursor },
        Ok(bytes) => ReadOutcome { events: events(&bytes, file, &workspaces), cursor },
    }
}

/// Complete lines only: a trailing fragment is a record the CLI is still
/// writing, and half a JSON object would be dropped as malformed anyway.
fn events(bytes: &[u8], file: &SourceFile, workspaces: &HashMap<String, String>) -> Vec<UsageEvent> {
    let finished = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(last) => last + 1,
        None => return Vec::new(),
    };
    // A subagent's calls belong to the session that spawned them: the panel's
    // session row is the unit a person reads, and folding them in is what keeps
    // the total equal to what the CLI itself prints for that session.
    let (workspace, session, _agent) = paths::layout(&file.path);
    // The CLI labels a session by its workspace root, which `workspaces.json`
    // resolves from the id the directory is named by. No map, no label.
    let project = workspace.and_then(|id| workspaces.get(&id).cloned());
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

fn event(parsed: Parsed, file: &SourceFile, session: &str, project: Option<&str>, ordinal: usize) -> UsageEvent {
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
/// Three shapes, best evidence first: the legacy payload's own `message_id`;
/// otherwise the stamp plus the model and all four stages, which is the tuple a
/// second call would have to match by millisecond; otherwise, for a row with
/// neither, the position in the file.
///
/// The agent is in it because the source states it (`agentId`), and a different
/// agent's call is a different call. Deliberately **not** in it: the session
/// directory. `migration-legacy` copies v1 sessions out of `~/.kimi` into a v2
/// directory whose session id is new, and both roots are read — an id inside the
/// key would bill that copied history twice, a signature outside it collapses it.
fn key(parsed: &Parsed, file: &SourceFile, session: &str, ordinal: usize) -> String {
    if let Some(id) = parsed.message_id.as_deref() {
        return format!("{TOOL_ID}#{id}");
    }
    // The agent as the source wrote it, not as the directory names it: v1 has no
    // `agents/` level, so the payload id is the one spelling a migrated copy keeps.
    let who = parsed.agent_id.as_deref().unwrap_or("");
    let c = &parsed.counts;
    match parsed.time_ms.filter(|ms| *ms > 0) {
        Some(ms) => {
            let [input, creation, read, output] = stages(c.input, c.cache_creation, c.cache_read, c.output);
            format!(
                "{TOOL_ID}#{who}#{ms}#{}#{input}#{creation}#{read}#{output}",
                parsed.model.as_deref().unwrap_or("-")
            )
        }
        // A row with no stamp cannot be told apart from its twin by content, so
        // the only identity left is where it sits.
        None => format!(
            "{TOOL_ID}#{who}#{}#{session}#{ordinal}",
            file.path.to_string_lossy()
        ),
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

/// Every home's `workspaces.json` → `id → root`, the label each host shows for a
/// session. The CLI home and the desktop runtime name their workspaces in their
/// own maps; an id is looked up across all of them, first hit wins.
///
/// The vendor's own state file, re-read per wire file: it is a few kilobytes and
/// a pass touches a handful of logs, so caching it would cost a staleness bug for
/// no measurable time. A corrupt or absent map means no project label, never a
/// lost event.
fn workspace_roots() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for path in paths::workspaces_files() {
        let Some(roots) = roots_at(&path) else { continue };
        for (id, root) in roots {
            out.entry(id).or_insert(root);
        }
    }
    out
}

fn roots_at(path: &Path) -> Option<HashMap<String, String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let entries = value.get("workspaces")?.as_object()?;
    let mut out = HashMap::new();
    for (id, entry) in entries {
        let root = entry.get("root").and_then(serde_json::Value::as_str)?;
        if !root.trim().is_empty() {
            out.insert(id.clone(), root.to_string());
        }
    }
    Some(out)
}

/// A wire file the caller can hand to [`read_file`], statted the way the indexer
/// stats a source.
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

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::TokenCounts;

    fn parsed(message_id: Option<&str>, time_ms: Option<i64>, model: Option<&str>) -> Parsed {
        Parsed {
            agent_id: Some("main".into()),
            model: model.map(str::to_string),
            counts: TokenCounts { input: 100.0, cache_creation: 1.0, cache_read: 2.0, output: 3.0, ..Default::default() },
            time_ms,
            message_id: message_id.map(str::to_string),
        }
    }

    fn file(path: &str) -> SourceFile {
        SourceFile {
            path: PathBuf::from(path),
            kind: usage_core::FileKind::Tree,
            size: 1,
            mtime_ms: 1_787_799_862_846,
        }
    }

    #[test]
    fn a_message_id_is_the_key_when_the_source_has_one() {
        let key = key(&parsed(Some("msg_9"), Some(1), Some("m")), &file("/w/ses/wire.jsonl"), "ses", 7);
        assert_eq!(key, "kimicode#msg_9");
    }

    /// The migrated-history case: the same call under two session directories is
    /// one event, because the session name is not part of the signature.
    #[test]
    fn the_same_call_in_two_session_directories_is_one_key() {
        let a = key(&parsed(None, Some(1_787_799_862_846), Some("m")), &file("/1/sessions/g/ses1/wire.jsonl"), "ses1", 3);
        let b = key(&parsed(None, Some(1_787_799_862_846), Some("m")), &file("/2/sessions/wd/ses2/agents/main/wire.jsonl"), "ses2", 9);
        assert_eq!(a, b, "a copy of one call is not two calls");
    }

    #[test]
    fn different_stamps_or_stages_are_different_calls() {
        let base = key(&parsed(None, Some(1_000), Some("m")), &file("/w/ses/wire.jsonl"), "ses", 0);
        assert_ne!(base, key(&parsed(None, Some(1_001), Some("m")), &file("/w/ses/wire.jsonl"), "ses", 0));
        let mut other = parsed(None, Some(1_000), Some("m"));
        other.counts.output = 4.0;
        assert_ne!(base, key(&other, &file("/w/ses/wire.jsonl"), "ses", 0));
        assert_ne!(base, key(&parsed(None, Some(1_000), Some("other")), &file("/w/ses/wire.jsonl"), "ses", 0));
    }

    /// Two agents' calls are two calls even when the millisecond and every stage
    /// match — which is why the payload's `agentId` is in the signature.
    #[test]
    fn a_different_agent_never_shares_a_key() {
        let mut sub = parsed(None, Some(1_000), Some("m"));
        sub.agent_id = Some("sub-1".into());
        assert_ne!(
            key(&parsed(None, Some(1_000), Some("m")), &file("/w/ses/wire.jsonl"), "ses", 0),
            key(&sub, &file("/w/ses/wire.jsonl"), "ses", 0)
        );
    }

    #[test]
    fn an_unstamped_row_is_keyed_by_position_not_collapsed() {
        let a = key(&parsed(None, None, Some("m")), &file("/w/ses/wire.jsonl"), "ses", 1);
        let b = key(&parsed(None, None, Some("m")), &file("/w/ses/wire.jsonl"), "ses", 2);
        assert_ne!(a, b, "two identical unstamped rows are two calls");
    }

    /// A complete line is read; the fragment the CLI is still writing is left for
    /// the next pass, and a file with no newline at all yields nothing.
    #[test]
    fn a_trailing_fragment_is_left_for_the_next_pass() {
        let file = file("/Users/me/.kimi-code/sessions/wd/ses/agents/main/wire.jsonl");
        let line = b"{\"type\":\"usage.record\",\"model\":\"m\",\"usage\":{\"inputOther\":5,\"output\":5},\"time\":1787799862846}\n";
        let half = b"{\"type\":\"usage.reco";
        let mut bytes = Vec::from(line);
        bytes.extend_from_slice(half);
        let out = events(&bytes, &file, &HashMap::new());
        assert_eq!(out.len(), 1, "the complete row is billed, the fragment is not");
        assert_eq!(out[0].counts.total(), 10.0);
        assert!(events(half, &file, &HashMap::new()).is_empty(), "no newline means no complete record");
    }

    #[test]
    fn workspace_ids_resolve_to_the_root_the_cli_shows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("workspaces.json"),
            r#"{"version":1,"workspaces":{"wd_x":{"root":"/Users/demo/projA"}}}"#,
        )
        .unwrap();
        let map = roots_at(&root.join("workspaces.json")).unwrap();
        assert_eq!(map.get("wd_x").map(String::as_str), Some("/Users/demo/projA"));
        // A corrupt map yields nothing rather than a panic.
        std::fs::write(root.join("broken.json"), "{oops").unwrap();
        assert!(roots_at(&root.join("broken.json")).is_none());
    }

    /// The CLI home and the desktop runtime each carry their own workspaces.json;
    /// an id resolves across both, and an id both maps know goes to the first
    /// home in the ladder rather than whichever file happened to be read last.
    #[test]
    fn workspace_ids_merge_across_two_homes() {
        let _g = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("cli");
        let second = dir.path().join("desktop");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(
            first.join("workspaces.json"),
            r#"{"workspaces":{"wd_cli":{"root":"/home/cli-proj"}}}"#,
        )
        .unwrap();
        std::fs::write(
            second.join("workspaces.json"),
            r#"{"workspaces":{"wd_desktop":{"root":"/home/desktop-proj"},
                "wd_cli":{"root":"/home/shadowed"}}}"#,
        )
        .unwrap();
        std::env::set_var(paths::ENV_DATA_DIR, format!("{},{}", first.display(), second.display()));
        let map = workspace_roots();
        assert_eq!(map.get("wd_desktop").map(String::as_str), Some("/home/desktop-proj"));
        assert_eq!(
            map.get("wd_cli").map(String::as_str),
            Some("/home/cli-proj"),
            "the first home wins an overlapping id"
        );
        std::env::remove_var(paths::ENV_DATA_DIR);
    }
}
