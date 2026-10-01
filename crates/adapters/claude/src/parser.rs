//! `<root>/projects/**/*.jsonl` → normalised [`usage_core::UsageEvent`]s.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};

use serde_json::Value;
use usage_core::{
    parse_ts_ms, Call, CallKind, Error, Meter, ReadCursor, ReadOutcome, SourceFile, TokenCounts,
    UsageEvent,
};

use crate::config::Config;
use crate::paths;
use crate::TOOL_ID;

/// Cheap pre-filter: of the 430 MB of logs almost nothing is billable payload,
/// so only lines carrying one of these markers get parsed as JSON.
const MARKS: [&str; 3] = ["\"usage\"", "\"tool_use\"", "<command-name>"];
const NAME_OPEN: &str = "<command-name>";
const NAME_CLOSE: &str = "</command-name>";

/// Read everything appended after `cursor`, stopping before a line that is
/// still being written.
pub(crate) fn read_file(
    file: &SourceFile,
    cursor: ReadCursor,
    cfg: &Config,
) -> Result<ReadOutcome, Error> {
    let Some(tail) = read_tail(file, cursor.0)? else { return Ok(untouched(cursor)) };
    let Some(last) = tail.iter().rposition(|b| *b == b'\n') else {
        // No complete line in the window: a record still being flushed.
        return Ok(untouched(cursor));
    };
    let processed = last + 1;
    let mut turns = Turns::default();
    for raw in tail[..processed].split(|b| *b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        // Invalid UTF-8 means a torn write, same class as a JSON parse failure.
        let Ok(text) = std::str::from_utf8(raw) else { continue };
        let text = text.strip_suffix('\r').unwrap_or(text);
        if !MARKS.iter().any(|mark| text.contains(mark)) {
            continue;
        }
        let Ok(line) = serde_json::from_str::<Value>(text) else { continue };
        turns.consume(&line, cfg);
    }
    Ok(ReadOutcome {
        events: turns.finish(file),
        cursor: ReadCursor(cursor.0 + processed as u64),
    })
}

fn untouched(cursor: ReadCursor) -> ReadOutcome {
    ReadOutcome { events: Vec::new(), cursor }
}

