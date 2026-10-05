//! End-to-end reads against a session tree built with Kimi Code's real layouts.

use std::path::Path;
use std::sync::Mutex;

use usage_core::{FileKind, Meter, ReadCursor, SourceAdapter};
use usage_adapter_kimicode::{KimiCodeAdapter, TOOL_ID};

/// `set_var` is process-global, so these tests run one at a time.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn record(agent: &str, model: &str, input: u64, output: u64, read: u64, write: u64, ms: u64, scope: &str) -> String {
    format!(
        r#"{{"type":"usage.record","agentId":"{agent}","model":"{model}","usage":{{"inputOther":{input},"output":{output},"inputCacheRead":{read},"inputCacheCreation":{write}}},"usageScope":"{scope}","time":{ms}}}"#
    )
}

/// A session tree: one v2 session with a main agent and a subagent, a
/// legacy-layout session, and the two state files the CLI keeps beside them.
struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn tree(
    rows: &[(&str, &str, Vec<(&str, Vec<String>)>)],
    legacy: Vec<String>,
) -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("workspaces.json"),
        r#"{"version":1,"workspaces":{"wd_x":{"root":"/Users/demo/projA"}}}"#,
    )
    .unwrap();
    // (workspace, session, [(agent, file rows)]) — `("", …)` is the legacy layout.
    for (workspace, session, agents) in rows {
        let session_dir = if workspace.is_empty() {
            root.join("sessions").join("group_old").join(session)
        } else {
            root.join("sessions").join(workspace).join(session)
        };
        for (agent, lines) in agents {
            let target = if agent.is_empty() {
                session_dir.clone()
            } else {
                session_dir.join("agents").join(agent)
            };
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("wire.jsonl"), format!("{}\n", lines.join("\n"))).unwrap();
        }
    }
    if !legacy.is_empty() {
        let target = root.join("sessions").join("group_old").join("ses_v1");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("wire.jsonl"), format!("{}\n", legacy.join("\n"))).unwrap();
    }
    Tree { dir }
}

/// Everything the adapter bills from the tree the environment variable pins.
fn read_all() -> Vec<usage_core::UsageEvent> {
    let mut out = Vec::new();
    for file in KimiCodeAdapter.discover(&usage_core::DateFilter::default()) {
        let outcome = KimiCodeAdapter.read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.cursor, ReadCursor(0), "a Tree source has no cursor to advance");
        out.extend(outcome.events);
    }
    out
}

