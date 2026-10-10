//! dsh adapter.
//!
//! One source: `<sessions-root>/<workspace>/<session>/session.jsonl.zstd` —
//! zstd-compressed JSONL event streams, one file per agent session. Usage
//! rides on per-call lines whose exact envelope the session's format version
//! decides: v3 bills `assistant/chunk` (`{"type":"usage"}` with
//! `inputTokens`/`outputTokens`/`cacheReadTokens`/`reasoningTokens`), v4
//! bills `assistant/message` (`data.usage` with
//! `inputTokens`/`outputTokens`/`cacheReadTokens`/`cacheWriteTokens`). Both
//! name the uncached input `inputTokens` — the exclusive convention — and
//! stamp every call with its own wall clock, so a session continued across
//! days attributes each day's spend to that day.
//!
//! The sessions roots follow the writer's own layout (see [`paths`]): the
//! macOS CLI-era tree `~/.dsh/sessions` first, the desktop app's
//! `%APPDATA%\dsh-desktop\harness\sessions` (Windows) /
//! `~/Library/Application Support/dsh-desktop/harness/sessions` (macOS) second.
//! Format v4 names the stream `session.v4.jsonl.zstd` and bills the same way;
//! its projection cache (`storages/session_projcache`) is the writer's own
//! cumulative ledger — read by the accuracy audit ([`doctor::ledger`]), never
//! by billing: the projection holds the session's lifetime totals stamped
//! with `lastPromptAt`, and billing that would drag every earlier day's spend
//! into whichever day the session was last touched. All existing roots are
//! read, one session each: the trees mirror each other by identity (workspace
//! slug + session dir), and the first root in priority order wins a mirrored
//! session — reading a session from both roots would double-bill, reading
//! only one root hides the other's sessions entirely (measured 2026-09-29:
//! the stale `~/.dsh` tree masked the live desktop app's sessions for a day).
//!
//! Cursor is `FileKind::Tree`: whole-file re-parse on change, stable
//! `<session>#<seq>` dedupe keys absorbing the re-emit.
//!
//! Owned by the dsh-adapter workstream. The public surface (`DshAdapter`,
//! `TOOL_ID`) is frozen — `usage-adapter-all` and the CLI link against it.

mod doctor;
mod loader;
mod parser;
mod paths;

/// The accuracy audit surface: doctor::ledger reads this.

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile,
};

pub const TOOL_ID: &str = "dsh";

pub use doctor::ledger;
pub use paths::diagnostic_candidates;
pub(crate) const DISPLAY_NAME: &str = "DSH";

#[derive(Debug, Default, Clone, Copy)]
pub struct DshAdapter;

impl SourceAdapter for DshAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        loader::probe()
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        loader::discover(filter)
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        loader::read(file, cursor)
    }
}

#[cfg(test)]
mod smoke {
    use std::collections::BTreeSet;

    use usage_core::{DateFilter, FileKind, Meter, ReadCursor, TokenCounts};

    use super::*;

    /// One real-shaped session: header → usage rows, plus the noise lines that
    /// must not bill (snapshots, tool payloads, a torn zero-usage row).
    fn write_fixture() -> Vec<u8> {
        let lines = [
            r#"{"type":"session","version":0,"id":"session-abc","createdAt":1787182948129,"cwd":"/Users/me/work/neuro","delegationDepth":0}"#,
            r#"{"type":"permission/preset","seq":0,"time":1787182949260,"data":{"preset":"workspace-write"}}"#,
            r#"{"type":"request/header","seq":11,"time":1787182954985,"data":{"header":{"config":{"provider":"deepseek-official","model":"deepseek-v4-flash","maxTokens":256000}}}}"#,
            r#"{"type":"assistant/chunk","seq":30,"time":1787182957036,"data":{"turn":1,"step":1,"chunk":{"type":"usage","usage":{"inputTokens":13322,"outputTokens":218,"cacheReadTokens":0,"reasoningTokens":85}}}}"#,
            r#"{"type":"user/message","seq":31,"time":1787182957040,"data":{"content":[]}}"#,
            r#"{"type":"assistant/chunk","seq":40,"time":1787182959478,"data":{"turn":1,"step":2,"chunk":{"type":"usage","usage":{"inputTokens":601,"outputTokens":170,"cacheReadTokens":13440,"reasoningTokens":4000}}}}"#,
            r#"{"type":"assistant/chunk","seq":41,"time":1787182959480,"data":{"turn":1,"step":2,"chunk":{"type":"usage","usage":{"inputTokens":0,"outputTokens":0,"cacheReadTokens":0}}}}"#,
        ];
        let jsonl = lines.join("\n");
        zstd::stream::encode_all(jsonl.as_bytes(), 3).unwrap()
    }

