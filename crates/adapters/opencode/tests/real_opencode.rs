//! One real-data smoke pass over this machine's `~/.local/share/opencode/opencode.db`.
//! Run with: cargo test -p usage-adapter-opencode -- --ignored --nocapture

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use rusqlite::{Connection, OpenFlags};
use usage_adapter_opencode::{Crow5Adapter, MimocodeAdapter, OpenCodeAdapter};
use usage_core::{DateFilter, ReadCursor, SourceAdapter, SourceFile};

#[test]
#[ignore = "reads ~/.local/share/opencode, which only exists with OpenCode installed"]
fn real_opencode_database() {
    let a = OpenCodeAdapter;
    let det = a.probe().expect("OpenCode detected on this machine");
    eprintln!("probe: {} roots={:?} hint={:?}", det.display, det.roots, det.hint);

    let files = a.discover(&DateFilter::default());
    assert_eq!(files.len(), 1);
    let file = &files[0];
    eprintln!("discover: {} ({:.1} MiB)", file.path.display(), file.size as f64 / (1 << 20) as f64);

    // Exactly what the indexer does: loop until the cursor stops advancing.
    let mut cursor = ReadCursor(0);
    let mut events = 0usize;
    let mut tokens = 0.0f64;
    let mut models: HashSet<String> = HashSet::new();
    let mut no_model = 0usize;
    let mut projects = HashSet::new();
    let mut batches = 0usize;
    let started = Instant::now();
    loop {
        let out = a.read(file, cursor).expect("read is infallible");
        batches += 1;
        events += out.events.len();
        for e in &out.events {
            tokens += e.counts.total();
            match &e.model {
                Some(m) => {
                    models.insert(m.clone());
                }
                None => no_model += 1,
            }
            if let Some(p) = &e.project {
                projects.insert(p.clone());
            }
        }
        if out.cursor == cursor {
            break;
        }
        cursor = out.cursor;
        assert!(batches < 10_000, "the cursor never stopped advancing");
    }
    eprintln!(
        "read: {events} events in {batches} batches, {tokens:.0} tokens, {} distinct models, {no_model} without a model, {} projects in {:?}",
        models.len(),
        projects.len(),
        started.elapsed()
    );
    let mut models: Vec<_> = models.into_iter().collect();
    models.sort();
    eprintln!("models: {models:?}");
    // The cursor should have walked the whole table; a very recent placeholder
    // zero (created seconds ago) is intentionally parked and retried next pass,
    // so the cursor may sit one row before the absolute max.
    let max_rowid = {
        let uri = format!("file:{}?mode=ro", file.path.display());
        let conn = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI).unwrap();
        conn.query_row("SELECT max(rowid) FROM message", [], |r| r.get::<_, i64>(0)).unwrap_or(0)
    };
    assert!((max_rowid - cursor.0 as i64).abs() <= 2, "cursor {cursor:?} should be at/near table end (max {max_rowid})");
    assert!(events > 1_000, "expected a real backlog, got {events}");
    assert!(tokens > 1.0e6, "expected over a million tokens, got {tokens}");
}

/// Crow5 on this machine keeps two OpenCode-dialect stores in one directory, so
/// this exercises both halves of the rule: the file the glob resolves to, and the
/// versioned file the sibling was first found in.
#[test]
#[ignore = "reads ~/.local/share/crow5, which only exists with Crow5 installed"]
fn real_crow5_databases_match_a_direct_sql_recomputation() {
    let dir = dirs::home_dir().unwrap().join(".local").join("share").join("crow5");
    std::env::set_var("CROW5_DATA_DIR", &dir);
    let files = Crow5Adapter.discover(&DateFilter::default());
    if files.is_empty() {
        eprintln!("no readable *.db under {}: nothing to smoke test", dir.display());
        std::env::remove_var("CROW5_DATA_DIR");
        return;
    }
    let resolved = files[0].path.clone();
    let detected = Crow5Adapter.probe().expect("the resolved file is detectable");
    eprintln!(
        "crow5 probe: hint {:?} — the glob resolved {} to {}",
        detected.hint,
        dir.display(),
        resolved.display()
    );

    cross_check(&Crow5Adapter, "crow5", &resolved);
    // The versioned store the dialect was first verified against, read through
    // the same adapter even when the glob has moved on to a newer file.
    let versioned = dir.join("opencode-powerformer-v1.18.1.db");
    if versioned != resolved && versioned.is_file() {
        cross_check(&Crow5Adapter, "crow5", &versioned);
    }
    std::env::remove_var("CROW5_DATA_DIR");
}

#[test]
#[ignore = "reads ~/.local/share/mimocode, which only exists with Mimocode installed"]
fn real_mimocode_database_matches_a_direct_sql_recomputation() {
    let dir = dirs::home_dir().unwrap().join(".local").join("share").join("mimocode");
    std::env::set_var("MIMOCODE_DATA_DIR", &dir);
    let Some(file) = MimocodeAdapter.discover(&DateFilter::default()).into_iter().next() else {
        eprintln!("no mimocode.db under {}: nothing to smoke test", dir.display());
        std::env::remove_var("MIMOCODE_DATA_DIR");
        return;
    };
    eprintln!("mimocode probe: {:?}", MimocodeAdapter.probe().map(|d| d.hint));
    cross_check(&MimocodeAdapter, "mimocode", &file.path);
    std::env::remove_var("MIMOCODE_DATA_DIR");
}