/// Both layouts, both `usageScope` values, a subagent, and a session copy: five
/// calls, one event each, and none of them attributed to the wrong session.
#[test]
fn a_real_session_tree_bills_every_call_once() {
    let _g = ENV_LOCK.lock().unwrap();
    let t = tree(
        &[
            (
                "wd_x",
                "ses_v2",
                vec![
                    (
                        "main",
                        vec![
                            r#"{"type":"metadata","protocol_version":"2","created_at":1787799860000}"#.to_string(),
                            record("main", "kimi-k2.7", 1000, 20, 40000, 500, 1787799862846, "turn"),
                            // A compaction request: `session` scope, and real spend.
                            record("main", "kimi-k2.7", 900, 300, 12000, 0, 1787799870000, "session"),
                        ],
                    ),
                    ("sub", vec![record("sub", "kimi-k2.7", 70, 10, 900, 0, 1787799871000, "turn")]),
                    // The session's own top-level log repeating one row the agent
                    // directory already carries.
                    (
                        "",
                        vec![record("main", "kimi-k2.7", 1000, 20, 40000, 500, 1787799862846, "turn")],
                    ),
                ],
            ),
        ],
        vec![
            r#"{"type":"user_input","timestamp":1787799860.5,"message":{"type":"StatusUpdate","payload":{"message_id":"msg_legacy","model":"kimi-for-coding","token_usage":{"input_other":100,"output":20,"input_cache_read":300,"input_cache_creation":5,"total":425}}}}"#.to_string(),
        ],
    );
    std::env::set_var("KIMI_DATA_DIR", t.path());

    let emitted = read_all();
    for event in &emitted {
        assert_eq!(event.tool, TOOL_ID);
        assert_eq!(event.meter, Meter::Tokens);
    }
    // The adapter hands every row to the index; the index is what collapses a key
    // it has seen. Five rows, four identities, is the whole cross-file story: the
    // session's own top-level log repeats a row the agent directory already has.
    assert_eq!(emitted.len(), 5, "every row is offered, repeats and all");
    let events = distinct(&emitted);
    assert_eq!(events.len(), 4, "one identity bills once");
    let total: f64 = events.iter().map(|e| e.counts.total()).sum();
    // 1000+20+40000+500, 900+300+12000, 70+10+900, 100+20+300+5
    assert_eq!(total, 41_520.0 + 13_200.0 + 980.0 + 425.0);

    let by_session: Vec<(&str, usize)> = {
        let mut counts = std::collections::BTreeMap::new();
        for e in &events {
            *counts.entry(e.session.as_str()).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    };
    assert_eq!(by_session, vec![("ses_v1", 1), ("ses_v2", 3)], "session attribution follows the layout");

    let v2 = events.iter().find(|e| e.session == "ses_v2").unwrap();
    assert_eq!(v2.project.as_deref(), Some("/Users/demo/projA"), "workspaces.json names the project");
    assert_eq!(v2.model.as_deref(), Some("kimi-k2.7"));
    let legacy = events.iter().find(|e| e.session == "ses_v1").unwrap();
    assert_eq!(legacy.dedupe_key.as_deref(), Some("kimicode#msg_legacy"), "the legacy id wins");
    assert_eq!(legacy.ts_ms, 1787799860000, "seconds landed as milliseconds");
    assert_eq!(legacy.counts.total(), 425.0);
    std::env::remove_var("KIMI_DATA_DIR");
}

/// The CLI rewrites this log when compaction retracts content, so the whole file
/// is offered as a `Tree` source and a re-read yields the same identities.
#[test]
fn a_rewritten_log_replays_the_same_keys_instead_of_new_ones() {
    let _g = ENV_LOCK.lock().unwrap();
    let t = tree(
        &[(
            "wd_x",
            "ses_rw",
            vec![(
                "main",
                vec![
                    record("main", "m", 10, 5, 0, 0, 1787799862846, "turn"),
                    record("main", "m", 20, 6, 0, 0, 1787799863000, "turn"),
                ],
            )],
        )],
        vec![],
    );
    std::env::set_var("KIMI_DATA_DIR", t.path());

    let files = KimiCodeAdapter.discover(&usage_core::DateFilter::default());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].kind, FileKind::Tree);
    let before: Vec<String> = keys_of(KimiCodeAdapter.read(&files[0], ReadCursor(0)).unwrap().events);

    // Compaction rewrites the file with one record removed, and the surviving one
    // at a different byte offset.
    let wire = t.path().join("sessions/wd_x/ses_rw/agents/main/wire.jsonl");
    std::fs::write(&wire, format!("{}\n", record("main", "m", 20, 6, 0, 0, 1787799863000, "turn"))).unwrap();
    let refreshed = files[0].clone().restat().expect("the file is still there");
    let after: Vec<String> = keys_of(KimiCodeAdapter.read(&refreshed, ReadCursor(0)).unwrap().events);

    assert!(before.contains(&after[0]), "the surviving record keeps its identity across a rewrite");
    assert_eq!(after.len(), 1, "and the retracted record is not minted a second key");
    std::env::remove_var("KIMI_DATA_DIR");
}

/// What `INSERT OR IGNORE … UNIQUE (dedupe_key)` does, simulated in the test.
fn distinct(events: &[usage_core::UsageEvent]) -> Vec<usage_core::UsageEvent> {
    let mut seen = std::collections::HashSet::new();
    events.iter().filter(|e| seen.insert(e.dedupe_key.clone())).cloned().collect()
}

fn keys_of(events: Vec<usage_core::UsageEvent>) -> Vec<String> {
    events.into_iter().filter_map(|e| e.dedupe_key.clone()).collect()
}

/// A model id the price table does not know is reported as it was written, so it
/// surfaces as unpriced instead of being billed as some other model.
#[test]
fn model_ids_are_passed_through_untouched() {
    let _g = ENV_LOCK.lock().unwrap();
    let t = tree(
        &[(
            "wd_x",
            "ses_m",
            vec![(
                "main",
                vec![
                    record("main", "kimi-code/kimi-for-coding", 10, 5, 0, 0, 1787799862846, "turn"),
                    record("main", "some-fork-model", 10, 5, 0, 0, 1787799863000, "turn"),
                ],
            )],
        )],
        vec![],
    );
    std::env::set_var("KIMI_DATA_DIR", t.path());
    let events = read_all();
    let models: Vec<Option<String>> = events.into_iter().map(|e| e.model).collect();
    assert_eq!(
        models,
        vec![Some("kimi-for-coding".into()), Some("some-fork-model".into())],
        "the routing prefix goes, the vendor id stays"
    );
    std::env::remove_var("KIMI_DATA_DIR");
}
