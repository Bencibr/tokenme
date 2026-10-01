//! `probe` / `discover` against a synthetic `$OPENCODE_DATA_DIR`.

use std::sync::Mutex;

mod common;

use common::*;
use usage_adapter_opencode::{Crow5Adapter, MimocodeAdapter, OpenCodeAdapter};
use usage_core::{DateFilter, FileKind, SourceAdapter};

/// `set_var` is process-global, so this binary runs its tests one at a time.
static DIR_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn probe_needs_the_file_and_the_message_table() {
    let _g = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let a = OpenCodeAdapter;

    // Nothing there at all.
    std::env::set_var("OPENCODE_DATA_DIR", tmp.path().join("absent"));
    assert!(a.probe().is_none());
    assert!(a.discover(&DateFilter::default()).is_empty());

    // A database without the table we read: detected roots exist, but the source
    // is not offered, and a read still returns Ok.
    let hollow = tmp.path().join("hollow");
    std::fs::create_dir_all(&hollow).unwrap();
    let _ = Connection::open(hollow.join("opencode.db"));
    std::env::set_var("OPENCODE_DATA_DIR", &hollow);
    assert!(a.probe().is_none(), "no `message` table means no usage to show");
    let files = a.discover(&DateFilter::default());
    assert_eq!(files.len(), 1);
    assert!(a.read(&files[0], usage_core::ReadCursor(0)).unwrap().events.is_empty());

    // The real shape.
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/a");
    message(&conn, "a1", "ses_1", 1787799862846, &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"));
    message(&conn, "a2", "ses_1", 1787799862900, &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"));
    drop(conn);
    std::env::set_var("OPENCODE_DATA_DIR", d.dir.path());

    let det = a.probe().expect("opencode detected");
    assert_eq!(det.id, "opencode");
    assert_eq!(det.display, "OpenCode");
    assert_eq!(det.roots, vec![d.dir.path().to_path_buf()]);
    assert_eq!(det.hint.as_deref(), Some("2 messages"));

    let files = a.discover(&DateFilter::default());
    assert_eq!(files.len(), 1, "one database holds every period");
    assert_eq!(files[0].kind, FileKind::Sqlite);
    assert_eq!(files[0].path, d.path);
    assert!(files[0].size > 0 && files[0].unchanged_since(files[0].size, files[0].mtime_ms));
    // A future window cannot prune a single-file source; the cursor does the work.
    assert_eq!(a.discover(&DateFilter::new(Some(i64::MAX / 2), None)).len(), 1);

    std::env::remove_var("OPENCODE_DATA_DIR");
}

/// Crow5 keeps its history and its live store as two readable `*.db` files (the
/// live one renamed on every upgrade), so *both* are sources: picking by mtime
/// would drop the other's messages, which are disjoint row ids. A foreign store
/// and a WAL sidecar in the same directory must not join the list, and `probe`
/// has to name what it read.
#[test]
fn crow5_resolves_its_versioned_database_and_names_it() {
    let _g = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("crow5");
    std::fs::create_dir_all(&dir).unwrap();

    // The upgraded-away store, with more rows than the live one: the order is by
    // mtime (a stable listing), but neither file is dropped.
    let stale = db_named(&dir, "opencode-powerformer-v1.17.0.db", false);
    let live = db_named(&dir, "opencode-powerformer-v1.18.1.db", false);
    seed_messages(&stale, 5);
    seed_messages(&live, 2);
    set_mtime(&stale, 1_700_000_000);
    set_mtime(&live, 1_800_000_000);
    // A sidecar and a foreign store are what a real product dir also contains.
    std::fs::write(dir.join("opencode-powerformer-v1.18.1.db-wal"), b"sidecar").unwrap();
    let _ = Connection::open(dir.join("telemetry.db")).unwrap();
    set_mtime(&dir.join("telemetry.db"), 1_900_000_000);

    std::env::set_var("CROW5_DATA_DIR", &dir);
    let det = Crow5Adapter.probe().expect("crow5 detected");
    assert_eq!(det.id, "crow5");
    assert_eq!(det.display, "Crow5");
    assert_eq!(det.roots, vec![dir.clone()]);
    assert_eq!(
        det.hint.as_deref(),
        Some("7 messages in 2 stores (opencode-powerformer-v1.18.1.db, opencode-powerformer-v1.17.0.db)"),
        "the hint names every store that was read"
    );

    let files = Crow5Adapter.discover(&DateFilter::default());
    assert_eq!(files.len(), 2, "both stores are sources: {files:?}");
    assert_eq!(files[0].path, live, "newest first, so the listing is stable");
    assert_eq!(files[1].path, stale);
    assert_eq!(files[0].kind, FileKind::Sqlite);
    let mut events = 0;
    for file in &files {
        let out = Crow5Adapter
            .read(file, usage_core::ReadCursor(0))
            .expect("read is infallible by contract");
        events += out.events.len();
        assert!(
            out.events.iter().all(|e| e.tool == "crow5"),
            "events from Crow5's file must not be billed to opencode"
        );
    }
    assert_eq!(events, 7, "5 in the old store + 2 in the live one");
    std::env::remove_var("CROW5_DATA_DIR");
}

/// Mimocode keeps a stable file name, so it is detected the way OpenCode is and
/// its hint carries no file name: only a versioned product has a choice to report.
#[test]
fn mimocode_is_detected_by_its_stable_file_name() {
    let _g = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("mimocode");
    std::fs::create_dir_all(&dir).unwrap();
    let db = db_named(&dir, "mimocode.db", true);
    seed_messages(&db, 3);

    std::env::set_var("MIMOCODE_DATA_DIR", &dir);
    let det = MimocodeAdapter.probe().expect("mimocode detected");
    assert_eq!((det.id.as_str(), det.display.as_str()), ("mimocode", "Mimocode"));
    assert_eq!(det.hint.as_deref(), Some("3 messages"));
    assert_eq!(MimocodeAdapter.discover(&DateFilter::default())[0].path, db);
    std::env::remove_var("MIMOCODE_DATA_DIR");

    // Siblings do not see each other's installs.
    let a = OpenCodeAdapter;
    std::env::set_var("OPENCODE_DATA_DIR", &dir);
    assert!(a.probe().is_none(), "opencode.db is not mimocode.db");
    std::env::remove_var("OPENCODE_DATA_DIR");
}

/// One project, one session and `n` billable assistant rows in a database that
/// already exists at `path`.
fn seed_messages(path: &std::path::Path, n: i64) {
    let conn = Connection::open(path).unwrap();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/a");
    for i in 1..=n {
        message(
            &conn,
            &format!("a{i}"),
            "ses_1",
            1_787_799_862_846 + i,
            &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"),
        );
    }
}

use rusqlite::Connection;
