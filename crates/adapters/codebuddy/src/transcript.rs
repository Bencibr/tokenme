//! The message files, read into [`UsageEvent`]s.
//!
//! Measured on this machine (2026-10-10, CodeBuddy CN 0.4.x): one JSON file per
//! message under `history/<workspace>/<conversation>/messages/`, and the turn's
//! **last assistant message** carries the turn's whole bill in
//! `extra.statsSnapshot` —
//!
//! ```json
//! {"inputTokens": 79962, "outputTokens": 2448, "cachedInputTokens": 56224,
//!  "cacheWriteTokens": 0, "cacheMissTokens": 23738, "thinkingTokens": 0,
//!  "elapsedMs": 139664, "lastOutputTokens": 1622, "credit": 2.69}
//! ```
//!
//! `inputTokens` includes the cache (79962 = 56224 cached + 23738 miss), so the
//! stages net exactly like every other vendor here: `cache_read =
//! cachedInputTokens`, `cache_creation = cacheWriteTokens`, `input =
//! cacheMissTokens`. `credit` is the vendor's own billing unit for the turn and
//! rides along the way WorkBuddy's does. The intermediate assistant messages of
//! the same turn carry only `lastStep*Tokens` (per agent step, not per turn) —
//! they have no `statsSnapshot` and are skipped; summing steps would bill a
//! turn twice over.
//!
//! The snapshot's turn scope is measured on one turn so far — a conversation's
//! second turn lands in its own file with its own id, so even if the vendor
//! turned out to repeat cumulative numbers the per-message dedupe key keeps the
//! index from double-counting a re-read; what it cannot fix is a cumulative
//! vendor field, and that stays an open measurement until a second turn exists.
//!
//! `extra` arrives as a JSON-encoded *string* inside the JSON file — parsed
//! twice, tolerated when it is already an object.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use usage_core::{FileKind, SourceFile};

use crate::{paths, DEDUPE_PREFIX, TOOL_ID};

/// The `history/<workspace>/<conversation>/messages` directories under every
/// data root, in walk order.
pub fn roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in paths::config_roots() {
        // Data/<uid>/CodeBuddyIDE/<uid>/history — the uid repeats by measure.
        let Ok(uids) = fs::read_dir(&root) else { continue };
        for uid in uids.flatten() {
            let ide = uid.path().join("CodeBuddyIDE");
            let Ok(ides) = fs::read_dir(&ide) else { continue };
            for id in ides.flatten() {
                let history = id.path().join("history");
                if history.is_dir() && !out.contains(&history) {
                    out.push(history);
                }
            }
        }
    }
    out
}

/// Every message file under `history/<ws>/<conv>/messages/`, mtime-filtered,
/// sorted by path so two discoveries agree on one order.
pub fn discover(root: &Path, since_ms: Option<i64>) -> Vec<SourceFile> {
    let mut files = Vec::new();
    let Ok(workspaces) = fs::read_dir(root) else { return files };
    for ws in workspaces.flatten() {
        let Ok(convs) = fs::read_dir(ws.path()) else { continue };
        for conv in convs.flatten() {
            let messages = conv.path().join("messages");
            let Ok(entries) = fs::read_dir(&messages) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                let mtime_ms = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if let Some(since) = since_ms {
                    if mtime_ms != 0 && mtime_ms < since {
                        continue;
                    }
                }
                files.push(SourceFile {
                    path,
                    kind: FileKind::Tree,
                    size: meta.len(),
                    mtime_ms,
                });
            }
        }
    }
    files
}

/// One message file → zero or one [`UsageEvent`]. `Tree` semantics: the file is
/// one record, re-read whole whenever it changes; the cursor is meaningless.
pub fn read(file: &SourceFile) -> usage_core::ReadOutcome {
    let event = fs::read(&file.path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|message| event_from_message(&message, file));
    usage_core::ReadOutcome {
        events: event.into_iter().collect(),
        cursor: usage_core::ReadCursor(0),
    }
}

