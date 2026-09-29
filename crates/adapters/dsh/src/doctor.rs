//! The accuracy audit: what DSH's own ledger says vs what this adapter reads.
//!
//! `ledger()` walks the same roots the adapter reads and returns one row per
//! session with the numbers straight off the source — projections for format
//! v4 (the cumulative `tokenUsage.totals`), v3 stream headers for identity —
//! plus the structural facts that explain honest drift: a projection whose
//! `inheritedEventCount` is non-zero covers only its v4-era events, and
//! `tokenUsage` is the main thread's ledger, so subagent spend (a separate
//! row) is not in it. `tokenme dsh-doctor` prints these next to the index's
//! events; any line that disagrees names its own reason.

use std::path::PathBuf;

use serde_json::Value;

/// One session's ledger, straight from the source.
#[derive(Debug, Clone)]
pub struct LedgerRow {
    pub session: String,
    /// `4` for projections, `3` for plain v3 streams (which carry per-call
    /// events, so their ledger row is identity-only).
    pub format: u8,
    /// Project path recorded on the session.
    pub cwd: Option<String>,
    /// `tokenUsage.totals` — cumulative, main thread only.
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// `lastPromptAt`, epoch ms — when the session was last used.
    pub last_prompt_at: Option<i64>,
    /// Events the session inherited from an older format; their tokens may
    /// sit outside this projection's totals.
    pub inherited_events: u64,
    /// Subagent entries on the projection; their spend is not in `totals`.
    pub subagents: usize,
    /// The projection file's own path (empty for stream-only sessions).
    pub projection: Option<PathBuf>,
}

/// Every session the writer has state for, projections first-class.
pub fn ledger() -> Vec<LedgerRow> {
    let mut out = Vec::new();
    let Some(dir) = crate::paths::projcache_dir() else { return out };
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let session = stem.strip_prefix("session-").unwrap_or(stem).to_string();
        let totals = v.pointer("/record/rows/tokenUsage/val/totals");
        let num = |k: &str| {
            totals
                .and_then(|t| t.get(k))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0)
        };
        out.push(LedgerRow {
            session,
            format: v
                .pointer("/record/identity/formatVersion")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u8,
            cwd: v
                .pointer("/record/identity/cwd")
                .and_then(Value::as_str)
                .map(str::to_string),
            input: num("uncachedInputTokens"),
            output: num("outputTokens"),
            cache_read: num("cacheReadTokens"),
            cache_write: num("cacheWriteTokens"),
            last_prompt_at: v
                .pointer("/record/rows/sessionListMetadata/val/lastPromptAt")
                .and_then(Value::as_i64),
            inherited_events: v
                .pointer("/record/identity/inheritedEventCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            subagents: v
                .pointer("/record/rows/subagent/val")
                .and_then(Value::as_object)
                .map(|o| o.len())
                .unwrap_or(0),
            projection: Some(path),
        });
    }
    out.sort_by(|a, b| a.session.cmp(&b.session));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_root_answers_an_empty_ledger() {
        // SAFETY: test-scoped; the env is dsh-adapter-private.
        let guard = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());
        assert!(ledger().is_empty(), "no projection dir, no ledger");
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
        drop(guard);
    }
}