    #[test]
    fn reads_a_fixture_session_end_to_end() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sessions/--Users-sp-workspace-neuro--/s1/session.jsonl.zstd");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let bytes = write_fixture();
        std::fs::write(&file, &bytes).unwrap();
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());

        let adapter = DshAdapter;
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1, "the session file is discovered");
        assert_eq!(sources[0].kind, FileKind::Tree);
        let outcome = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 2, "the zero-usage row is not billable");
        let e = &outcome.events[0];
        assert_eq!(e.tool, "dsh");
        assert_eq!(e.session, "session-abc");
        // The cached prefix is its own stage, exclusive of `input`.
        assert_eq!(
            e.counts,
            TokenCounts { input: 13322.0, cache_creation: 0.0, cache_read: 0.0, output: 218.0, reasoning: 85.0, credits: 0.0 }
        );
        assert_eq!(e.model.as_deref(), Some("deepseek-v4-flash"), "carried from request/header");
        assert_eq!(e.project.as_deref(), Some("neuro"), "basename of the session cwd");
        assert_eq!(e.dedupe_key.as_deref(), Some("session-abc#30"));
        assert_eq!(e.ts_ms, 1_787_182_957_036);
        assert_eq!(e.meter, Meter::Tokens);
        // Reasoning beyond output is clamped, never added on top.
        assert_eq!(outcome.events[1].counts.reasoning, 170.0);
        assert_eq!(outcome.cursor.0 as u64, bytes.len() as u64);
        let keys: BTreeSet<_> = outcome.events.iter().filter_map(|e| e.dedupe_key.clone()).collect();
        assert_eq!(keys.len(), outcome.events.len(), "dedupe keys are unique per call");
        // A re-read of an unchanged file re-emits the same keys — the index
        // absorbs them; nothing new is invented.
        let again = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        let again_keys: BTreeSet<_> = again.events.iter().filter_map(|e| e.dedupe_key.clone()).collect();
        assert_eq!(again_keys, keys);
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
    }

    /// A real-shaped v4 session: `session.v4.jsonl.zstd`, usage on
    /// `assistant/message` (`data.usage`, `inputTokens` uncached), two calls
    /// on two different days — the shape whose projection-era billing dragged
    /// the whole session total into whichever day the session was last
    /// touched. A projection cache beside it exists and must be ignored:
    /// billing reads the stream.
    fn write_v4_fixture() -> Vec<u8> {
        let lines = [
            r#"{"type":"session","version":4,"id":"session-d2b4cf00","createdAt":1791463181906,"cwd":"/Users/dev/workspace/wifitouch","isSeeded":false,"delegationDepth":0}"#,
            r#"{"type":"permission/preset","seq":1,"time":1791463182000,"data":{"preset":"workspace-write"}}"#,
            r#"{"type":"request/header","seq":20,"time":1791900000000,"data":{"header":{"config":{"provider":"xhs","model":"xhs"}}}}"#,
            r#"{"type":"assistant/message","seq":51,"time":1791900005000,"data":{"turn":2,"step":1,"message":{"role":"assistant","content":[]},"usage":{"inputTokens":12000000,"outputTokens":200000,"cacheReadTokens":0,"totalTokens":12200000}}}"#,
            r#"{"type":"user/message","seq":60,"time":1792500000000,"data":{"content":[]}}"#,
            r#"{"type":"request/header","seq":70,"time":1792500001000,"data":{"header":{"config":{"provider":"xhs","model":"deepseek-flash"}}}}"#,
            r#"{"type":"assistant/message","seq":80,"time":1792500006000,"data":{"turn":3,"step":1,"message":{"role":"assistant","content":[]},"usage":{"inputTokens":50000,"outputTokens":1600,"cacheReadTokens":1000,"totalTokens":52600}}}"#,
            r#"{"type":"assistant/message","seq":81,"time":1792500007000,"data":{"turn":3,"step":2,"message":{"role":"assistant","content":[]}}}"#,
        ];
        let jsonl = lines.join("\n");
        zstd::stream::encode_all(jsonl.as_bytes(), 3).unwrap()
    }

    #[test]
    fn a_v4_session_bills_each_day_from_its_own_stream() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let stream = dir
            .path()
            .join("sessions/--Users-dev-workspace-wifitouch--/s1/session.v4.jsonl.zstd");
        std::fs::create_dir_all(stream.parent().unwrap()).unwrap();
        let bytes = write_v4_fixture();
        std::fs::write(&stream, &bytes).unwrap();
        // The writer's projection cache sits beside the sessions tree; its
        // existence must not turn into a second source (and its lifetime
        // cumulative must never bill).
        let projcache = dir
            .path()
            .join("storages")
            .join("session_projcache")
            .join("sessions");
        std::fs::create_dir_all(&projcache).unwrap();
        std::fs::write(
            projcache.join("session-d2b4cf00.json"),
            r#"{"record":{"identity":{"formatVersion":4,"createdAt":1,"cwd":"C:\\w"},"rows":{"tokenUsage":{"val":{"totals":{"uncachedInputTokens":12200000,"outputTokens":201600,"cacheReadTokens":1000,"cacheWriteTokens":0}}}}}}"#,
        )
        .unwrap();
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());

        let adapter = DshAdapter;
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1, "the stream bills; the projection is not a source");
        let outcome = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 2, "the usage-less message is not billable");

        let first = &outcome.events[0];
        assert_eq!(first.session, "session-d2b4cf00");
        assert_eq!(
            first.counts,
            TokenCounts { input: 12_000_000.0, cache_creation: 0.0, cache_read: 0.0, output: 200_000.0, reasoning: 0.0, credits: 0.0 }
        );
        assert_eq!(first.model.as_deref(), Some("xhs"), "carried from the v4 request/header");
        assert_eq!(first.project.as_deref(), Some("wifitouch"));
        assert_eq!(first.dedupe_key.as_deref(), Some("session-d2b4cf00#51"));
        assert_eq!(first.ts_ms, 1_791_900_005_000, "the call's own day, not the session's last touch");

        // The second call lands days later, under the model the newer header
        // named — the two days stay two days, which is the whole point.
        let second = &outcome.events[1];
        assert_eq!(second.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(second.ts_ms, 1_792_500_006_000);
        assert_eq!(
            second.counts,
            TokenCounts { input: 50_000.0, cache_creation: 0.0, cache_read: 1_000.0, output: 1_600.0, reasoning: 0.0, credits: 0.0 }
        );
        let keys: BTreeSet<_> = outcome.events.iter().filter_map(|e| e.dedupe_key.clone()).collect();
        assert_eq!(keys.len(), 2);
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
    }
}