/// `Ok(None)` covers every "this file is not readable right now" case: the
/// indexer logs it and moves on, so it must not abort the whole pass.
fn read_tail(file: &SourceFile, start: u64) -> Result<Option<Vec<u8>>, Error> {
    let Ok(mut handle) = File::open(&file.path) else { return Ok(None) };
    let Ok(len) = handle.metadata().map(|m| m.len()) else { return Ok(None) };
    if start > len {
        // The log was truncated or rotated underneath our cursor. Only the
        // indexer, which owns the manifest, may decide to purge and re-read.
        return Err(Error::Cursor { path: file.path.clone(), cursor: start });
    }
    if start == len {
        return Ok(Some(Vec::new()));
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if handle.seek(SeekFrom::Start(start)).is_err() || handle.read_to_end(&mut buf).is_err() {
        // A short read would hand us a torn record; drop the window whole.
        return Ok(None);
    }
    Ok(Some(buf))
}

#[derive(Default)]
struct Bucket {
    id: String,
    ts_ms: Option<i64>,
    session: Option<String>,
    project: Option<String>,
    model: Option<String>,
    counts: TokenCounts,
    calls: Vec<Call>,
}

/// Assistant turns seen in one read window, in file order. Fields are owned so a
/// parsed line can be dropped at once: keeping every `Value` of an 84 MB log
/// alive until the end of the window would cost more than the log itself.
#[derive(Default)]
struct Turns {
    turns: Vec<Bucket>,
    index: HashMap<String, usize>,
    /// `<command-name>` tags waiting for the turn that answers them.
    pending: Vec<Call>,
}

impl Turns {
    fn consume(&mut self, line: &Value, cfg: &Config) {
        match line.get("type").and_then(Value::as_str) {
            Some("assistant") => self.assistant(line, cfg),
            Some("user") => self.user(line, cfg),
            // last-prompt, mode, permission-mode, atis-latch, attachment,
            // ai-title, file-history-*, cost-state, queue-operation, system.
            _ => {}
        }
    }

    fn assistant(&mut self, line: &Value, cfg: &Config) {
        let Some(message) = line.get("message") else { return };
        // Without a message id there is nothing for `dedupe_key` to repeat on,
        // so a later pass could not tell a re-write from a new turn.
        let Some(id) = message.get("id").and_then(Value::as_str) else { return };
        let slot = match self.index.get(id) {
            Some(&seen) => seen,
            None => {
                let fresh = Bucket {
                    id: id.to_string(),
                    // A slash command belongs to the turn that answers it.
                    calls: std::mem::take(&mut self.pending),
                    ..Bucket::default()
                };
                self.turns.push(fresh);
                self.index.insert(id.to_string(), self.turns.len() - 1);
                self.turns.len() - 1
            }
        };
        let bucket = &mut self.turns[slot];
        if bucket.ts_ms.is_none() {
            bucket.ts_ms = text(line, "timestamp").and_then(parse_ts_ms);
        }
        if bucket.session.is_none() {
            bucket.session = owned(text(line, "sessionId").or_else(|| text(line, "session_id")));
        }
        if bucket.project.is_none() {
            bucket.project = owned(text(line, "cwd"));
        }
        // Model ids stay verbatim, including proxy names (`step-5-preview`) and
        // the `<synthetic>` sentinel: substituting a plausible Anthropic model
        // would bill a turn the source never ran, while an unknown id simply
        // surfaces as unpriced.
        if bucket.model.is_none() {
            bucket.model = owned(text(message, "model"));
        }
        if let Some(usage) = message.get("usage").filter(|u| u.is_object()) {
            let counts = counts_from(usage);
            // Streaming emits one line per content block and only the last one
            // carries the finished usage; a retried write repeats it verbatim.
            // Keep the largest snapshot — summing would count the tail twice.
            if counts.total() > bucket.counts.total() {
                bucket.counts = counts;
            }
        }
        content_calls(message.get("content"), cfg, &mut bucket.calls);
    }

    fn user(&mut self, line: &Value, cfg: &Config) {
        let Some(content) = line.get("message").and_then(|m| m.get("content")) else { return };
        match content {
            Value::String(text) => push_command(text, cfg, &mut self.pending),
            Value::Array(blocks) => {
                for block in blocks {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        push_command(text, cfg, &mut self.pending);
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(self, file: &SourceFile) -> Vec<UsageEvent> {
        let mut events = Vec::new();
        for bucket in self.turns {
            // A turn whose lines all reported zeros (aborted block, error stub)
            // has nothing to bill and would only inflate the totals.
            if bucket.counts.total() <= 0.0 {
                continue;
            }
            let mut event = UsageEvent::new(
                TOOL_ID,
                // A record with no timestamp is filed under the file's mtime
                // rather than epoch 0, which would invent a 1970 bucket.
                bucket.ts_ms.unwrap_or(file.mtime_ms),
                bucket.session.unwrap_or_else(|| session_fallback(file)),
            );
            event.project = bucket.project.or_else(|| project_fallback(file));
            event.model = bucket.model;
            event.counts = bucket.counts;
            event.meter = Meter::Tokens;
            event.dedupe_key = Some(bucket.id);
            event.calls = bucket.calls;
            event.source = file.key();
            events.push(event);
        }
        events
    }
}

fn owned(value: Option<&str>) -> Option<String> {
    Some(value?.to_string())
}

fn text<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn session_fallback(file: &SourceFile) -> String {
    // `<root>/projects/<workspace>/<session>.jsonl` already names the session.
    file.path.file_stem().and_then(OsStr::to_str).unwrap_or_default().to_string()
}

fn project_fallback(file: &SourceFile) -> Option<String> {
    let seg = paths::project_segment(&file.path).and_then(OsStr::to_str)?;
    // The dir encodes the workspace with `/` and `.` both collapsed to `-`,
    // which is lossy for names containing dashes; enough for a label.
    Some(if seg.starts_with('-') {
        seg.replace("--", "/.").replace('-', "/")
    } else {
        seg.to_string()
    })
}

fn push_unique(calls: &mut Vec<Call>, call: Call) {
    if !calls.contains(&call) {
        calls.push(call);
    }
}

fn push_command(text: &str, cfg: &Config, out: &mut Vec<Call>) {
    let Some(name) = command_name(text) else { return };
    // Same whitelist as the `Skill` tool: built-in slash commands (`/clear`,
    // `/model`) are not skills and would otherwise dominate the breakdown.
    if !cfg.allows_skill(name) {
        return;
    }
    push_unique(out, Call { kind: CallKind::Skill, name: name.to_string() });
}

/// Slash-invoked skills land as a user prompt wrapped in
/// `<command-name>/foo</command-name>`, not as a `Skill` tool call.
fn command_name(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once(NAME_OPEN)?;
    let (name, _) = rest.split_once(NAME_CLOSE)?;
    let name = name.trim().trim_start_matches('/');
    (!name.is_empty()).then_some(name)
}

fn content_calls(content: Option<&Value>, cfg: &Config, out: &mut Vec<Call>) {
    let Some(blocks) = content.and_then(Value::as_array) else { return };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(name) = block.get("name").and_then(Value::as_str) else { continue };
        // Built-in tools (Bash, Read, Edit, Agent, …) are deliberately dropped:
        // they would swamp the breakdown with activity that is not an extension.
        let call = if let Some(rest) = name.strip_prefix("mcp__") {
            let Some((server, _)) = rest.split_once("__") else { continue };
            if !cfg.allows_mcp(server) {
                continue;
            }
            Call { kind: CallKind::Mcp, name: server.to_string() }
        } else if name == "Skill" {
            let Some(skill) =
                block.get("input").and_then(|input| input.get("skill")).and_then(Value::as_str)
            else {
                continue;
            };
            if !cfg.allows_skill(skill) {
                continue;
            }
            Call { kind: CallKind::Skill, name: skill.to_string() }
        } else {
            continue;
        };
        push_unique(out, call);
    }
}

fn counts_from(usage: &Value) -> TokenCounts {
    let num = |row: &Value, key: &str| row.get(key).map(number).unwrap_or(0.0);
    TokenCounts {
        input: num(usage, "input_tokens"),
        // THE TRAP: `cache_creation_input_tokens` is the API's authoritative
        // cache-write count, and the nested `cache_creation` object is only that
        // number split into 5m/1h tiers. Adding both doubles every cache write.
        cache_creation: num(usage, "cache_creation_input_tokens"),
        cache_read: num(usage, "cache_read_input_tokens"),
        output: num(usage, "output_tokens"),
        // Thinking tokens are a sub-breakdown of `output_tokens`, never extra.
        reasoning: usage
            .get("output_tokens_details")
            .map(|details| num(details, "thinking_tokens"))
            .unwrap_or(0.0),
        credits: 0.0,
    }
}

/// Third-party proxies stringify numbers now and then; a value we cannot read is
/// a zero, not a reason to throw away the turn.
fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn cache_write_is_counted_once() {
        let usage = json(
            r#"{"input_tokens":100,"output_tokens":20,
                "cache_creation_input_tokens":900,"cache_read_input_tokens":4000,
                "cache_creation":{"ephemeral_5m_input_tokens":600,"ephemeral_1h_input_tokens":300},
                "output_tokens_details":{"thinking_tokens":8}}"#,
        );
        let counts = counts_from(&usage);
        assert_eq!(
            (counts.input, counts.cache_creation, counts.cache_read, counts.output),
            (100.0, 900.0, 4000.0, 20.0)
        );
        assert_eq!(counts.reasoning, 8.0);
        assert_eq!(counts.total(), 5020.0);
    }

    #[test]
    fn unreadable_usage_values_do_not_lose_the_turn() {
        assert_eq!(counts_from(&json(r#"{"input_tokens":"7","output_tokens":-3}"#)).total(), 7.0);
        assert!(counts_from(&json("{}")).is_zero());
    }

    #[test]
    fn slash_command_tag_becomes_a_skill_name() {
        assert_eq!(
            command_name("<command-name>/deep-research</command-name>\n<command-args>x</command-args>"),
            Some("deep-research")
        );
        assert_eq!(command_name("no tag here"), None);
        assert_eq!(command_name("<command-name>   </command-name>"), None);
        assert_eq!(command_name("<command-name>broken"), None);
    }

    #[test]
    fn only_whitelisted_extensions_are_recorded() {
        let cfg = Config::default();
        let mut calls = Vec::new();
        for block in [
            r#"{"type":"tool_use","name":"Bash","input":{}}"#,
            r#"{"type":"tool_use","name":"mcp__bugx__run","input":{}}"#,
            r#"{"type":"tool_use","name":"Skill","input":{"skill":"orca"}}"#,
            r#"{"type":"text","text":"<command-name>/orca</command-name>"}"#,
        ] {
            content_calls(Some(&json(block)), &cfg, &mut calls);
        }
        assert!(calls.is_empty(), "built-ins and unlisted servers/skills must not appear");
    }

    #[test]
    fn mangled_project_dir_decodes_to_a_readable_label() {
        let file = source_file("/h/.claude/projects/-Users-dev--codex-worktrees-x/sess/a.jsonl");
        assert_eq!(project_fallback(&file).as_deref(), Some("/Users/dev/.codex/worktrees/x"));
        assert_eq!(session_fallback(&file), "a");
    }

    fn source_file(path: &str) -> SourceFile {
        SourceFile {
            path: std::path::PathBuf::from(path),
            kind: usage_core::FileKind::Jsonl,
            size: 1,
            mtime_ms: 1,
        }
    }
}