/// The turn-final assistant message → one billable turn, or `None`.
fn event_from_message(message: &Value, file: &SourceFile) -> Option<usage_core::UsageEvent> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let id = message.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let extra = match message.get("extra") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok()?,
        Some(value @ Value::Object(_)) => value.clone(),
        _ => return None,
    };
    let snapshot = extra.get("statsSnapshot")?;
    let ts_ms = message
        .get("createdAt")
        .and_then(Value::as_str)
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.timestamp_millis())
        .filter(|ts| *ts > 0)?;
    // inputTokens includes the cache; net the stages apart, clamping a vendor
    // that changes its convention into a sane shape rather than a negative.
    let total = number(snapshot.get("inputTokens")).unwrap_or(0.0).max(0.0);
    let cached = number(snapshot.get("cachedInputTokens")).unwrap_or(0.0).max(0.0);
    let write = number(snapshot.get("cacheWriteTokens")).unwrap_or(0.0).max(0.0);
    let miss = number(snapshot.get("cacheMissTokens")).unwrap_or(0.0).max(0.0);
    let cache_read = cached.min(total);
    let cache_creation = write.min(total - cache_read);
    let input = if miss > 0.0 { miss.min(total) } else { (total - cache_read - cache_creation).max(0.0) };
    let output = number(snapshot.get("outputTokens")).unwrap_or(0.0).max(0.0);
    let reasoning = number(snapshot.get("thinkingTokens")).unwrap_or(0.0).max(0.0);
    let credits = number(snapshot.get("credit")).unwrap_or(0.0).max(0.0);
    if input == 0.0 && cache_read == 0.0 && cache_creation == 0.0 && output == 0.0 && credits == 0.0 {
        // A snapshot without numbers is workflow bookkeeping, not usage.
        return None;
    }
    // The conversation directory names the session the app lists.
    let session = file
        .path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| id.to_string());
    let mut event = usage_core::UsageEvent::new(TOOL_ID, ts_ms, session).with(usage_core::TokenCounts {
        input,
        cache_creation,
        cache_read,
        output,
        reasoning,
        credits,
    });
    event.meter = usage_core::Meter::Tokens;
    event.model = extra
        .get("modelId")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    event.dedupe_key = Some(format!("{DEDUPE_PREFIX}{id}"));
    event.source = file.key();
    Some(event)
}

/// A number that may arrive as either JSON type.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) const TURN: &str = r#"{"role":"assistant","message":"done","id":"msg-1","extra":"{\"requestId\":\"r1\",\"modelId\":\"auto\",\"modelName\":\"Auto\",\"statsSnapshot\":{\"inputTokens\":79962,\"outputTokens\":2448,\"cachedInputTokens\":56224,\"cacheWriteTokens\":0,\"cacheMissTokens\":23738,\"thinkingTokens\":183,\"elapsedMs\":139664,\"lastOutputTokens\":1622,\"credit\":2.69}}","createdAt":"2026-10-10T01:42:11.467Z"}"#;
    pub(crate) const USER: &str = r#"{"role":"user","message":"scan","id":"msg-0","extra":"{\"requestId\":\"r1\"}","createdAt":"2026-10-10T01:39:11.000Z"}"#;

    /// A history tree shaped like the runtime's: one uid pair, one workspace,
    /// one conversation, a turn-final assistant file and a user file.
    pub(crate) fn fixture(dir: &Path) -> PathBuf {
        let messages = dir
            .join("uid-a")
            .join("CodeBuddyIDE")
            .join("uid-a")
            .join("history")
            .join("ws-hash")
            .join("conv-1")
            .join("messages");
        fs::create_dir_all(&messages).unwrap();
        let mut f = fs::File::create(messages.join("msg-1.json")).unwrap();
        f.write_all(TURN.as_bytes()).unwrap();
        let mut f = fs::File::create(messages.join("msg-0.json")).unwrap();
        f.write_all(USER.as_bytes()).unwrap();
        messages
    }
}