/// One database through the adapter, and the same totals recomputed by SQL that
/// was written from the documented `message.data` shape rather than from the
/// parser. Both are printed so a disagreement is measurable, not just red.
fn cross_check(adapter: &dyn SourceAdapter, tool: &str, path: &Path) {
    let started = Instant::now();
    let ours = through_the_adapter(adapter, tool, path);
    let direct = recomputed_in_sql(path);
    eprintln!(
        "{tool} {} adapter: rows={} input={:.0} cache_creation={:.0} cache_read={:.0} output={:.0} reasoning={:.0} in {:?}",
        path.display(),
        ours.rows,
        ours.input,
        ours.cache_creation,
        ours.cache_read,
        ours.output,
        ours.reasoning,
        started.elapsed()
    );
    eprintln!(
        "{tool} {}    sql: rows={} input={:.0} cache_creation={:.0} cache_read={:.0} output={:.0} reasoning={:.0}",
        path.display(),
        direct.rows,
        direct.input,
        direct.cache_creation,
        direct.cache_read,
        direct.output,
        direct.reasoning,
    );
    assert_eq!(
        format!("{ours:?}"),
        format!("{direct:?}"),
        "the adapter's stages must equal a direct SQL recomputation of {tool}"
    );
    assert!(ours.rows > 0, "{tool} at {} produced nothing", path.display());
}

/// What the adapter reports: one event per billable assistant row.
fn through_the_adapter(adapter: &dyn SourceAdapter, tool: &str, path: &Path) -> Stages {
    let meta = std::fs::metadata(path).unwrap();
    let file = SourceFile {
        path: path.to_path_buf(),
        kind: usage_core::FileKind::Sqlite,
        size: meta.len(),
        mtime_ms: 0,
    };
    let mut cursor = ReadCursor(0);
    let mut out = Stages::default();
    let mut ids = HashSet::new();
    loop {
        let batch = adapter.read(&file, cursor).expect("read is infallible");
        for e in &batch.events {
            assert_eq!(e.tool, tool, "a sibling must never be billed as another product");
            out.rows += 1;
            out.input += e.counts.input;
            out.cache_creation += e.counts.cache_creation;
            out.cache_read += e.counts.cache_read;
            out.output += e.counts.output;
            out.reasoning += e.counts.reasoning;
            assert!(
                ids.insert(e.dedupe_key.clone()),
                "duplicate dedupe_key {:?}",
                e.dedupe_key
            );
        }
        if batch.cursor == cursor {
            break;
        }
        cursor = batch.cursor;
        assert!(out.rows < 1_000_000, "the cursor never stopped advancing");
    }
    out
}/// The same arithmetic, written independently against the JSON in SQL: the four
/// stages clamped at zero, reasoning folded into output, and a row that adds up
/// to nothing contributing nothing (which is what `parse_message` drops).
#[rustfmt::skip]
fn recomputed_in_sql(path: &Path) -> Stages {
    const SELECT: &str = "SELECT count(*), coalesce(sum(i),0), coalesce(sum(cw),0), coalesce(sum(cr),0), coalesce(sum(o),0), coalesce(sum(r),0) FROM ( \
        SELECT max(0, coalesce(json_extract(data, '$.tokens.input'), 0))        AS i, \
               max(0, coalesce(json_extract(data, '$.tokens.cache.write'), 0))  AS cw, \
               max(0, coalesce(json_extract(data, '$.tokens.cache.read'), 0))   AS cr, \
               max(0, coalesce(json_extract(data, '$.tokens.output'), 0)) \
             + max(0, coalesce(json_extract(data, '$.tokens.reasoning'), 0))    AS o, \
               max(0, coalesce(json_extract(data, '$.tokens.reasoning'), 0))    AS r \
          FROM message \
         WHERE CASE WHEN json_valid(data) THEN json_extract(data, '$.role') ELSE NULL END = 'assistant') \
       WHERE i + cw + cr + o > 0";
    let uri = format!("file:{}?mode=ro", path.display());
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("the live database opens read-only");
    conn.query_row(SELECT, [], |row| {
        Ok(Stages {
            rows: row.get(0)?,
            input: row.get::<_, f64>(1)?,
            cache_creation: row.get::<_, f64>(2)?,
            cache_read: row.get::<_, f64>(3)?,
            output: row.get::<_, f64>(4)?,
            reasoning: row.get::<_, f64>(5)?,
        })
    })
    .expect("the recomputation query runs")
}

/// Per-stage sums, compared as a debug string so a failure names the stage.
#[derive(Debug, Default)]
struct Stages {
    rows: i64,
    input: f64,
    cache_creation: f64,
    cache_read: f64,
    output: f64,
    reasoning: f64,
}
