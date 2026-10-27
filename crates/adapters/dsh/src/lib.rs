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
mod speed;

/// The accuracy audit surface: doctor::ledger reads this.

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile,
};

pub const TOOL_ID: &str = "dsh";

pub use doctor::ledger;
pub use paths::diagnostic_candidates;
pub use speed::{read_speed_samples, DshSpeedSample};
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

    /// A resumed session's stream bills fewer calls once the seed marker lands
    /// (see [`parser`]), and a second resume adds a marker that retires calls
    /// billed in the meantime — so every change re-reads from scratch and the
    /// old rows go with it.
    fn shrinks_on_reread(&self) -> bool {
        true
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

    /// A resumed session: the writer forks the stream, replays the parent's
    /// calls verbatim and marks the replay's end with `session/end-seed`
    /// `{"inherited": true}`. Both files on disk, the same calls must not bill
    /// twice (measured live 2026-10-27: fork 0d3816f9 replayed all 269 calls of
    /// parent a2c6077d, and the day read 144.31M against a true 87.0M).
    fn write_fork_pair() -> (Vec<u8>, Vec<u8>) {
        let parent = [
            r#"{"type":"session","version":4,"id":"session-parent","createdAt":1793070000000,"cwd":"/Users/dev/workspace/neuro","isSeeded":false,"delegationDepth":0}"#,
            r#"{"type":"request/header","seq":11,"time":1793070010000,"data":{"header":{"config":{"provider":"deepseek-official","model":"deepseek-v4-flash"}}}}"#,
            r#"{"type":"assistant/message","seq":22,"time":1793070020000,"data":{"turn":1,"step":1,"usage":{"inputTokens":1000,"outputTokens":10,"cacheReadTokens":9000,"totalTokens":10010}}}"#,
            r#"{"type":"assistant/message","seq":33,"time":1793070030000,"data":{"turn":1,"step":2,"usage":{"inputTokens":200,"outputTokens":20,"cacheReadTokens":10000,"totalTokens":10220}}}"#,
            r#"{"type":"turn/end","seq":40,"time":1793070031000,"data":{"turn":1}}"#,
        ];
        let fork = [
            r#"{"type":"session","version":4,"id":"session-fork","createdAt":1793070760830,"cwd":"/Users/dev/workspace/neuro","parentSession":"session-parent","isSeeded":true,"delegationDepth":0}"#,
            r#"{"type":"request/header","seq":11,"time":1793070010000,"data":{"header":{"config":{"provider":"deepseek-official","model":"deepseek-v4-flash"}}}}"#,
            r#"{"type":"assistant/message","seq":22,"time":1793070020000,"data":{"turn":1,"step":1,"usage":{"inputTokens":1000,"outputTokens":10,"cacheReadTokens":9000,"totalTokens":10010}}}"#,
            r#"{"type":"assistant/message","seq":33,"time":1793070030000,"data":{"turn":1,"step":2,"usage":{"inputTokens":200,"outputTokens":20,"cacheReadTokens":10000,"totalTokens":10220}}}"#,
            r#"{"type":"turn/end","seq":40,"time":1793070031000,"data":{"turn":1}}"#,
            r#"{"type":"session/end-seed","seq":41,"time":1793070431848,"data":{"inherited":true}}"#,
            r#"{"type":"assistant/message","seq":52,"time":1793070780000,"data":{"turn":2,"step":1,"usage":{"inputTokens":300,"outputTokens":30,"cacheReadTokens":11000,"totalTokens":11330}}}"#,
        ];
        (
            zstd::stream::encode_all(parent.join("\n").as_bytes(), 3).unwrap(),
            zstd::stream::encode_all(fork.join("\n").as_bytes(), 3).unwrap(),
        )
    }

    fn write_fork_tree(dir: &std::path::Path, parent: Option<&[u8]>, fork: &[u8]) -> std::path::PathBuf {        let workspace = dir.join("sessions/--Users-dev-workspace-neuro--");
        let fork_path = workspace.join("session-fork/session.v4.jsonl.zstd");
        std::fs::create_dir_all(fork_path.parent().unwrap()).unwrap();
        std::fs::write(&fork_path, fork).unwrap();
        if let Some(bytes) = parent {
            let parent_path = workspace.join("session-parent/session.v4.jsonl.zstd");
            std::fs::create_dir_all(parent_path.parent().unwrap()).unwrap();
            std::fs::write(&parent_path, bytes).unwrap();
        }
        fork_path
    }

    fn read_one(path: &std::path::Path) -> Vec<usage_core::UsageEvent> {
        let file = SourceFile {
            path: path.to_path_buf(),
            kind: FileKind::Tree,
            size: std::fs::metadata(path).unwrap().len(),
            mtime_ms: 0,
        };
        loader::read(&file, ReadCursor(0)).unwrap().events
    }

    #[test]
    fn a_fork_does_not_bill_its_parents_replayed_calls() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let (parent, fork) = write_fork_pair();
        let fork_path = write_fork_tree(dir.path(), Some(&parent), &fork);
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());

        let parent_events = read_one(&dir.path().join("sessions/--Users-dev-workspace-neuro--/session-parent/session.v4.jsonl.zstd"));
        assert_eq!(parent_events.len(), 2, "the parent bills its own calls");

        let fork_events = read_one(&fork_path);
        assert_eq!(fork_events.len(), 1, "the replayed range belongs to the parent");
        assert_eq!(fork_events[0].session, "session-fork");
        assert_eq!(fork_events[0].dedupe_key.as_deref(), Some("session-fork#52"));
        // The replay's own `request/header` lines still feed the parser, so the
        // fork's first call carries the model the session was billed under.
        assert_eq!(fork_events[0].model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(fork_events[0].counts, TokenCounts { input: 300.0, cache_creation: 0.0, cache_read: 11_000.0, output: 30.0, reasoning: 0.0, credits: 0.0 });

        // Family-wide totals: 2 parent calls + 1 fork call, never 4.
        let total = parent_events.iter().chain(fork_events.iter())
            .map(|e| e.counts.total())
            .sum::<f64>();
        assert_eq!(total, 10_010.0 + 10_220.0 + 11_330.0);
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
    }

    #[test]
    fn a_fork_whose_parent_is_gone_keeps_the_replay() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let (_parent, fork) = write_fork_pair();
        let fork_path = write_fork_tree(dir.path(), None, &fork);
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());

        let fork_events = read_one(&fork_path);
        assert_eq!(fork_events.len(), 3, "the replay is the only copy of that spend");
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
    }

    /// A resumed session can be re-homed under another workspace slug; the
    /// parent still owns the replay, so the fallback lookup must find it there.
    #[test]
    fn a_parent_in_a_peer_workspace_dir_still_absorbs_the_replay() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let (parent, fork) = write_fork_pair();
        let fork_path = dir
            .path()
            .join("sessions/--Users-dev-workspace-moved--/session-fork/session.v4.jsonl.zstd");
        let parent_path = dir
            .path()
            .join("sessions/--Users-dev-workspace-neuro--/session-parent/session.v4.jsonl.zstd");
        for path in [&fork_path, &parent_path] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&fork_path, &fork).unwrap();
        std::fs::write(&parent_path, &parent).unwrap();
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());

        assert_eq!(read_one(&fork_path).len(), 1, "peer workspace, same family, one owner");
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
    }

    /// A seeded header with no `inherited` marker at all: nothing separates the
    /// replay from the fork's own calls, so the safe answer is to bill the whole
    /// stream — never to bill nothing (measured 2026-10-27: every fork on this
    /// machine carried a marker, but the writer's older builds may not).
    #[test]
    fn a_seeded_stream_without_a_marker_bills_every_call() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let (parent, _fork) = write_fork_pair();
        let unmarked = [
            r#"{"type":"session","version":4,"id":"session-fork","createdAt":1793070760830,"cwd":"/Users/dev/workspace/neuro","parentSession":"session-parent","isSeeded":true,"delegationDepth":0}"#,
            r#"{"type":"assistant/message","seq":22,"time":1793070020000,"data":{"turn":1,"step":1,"usage":{"inputTokens":1000,"outputTokens":10,"cacheReadTokens":9000}}}"#,
            r#"{"type":"assistant/message","seq":33,"time":1793070030000,"data":{"turn":1,"step":2,"usage":{"inputTokens":200,"outputTokens":20,"cacheReadTokens":10000}}}"#,
        ];
        let fork = zstd::stream::encode_all(unmarked.join("\n").as_bytes(), 3).unwrap();
        let fork_path = write_fork_tree(dir.path(), Some(&parent), &fork);
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());

        assert_eq!(read_one(&fork_path).len(), 2, "no boundary means no skip");
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
    }

    #[test]
    fn a_fork_of_a_v3_parent_still_bills_its_own_calls() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        // No `isSeeded`/`parentSession` on a v3 header: nothing is replayed.
        let bytes = zstd::stream::encode_all(
            [
                r#"{"type":"session","version":3,"id":"session-v3","cwd":"/Users/dev/workspace/neuro"}"#,
                r#"{"type":"assistant/chunk","seq":30,"time":1793070020000,"data":{"chunk":{"type":"usage","usage":{"inputTokens":1000,"outputTokens":10,"cacheReadTokens":0}}}}"#,
            ]
            .join("\n")
            .as_bytes(),
            3,
        )
        .unwrap();
        let path = dir.path().join("sessions/--Users-dev-workspace-neuro--/session-v3/session.jsonl.zstd");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());

        assert_eq!(read_one(&path).len(), 1);
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
    }

    /// A fork resumed a second time carries a SECOND inherited marker, and the
    /// calls between the two are the parent's as well (measured live 2026-10-27:
    /// fork `0f9b6ecd`'s 19 calls after its first marker were all present in the
    /// parent's stream, `35e1d11f` 466 of 487). Only the last marker ends the
    /// replay — stopping at the first one leaves that stretch double-billed.
    #[test]
    fn a_second_seed_marker_extends_the_skipped_replay() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let parent = [
            r#"{"type":"session","version":4,"id":"session-parent","createdAt":1793070000000,"cwd":"/Users/dev/workspace/neuro","isSeeded":false,"delegationDepth":0}"#,
            r#"{"type":"assistant/message","seq":22,"time":1793070020000,"data":{"turn":1,"step":1,"usage":{"inputTokens":1000,"outputTokens":10,"cacheReadTokens":9000}}}"#,
            r#"{"type":"assistant/message","seq":33,"time":1793070030000,"data":{"turn":1,"step":2,"usage":{"inputTokens":200,"outputTokens":20,"cacheReadTokens":10000}}}"#,
            r#"{"type":"assistant/message","seq":44,"time":1793070040000,"data":{"turn":2,"step":1,"usage":{"inputTokens":300,"outputTokens":30,"cacheReadTokens":11000}}}"#,
        ];
        let fork = [
            r#"{"type":"session","version":4,"id":"session-fork","createdAt":1793070760830,"cwd":"/Users/dev/workspace/neuro","parentSession":"session-parent","isSeeded":true,"delegationDepth":0}"#,
            r#"{"type":"assistant/message","seq":22,"time":1793070020000,"data":{"turn":1,"step":1,"usage":{"inputTokens":1000,"outputTokens":10,"cacheReadTokens":9000}}}"#,
            r#"{"type":"session/end-seed","seq":23,"time":1793070021000,"data":{"inherited":true}}"#,
            r#"{"type":"assistant/message","seq":33,"time":1793070030000,"data":{"turn":1,"step":2,"usage":{"inputTokens":200,"outputTokens":20,"cacheReadTokens":10000}}}"#,
            r#"{"type":"session/end-seed","seq":34,"time":1793070031000,"data":{"inherited":true}}"#,
            r#"{"type":"assistant/message","seq":55,"time":1793070790000,"data":{"turn":3,"step":1,"usage":{"inputTokens":400,"outputTokens":40,"cacheReadTokens":12000}}}"#,
        ];
        let fork_bytes = zstd::stream::encode_all(fork.join("\n").as_bytes(), 3).unwrap();
        let parent_bytes = zstd::stream::encode_all(parent.join("\n").as_bytes(), 3).unwrap();
        let fork_path = write_fork_tree(dir.path(), Some(&parent_bytes), &fork_bytes);
        std::env::set_var(crate::paths::ENV_DSH_HOME, dir.path());

        let fork_events = read_one(&fork_path);
        assert_eq!(fork_events.len(), 1, "both markers bound the parent's calls");
        assert_eq!(fork_events[0].dedupe_key.as_deref(), Some("session-fork#55"));
        std::env::remove_var(crate::paths::ENV_DSH_HOME);
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
