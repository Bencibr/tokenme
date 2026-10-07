//! Shared scaffolding for the OpenCode integration tests: a database with the
//! real `message` / `session` / `project` shapes.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use usage_core::{FileKind, SourceAdapter, SourceFile};

/// Verbatim from this machine's `opencode.db`, including the `cost` field the
/// adapter must ignore.
pub const ASSISTANT_SAMPLE: &str = include_str!("../fixtures/assistant_message.json");

pub struct TestDb {
    pub dir: tempfile::TempDir,
    pub path: PathBuf,
}

impl TestDb {
    pub fn conn(&self) -> Connection {
        Connection::open(&self.path).expect("writable handle for the test fixture")
    }

    pub fn source(&self) -> SourceFile {
        let meta = std::fs::metadata(&self.path).expect("db exists");
        SourceFile {
            path: self.path.clone(),
            kind: FileKind::Sqlite,
            size: meta.len(),
            mtime_ms: meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64,
        }
    }

    pub fn key(&self) -> String {
        self.source().key()
    }
}

/// `wal` mirrors the live app; a plain journal exercises the other open path.
pub fn db(wal: bool) -> TestDb {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    create_db(&path, wal);
    TestDb { dir, path }
}

/// The same schema planted under an arbitrary file name, which is how a
/// versioned product's directory has to be built (`db(wal)` can only stand for
/// OpenCode's own stable name). The caller owns the directory.
pub fn db_named(dir: &Path, name: &str, wal: bool) -> PathBuf {
    let path = dir.join(name);
    create_db(&path, wal);
    path
}

fn create_db(path: &Path, wal: bool) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE project (
          id text PRIMARY KEY, worktree text NOT NULL, vcs text, name text,
          time_created integer NOT NULL, sandboxes text NOT NULL DEFAULT '[]'
        );
        CREATE TABLE session (
          id text PRIMARY KEY, project_id text NOT NULL, slug text NOT NULL,
          directory text NOT NULL, title text NOT NULL, version text NOT NULL,
          cost real NOT NULL DEFAULT 0, tokens_input integer NOT NULL DEFAULT 0,
          tokens_output integer NOT NULL DEFAULT 0, tokens_reasoning integer NOT NULL DEFAULT 0,
          tokens_cache_read integer NOT NULL DEFAULT 0, tokens_cache_write integer NOT NULL DEFAULT 0,
          agent text, model text, time_created integer NOT NULL, time_updated integer NOT NULL
        );
        CREATE TABLE message (
          id text PRIMARY KEY, session_id text NOT NULL,
          time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL
        );
        -- The dialect's v2 record table, verbatim in shape from the live store,
        -- where OpenCode 1.18.30 keeps it empty while `message` is still written.
        CREATE TABLE session_message (
          id text PRIMARY KEY, session_id text NOT NULL, type text NOT NULL,
          seq integer NOT NULL, time_created integer NOT NULL,
          time_updated integer NOT NULL, data text NOT NULL
        );
        "#,
    )
    .unwrap();
    if wal {
        conn.execute_batch("PRAGMA journal_mode=wal;").unwrap();
    }
    drop(conn);
}

/// Moves a file's mtime, which is what the versioned-file glob ranks on.
pub fn set_mtime(path: &Path, secs: u64) {
    let handle = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    handle
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .unwrap();
}

pub fn project(conn: &Connection, id: &str, name: Option<&str>) {
    conn.execute(
        "INSERT INTO project (id, worktree, vcs, name, time_created) VALUES (?1,?2,NULL,?3,1787000000000)",
        rusqlite::params![id, format!("/work/{id}"), name],
    )
    .unwrap();
}

pub fn session(conn: &Connection, id: &str, project_id: &str, directory: &str) {
    conn.execute(
        "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
         VALUES (?1,?2,'slug',?3,'t','1.0',1787000000000,1787000000000)",
        rusqlite::params![id, project_id, directory],
    )
    .unwrap();
}

/// One `message` row; returns its rowid so cursor assertions stay exact.
pub fn message(conn: &Connection, id: &str, session_id: &str, time_created: i64, data: &str) -> i64 {
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1,?2,?3,?3,?4)",
        rusqlite::params![id, session_id, time_created, data],
    )
    .unwrap();
    conn.query_row("SELECT max(rowid) FROM message", [], |r| r.get(0)).unwrap()
}

/// One `session_message` (v2) row; returns its rowid. `type` is where the role
/// lives now, and the payload carries none.
pub fn v2_message(
    conn: &Connection,
    id: &str,
    session_id: &str,
    type_: &str,
    seq: i64,
    time_created: i64,
    data: &str,
) -> i64 {
    conn.execute(
        "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1,?2,?3,?4,?5,?5,?6)",
        rusqlite::params![id, session_id, type_, seq, time_created, data],
    )
    .unwrap();
    conn.query_row("SELECT max(rowid) FROM session_message", [], |r| r.get(0)).unwrap()
}

/// The v2 payload: the same stage numbers, no `role` key at all.
pub fn v2_assistant_data(model: &str, total: i64, input: i64, output: i64, reasoning: i64, read: i64) -> String {
    format!(
        r#"{{"agent":"build","modelID":"{model}","providerID":"opencode","cost":0.5,"time":{{"created":1787799862846}},"tokens":{{"total":{total},"input":{input},"output":{output},"reasoning":{reasoning},"cache":{{"write":0,"read":{read}}}}}}}"#
    )
}

pub fn assistant_data(model: &str, total: i64, input: i64, output: i64, reasoning: i64, read: i64, cwd: &str) -> String {
    format!(
        r#"{{"role":"assistant","modelID":"{model}","providerID":"opencode","path":{{"cwd":"{cwd}","root":"/"}},"cost":0.5,"tokens":{{"total":{total},"input":{input},"output":{output},"reasoning":{reasoning},"cache":{{"write":0,"read":{read}}}}}}}"#
    )
}

pub fn user_data(text: &str) -> String {
    format!(r#"{{"role":"user","time":{{"created":1787799862846}},"tokens":{{"total":120,"input":120,"output":0}},"text":"{text}"}}"#)
}

pub fn read_once(path: &Path, cursor: usage_core::ReadCursor) -> usage_core::ReadOutcome {
    usage_adapter_opencode::OpenCodeAdapter
        .read(&source_at(path), cursor)
        .expect("read is infallible by contract")
}

pub fn source_at(path: &Path) -> SourceFile {
    let meta = std::fs::metadata(path).unwrap();
    SourceFile {
        path: path.to_path_buf(),
        kind: FileKind::Sqlite,
        size: meta.len(),
        mtime_ms: meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64,
    }
}
