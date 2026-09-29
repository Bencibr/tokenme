//! Black-box tests for the `tokenme` binary. They avoid the real adapters: the
//! cases here either exit before probing or use `--no-ingest`/`--status`, so a
//! half-finished adapter cannot break the CLI's own contract.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::process::{Command, Output, Stdio};

fn tokenme() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tokenme"))
}

/// A database of this process's own. Two `cargo test` runs on one fixed path (or a
/// leftover from an aborted one) make SQLite answer "disk I/O error", which reads
/// like a product failure but is a harness collision.
fn temp_db(name: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "tokenme-cli-test-{name}-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    remove_db(&path);
    path
}

/// A database and its WAL sidecars, which SQLite leaves behind on an abrupt exit.
fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn run(args: &[&str]) -> Output {
    tokenme()
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tokenme builds and runs")
        .wait_with_output()
        .expect("tokenme exited")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn help_is_printable() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for cmd in ["detect", "daily", "weekly", "monthly", "report", "sessions", "quota", "index", "budget", "pricing"] {
        assert!(text.contains(cmd), "help omits {cmd}");
    }
    assert!(text.contains("--pricing-override"), "help omits global flags");
}

#[test]
fn no_arguments_prints_help() {
    let out = run(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Usage:"), "expected help, got {:?}", stdout(&out));
}

#[test]
fn an_unopenable_index_is_exit_1() {
    // `/proc` is a useful Unix fixture, but Windows treats that spelling as a
    // relative filename and SQLite can create it.  A directory is not a valid
    // SQLite database on either platform and gives the same open failure.
    let blocked = std::env::temp_dir().join(format!(
        "tokenme-cli-unopenable-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir(&blocked);
    std::fs::create_dir(&blocked).expect("create an unopenable database directory");
    let blocked = blocked.to_string_lossy().into_owned();
    let out = run(&["--db", &blocked, "index", "--status"]);
    let _ = std::fs::remove_dir(&blocked);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("cannot open index"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "nothing may reach stdout on failure");
}

#[test]
fn an_unknown_tool_is_rejected() {
    let out = run(&["--tool", "not-a-tool", "detect"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown --tool"), "{}", stderr(&out));
}

#[test]
fn a_malformed_price_override_is_rejected() {
    let out = run(&["--pricing-override", "glm-5.2", "report", "--no-ingest"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("--pricing-override"), "{}", stderr(&out));
}

#[test]
fn a_malformed_date_is_rejected() {
    let out = run(&["--since", "last-tuesday", "daily", "--no-ingest"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("YYYY-MM-DD"), "{}", stderr(&out));
}

#[test]
fn index_status_on_a_fresh_db_is_valid_json() {
    let db = temp_db("status");
    let out = run(&["--db", &db.to_string_lossy(), "--offline", "index", "--status", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("--json parses");
    assert_eq!(value["total_events"], 0);
    assert_eq!(value["files_tracked"], 0);
    assert_eq!(value["ingest"], serde_json::Value::Null);
    assert_eq!(value["errors"].as_array().map(Vec::len), Some(0));
    remove_db(&db);
}

#[test]
fn periods_on_an_empty_index_render_zero_rows() {
    let db = temp_db("periods");
    let out = run(&[
        "--db",
        &db.to_string_lossy(),
        "--offline",
        "--json",
        "--no-ingest",
        "daily",
        "--days",
        "3",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json parses");
    assert_eq!(value["grain"], "day");
    assert_eq!(value["rows"].as_array().map(Vec::len), Some(3));
    assert_eq!(value["rows"][0]["requests"], 0);
    assert_eq!(value["totals"]["total_tokens"], 0.0);
    remove_db(&db);

    // The same state in a terminal renders a table with an explicit total row.
    let text = run(&["--db", &db.to_string_lossy(), "--offline", "--no-ingest", "daily", "--days", "3"]);
    let rendered = stdout(&text);
    assert!(rendered.contains("Day"), "{rendered}");
    assert!(rendered.contains("total"), "{rendered}");
    assert!(rendered.contains("prices:"), "the price source must be stated: {rendered}");
}

#[test]
fn report_json_is_the_whole_usage_core_report() {
    let db = temp_db("report");
    let out = run(&[
        "--db",
        &db.to_string_lossy(),
        "--offline",
        "--json",
        "--no-ingest",
        "report",
        "--window",
        "week",
        "--group",
        "model",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("report parses");
    // The panel and the CLI agree because this is `usage_core::Report`, field for field.
    for key in ["day", "week", "month", "heatmap", "quotas", "sources", "pricing", "recent_sessions", "all_time"] {
        assert!(value.get(key).is_some(), "Report.{key} missing");
    }
    assert_eq!(value["heatmap"].as_array().map(Vec::len), Some(371));
    assert_eq!(value["day"]["summary"]["requests"], 0);
    remove_db(&db);
}

// ------------------------------------------------------------------- pricing

/// A price is a pick between listings, so the pick has to be inspectable — that
/// is what `pricing explain` is for, and it must work with no index at all.
#[test]
fn a_forced_price_names_itself_as_the_source() {
    let out = run(&[
        "--offline",
        "--json",
        "--pricing-override",
        "glm-9.9=1/2/0.5/0.1",
        "pricing",
        "explain",
        "glm-9.9",
        "not-a-model-anywhere",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json parses");
    assert_eq!(v[0]["chosen"]["provider"], "override", "a forced price is not a models.dev listing");
    assert_eq!(v[0]["chosen"]["input"], 1.0);
    assert_eq!(v[0]["beaten"].as_array().map(Vec::len), Some(0), "nothing was contested");
    assert_eq!(v[1]["chosen"], serde_json::Value::Null, "an unknown id has no price, not a $0 one");
    assert!(v[0]["pricing"]["key_count"].as_u64().unwrap_or(0) > 0, "the table it came from is stated");
}

#[test]
fn explain_needs_a_model_to_explain() {
    let out = run(&["--offline", "pricing", "explain"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("at least one model"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "nothing may reach stdout on failure");
}

/// The error bar on the money column, on an index with nothing in it.
#[test]
fn contested_pricing_of_an_empty_index_is_an_empty_answer() {
    let db = temp_db("contested");
    let out = run(&[
        "--db",
        &db.to_string_lossy(),
        "--offline",
        "--json",
        "--no-ingest",
        "pricing",
        "contested",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json parses");
    assert_eq!(v["rows"].as_array().map(Vec::len), Some(0));
    assert_eq!(v["cost_in_rows"], 0.0);
    assert_eq!(v["max_worst_delta"], 0.0);

    let text = run(&["--db", &db.to_string_lossy(), "--offline", "--no-ingest", "pricing", "contested"]);
    assert!(stdout(&text).contains("nothing contested"), "{}", stdout(&text));
    remove_db(&db);
}

/// Icons are decoration, so the contract is only that the answer is a map of
/// data URLs — and that a machine or platform with no bundles yields `{}` rather
/// than an error, since the panel then draws monograms everywhere.
#[test]
fn icon_urls_are_a_map_of_data_urls_or_nothing() {
    let out = run(&["--offline", "--json", "icons"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json parses");
    let map = v.as_object().expect("an object keyed by tool");
    for (tool, url) in map {
        let url = url.as_str().unwrap_or_default();
        assert!(url.starts_with("data:image/png;base64,"), "{tool}: {url}");
    }
}
