//! dsh adapter.
//!
//! One source: `<sessions-root>/<workspace>/<session>/session.jsonl.zstd` —
//! zstd-compressed JSONL event streams, one file per agent session. Usage
//! rides on `assistant/chunk` lines whose chunk is `{"type":"usage"}` with the
//! exclusive token convention (`inputTokens` excludes `cacheReadTokens`);
//! the step's model arrives earlier on `request/header`.
//!
//! The sessions roots follow the writer's own layout (see [`paths`]): the
//! macOS CLI-era tree `~/.dsh/sessions` first, the desktop app's
//! `%APPDATA%\dsh-desktop\harness\sessions` (Windows) /
//! `~/Library/Application Support/dsh-desktop/harness/sessions` (macOS) second.
//! Format v4 names the stream `session.v4.jsonl.zstd`; the header gains
//! `"version":4` and the tokens of such a session live in its projection
//! cache, not the stream. All existing roots are read, one session each: the
//! trees mirror each other by identity (workspace slug + session dir), and the
//! first root in priority order wins a mirrored session — reading a session
//! from both roots would double-bill, reading only one root hides the other's
//! sessions entirely (measured 2026-09-29: the stale `~/.dsh` tree masked the
//! live desktop app's sessions for a day).
//!
//! Cursor is `FileKind::Tree`: whole-file re-parse on change, stable
//! `<session>#<seq>` dedupe keys absorbing the re-emit.
//!
//! Owned by the dsh-adapter workstream. The public surface (`DshAdapter`,
//! `TOOL_ID`) is frozen — `usage-adapter-all` and the CLI link against it.

mod doctor;
mod loader;
mod parser;
mod proj;
mod paths;

/// The accuracy audit surface:  reads this.

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile,
};

pub const TOOL_ID: &str = "dsh";

pub use doctor::ledger;
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
}
