mod common;

/// The heatmap in `usage-core::report` spans two years (742 days — the year
/// view's last-year page); an index that forgets more than that would show
/// holes at the left edge, forever.
const _: () = assert!(usage_index::RETENTION_DAYS > usage_core::report::HEATMAP_DAYS);

use common::{adapters_for, append_fixture, count_of, replace_file, staged, Mock};
use usage_core::{CallKind, DateFilter, DetectedSource, Meter, UsageEvent};
use usage_index::{Index, RETENTION_DAYS};

fn ingest(adapter: &Mock, filter: DateFilter) -> (Index, usage_index::IngestReport) {
    let mut idx = Index::open_in_memory().unwrap();
    let report = idx.ingest_adapter(adapter, &filter).unwrap();
    assert!(idx.errors().is_empty(), "unexpected ingest errors: {:?}", idx.errors());
    (idx, report)
}

fn sorted(mut events: Vec<UsageEvent>) -> Vec<UsageEvent> {
    events.sort_by(|a, b| a.dedupe_key.cmp(&b.dedupe_key));
    events
}

#[test]
fn cold_ingest_inserts_every_event_and_the_next_pass_is_free() {
    let dir = staged(&["basic.jsonl", "second.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());
    let (mut idx, first) = ingest(&adapter, DateFilter::default());
    assert_eq!(first.files_scanned, 2);
    assert_eq!(first.files_changed, 2);
    assert_eq!(first.new_events, 7);
    assert_eq!(first.deduped, 0);
    assert_eq!(first.purged, 0);
    assert_eq!(first.total_events, 7);
    assert_eq!(count_of(&idx, "mocka"), 7);
    assert!(first.per_tool.contains_key("mocka"));

    let second = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(second.files_scanned, 2, "unchanged files are still scanned");
    assert_eq!(second.files_changed, 0);
    assert_eq!(second.new_events, 0);
    assert_eq!(second.total_events, 7, "totals survive a no-op pass");
}

#[test]
fn appending_one_line_resumes_at_the_byte_cursor() {
    let dir = staged(&["growable.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());
    let (mut idx, first) = ingest(&adapter, DateFilter::default());
    assert_eq!(first.new_events, 2);

    append_fixture(dir.path(), "growable.jsonl", "appended.jsonl");
    let second = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(second.files_changed, 1);
    assert_eq!(second.new_events, 1, "only the appended record is read");
    assert_eq!(second.purged, 0, "a grow is not a rewrite");

    let events = idx.all_events().unwrap();
    assert_eq!(events.len(), 3);
    assert!(events.iter().any(|e| e.dedupe_key.as_deref() == Some("g-3")));
    assert_eq!(events.iter().map(|e| e.counts.input).sum::<f64>(), 600.0);
}

#[test]
fn keyed_events_collapse_but_null_keys_never_collide() {
    let dir = staged(&["dupes.jsonl", "nullkey.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());
    let (mut idx, first) = ingest(&adapter, DateFilter::default());
    assert_eq!(first.new_events, 3, "one keyed row plus two unkeyed rows");
    assert_eq!(first.deduped, 2, "the two duplicate ids were ignored");
    assert_eq!(idx.event_count().unwrap(), 3);

    // Replaying a whole file (a `Tree`-shaped source ignores cursors) shows the
    // asymmetry that the partial unique index creates on purpose.
    let replay = Mock { id: "mocka", dir: dir.path().to_path_buf(), fail_on: None, ignore_cursor: true };
    // Both files change, so the replay re-reads both from byte 0.
    append_fixture(dir.path(), "dupes.jsonl", "newline.jsonl");
    append_fixture(dir.path(), "nullkey.jsonl", "newline.jsonl");
    let again = idx.ingest_adapter(&replay, &DateFilter::default()).unwrap();
    assert_eq!(again.deduped, 3, "the keyed file collapses again on replay");
    assert_eq!(again.new_events, 2, "unkeyed rows re-insert, so Codex relies on the cursor instead");
    assert_eq!(idx.event_count().unwrap(), 5);
}

#[test]
fn ingest_preserves_duplicate_calls_and_grows_by_call_multiplicity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("calls.jsonl");
    std::fs::write(
        &path,
        br#"{"id":"call-1","ts":1789172800000,"session":"s","in":1,"calls":[{"kind":"mcp","name":"tools/list"},{"kind":"mcp","name":"tools/list"}]}
"#,
    ).unwrap();
    let adapter = Mock::new("mocka", dir.path());
    let mut idx = Index::open_in_memory().unwrap();

    let first = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(first.new_events, 1);
    assert_eq!(idx.all_events().unwrap()[0].calls.len(), 2, "two real invocations must remain two rows");

    let replay = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(replay.files_changed, 0);
    assert_eq!(idx.all_events().unwrap()[0].calls.len(), 2, "a no-op ingest must not add a duplicate");

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(br#"{"id":"call-1","ts":1789172800000,"session":"s","in":1,"calls":[{"kind":"mcp","name":"tools/list"},{"kind":"mcp","name":"tools/list"},{"kind":"mcp","name":"tools/list"}]}
"#).unwrap();
    file.flush().unwrap();
    let growth = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(growth.files_changed, 1);
    assert_eq!(idx.all_events().unwrap()[0].calls.len(), 3, "call-only growth must add only the new invocation");
}

#[test]
fn dirty_mirror_is_opt_in_and_keyless_state_starts_only_after_pairing_gate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keyless.jsonl");
    std::fs::write(
        &path,
        br#"{"ts":1789172800000,"session":"s","in":1}
"#,
    ).unwrap();
    let adapter = Mock::new("mocka", dir.path());
    let mut idx = Index::open_in_memory().unwrap();

    idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert!(idx.sync_dirty_events().unwrap().is_empty(), "unpaired ingest must not retain full event JSON");
    assert_eq!(idx.conn().query_row::<i64, _, _>("SELECT count(*) FROM sync_occurrence", [], |row| row.get(0)).unwrap(), 0);

    idx.conn().execute(
        "CREATE TABLE device_sync_secondary_config(config_key TEXT PRIMARY KEY, enabled INTEGER NOT NULL)",
        [],
    ).unwrap();
    idx.conn().execute(
        "INSERT INTO device_sync_secondary_config(config_key, enabled) VALUES('secondary', 1)",
        [],
    ).unwrap();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(br#"{"ts":1789172800000,"session":"s","in":1}
"#).unwrap();
    file.flush().unwrap();
    idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    let dirty = idx.sync_dirty_events().unwrap();
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].event.calls.len(), 0);
    let (dirty_event_id, dirty_json_len): (Option<i64>, i64) = idx.conn().query_row(
        "SELECT event_id, length(event_json) FROM sync_dirty LIMIT 1",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert!(dirty_event_id.is_some(), "paired ingest stores a durable event reference");
    assert_eq!(dirty_json_len, 0, "paired ingest must not duplicate the full event JSON");
    let first_revision = dirty[0].revision;
    assert!(first_revision > 0);
    assert_eq!(idx.conn().query_row::<i64, _, _>("SELECT count(*) FROM sync_occurrence", [], |row| row.get(0)).unwrap(), 1);

    // ACK removes the current dirty row, not the global counter. A later
    // ingest must not restart at revision 1, or an old ACK could hide it.
    idx.conn().execute("DELETE FROM sync_dirty", []).unwrap();
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(br#"{"ts":1789172800000,"session":"s","in":1}
"#).unwrap();
    file.flush().unwrap();
    idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    let next_revision = idx.sync_dirty_events().unwrap()[0].revision;
    assert!(next_revision > first_revision);
}

#[test]
fn sync_snapshot_pages_bound_rows_and_follow_stable_identity_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paged.jsonl");
    let mut body = String::new();
    for id in 0..520 {
        body.push_str(&format!(r#"{{"id":"page-{id:04}","ts":1789172800000,"session":"s-{id}","in":1}}
"#));
    }
    std::fs::write(&path, body).unwrap();
    let adapter = Mock::new("mocka", dir.path());
    let mut idx = Index::open_in_memory().unwrap();
    idx.conn().execute(
        "CREATE TABLE device_sync_secondary_config(config_key TEXT PRIMARY KEY, enabled INTEGER NOT NULL)",
        [],
    ).unwrap();
    idx.conn().execute(
        "INSERT INTO device_sync_secondary_config(config_key, enabled) VALUES('secondary', 1)",
        [],
    ).unwrap();
    idx.conn().execute(
        "INSERT INTO meta(key, value) VALUES('device_sync_initial_coverage_lo', '1789000000000')",
        [],
    ).unwrap();
    idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(idx.event_count().unwrap(), 520);

    let mut after_identity = None;
    let mut after_revision = 0_u64;
    let mut seen = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = idx.sync_snapshot_page(after_identity.as_deref(), after_revision, 1_800_000_000_000, 30, 64, 256 * 1024).unwrap();
        assert!(page.events.len() + page.dirty_events.len() <= 64);
        for event in &page.events {
            assert!(seen.insert(event.identity.clone()), "event identity repeated across pages");
        }
        for dirty in &page.dirty_events {
            assert!(dirty.revision > after_revision);
        }
        let progressed = page.next_identity != after_identity || page.next_dirty_revision != after_revision;
        assert!(progressed || (page.events_done && page.dirty_done));
        after_identity = page.next_identity;
        after_revision = page.next_dirty_revision;
        pages += 1;
        if page.events_done && page.dirty_done { break; }
        assert!(pages < 32, "bounded pages must not loop forever");
    }
    assert!(pages > 1);
    assert_eq!(seen.len(), 520);
}

#[test]
fn sync_identity_initialization_is_bounded_and_restart_stable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keyless.jsonl");
    let body = (0..4096).map(|_| r#"{"ts":1789172800000,"session":"s","in":1}
"#).collect::<String>();
    std::fs::write(&path, body).unwrap();
    let adapter = Mock::new("mocka", dir.path());
    let mut idx = Index::open_in_memory().unwrap();
    idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(idx.event_count().unwrap(), 4096);
    idx.conn().execute(
        "CREATE TABLE device_sync_secondary_config(config_key TEXT PRIMARY KEY, enabled INTEGER NOT NULL)",
        [],
    ).unwrap();
    idx.conn().execute(
        "INSERT INTO device_sync_secondary_config(config_key, enabled) VALUES('secondary', 1)",
        [],
    ).unwrap();

    idx.ensure_sync_occurrences().unwrap();
    let before: Vec<(i64, String, String)> = {
        let mut stmt = idx.conn().prepare("SELECT event_id, identity, occurrence_id FROM sync_event_identity ORDER BY event_id").unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, Option<String>>(2)?.unwrap_or_default())))
            .unwrap().collect::<std::result::Result<_, _>>().unwrap()
    };
    assert_eq!(before.len(), 4096);
    idx.ensure_sync_occurrences().unwrap();
    let after: Vec<(i64, String, String)> = {
        let mut stmt = idx.conn().prepare("SELECT event_id, identity, occurrence_id FROM sync_event_identity ORDER BY event_id").unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, Option<String>>(2)?.unwrap_or_default())))
            .unwrap().collect::<std::result::Result<_, _>>().unwrap()
    };
    assert_eq!(before, after, "restarting identity initialization must not renumber keyless rows");
}

#[test]
fn dirty_rows_that_fill_a_page_defer_events_to_the_next_page() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dirty-and-event.jsonl");
    std::fs::write(&path, br#"{"id":"dirty","ts":1789172800000,"session":"dirty","in":1}
{"id":"event","ts":1789172800000,"session":"event","in":1}
"#).unwrap();
    let adapter = Mock::new("mocka", dir.path());
    let mut idx = Index::open_in_memory().unwrap();
    idx.conn().execute(
        "CREATE TABLE device_sync_secondary_config(config_key TEXT PRIMARY KEY, enabled INTEGER NOT NULL)",
        [],
    ).unwrap();
    idx.conn().execute(
        "INSERT INTO device_sync_secondary_config(config_key, enabled) VALUES('secondary', 1)",
        [],
    ).unwrap();
    idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    let dirty_len: usize = idx.conn().query_row("SELECT length(event_json) FROM sync_dirty LIMIT 1", [], |row| row.get::<_, i64>(0)).unwrap() as usize;
    let identity_len: usize = idx.conn().query_row("SELECT length(identity) FROM sync_dirty LIMIT 1", [], |row| row.get::<_, i64>(0)).unwrap() as usize;
    let event_len: usize = idx.conn().query_row("SELECT length(json_object('tool', tool, 'ts_ms', ts_ms, 'session', session, 'project', project, 'model', model, 'counts', json_object('input', in_tok, 'cache_creation', cc_tok, 'cache_read', cr_tok, 'output', out_tok, 'reasoning', reason_tok, 'credits', credits), 'meter', meter, 'dedupe_key', dedupe_key, 'calls', json_array(), 'source', source)) FROM event WHERE dedupe_key = 'event'", [], |row| row.get::<_, i64>(0)).unwrap() as usize;
    let max_bytes = dirty_len + identity_len + 64 + event_len + identity_len + 63;
    let page = idx.sync_snapshot_page(None, 0, 1_789_259_200_000, 30, 256, max_bytes).unwrap();
    assert!(!page.dirty_events.is_empty());
    assert!(page.events.is_empty(), "a normal event that does not fit after dirty rows belongs to the next page");
    assert!(!page.events_done);
}

#[test]
fn late_offline_growth_is_dirty_and_prune_waits_for_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("offline.jsonl");
    let old = 1_800_000_000_000_i64 - 31 * 86_400_000;
    std::fs::write(&path, format!(r#"{{"id":"offline-1","ts":{old},"session":"s","in":1}}
"#)).unwrap();
    let adapter = Mock::new("mocka", dir.path());
    let mut idx = Index::open_in_memory().unwrap();
    idx.conn().execute(
        "CREATE TABLE device_sync_secondary_config(config_key TEXT PRIMARY KEY, enabled INTEGER NOT NULL)",
        [],
    ).unwrap();
    idx.conn().execute(
        "INSERT INTO device_sync_secondary_config(config_key, enabled) VALUES('secondary', 1)",
        [],
    ).unwrap();
    idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(idx.sync_dirty_events().unwrap().len(), 1);
    assert_eq!(idx.prune(1_800_000_000_000_i64 - 30 * 86_400_000).unwrap(), 0);
    assert_eq!(idx.event_count().unwrap(), 1);

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(format!(r#"{{"id":"offline-1","ts":{old},"session":"s","in":2}}
"#).as_bytes()).unwrap();
    file.flush().unwrap();
    idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    let dirty = idx.sync_dirty_events().unwrap();
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].event.counts.input, 2.0);
    assert_eq!(idx.prune(1_800_000_000_000_i64 - 30 * 86_400_000).unwrap(), 0);

    idx.conn().execute("DELETE FROM sync_dirty", []).unwrap();
    assert_eq!(idx.prune(1_800_000_000_000_i64 - 30 * 86_400_000).unwrap(), 1);
    assert_eq!(idx.event_count().unwrap(), 0);
}

#[test]
fn shrinking_a_file_purges_its_stale_rows_before_rereading() {
    let dir = staged(&["shrinkable_full.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());
    let (mut idx, first) = ingest(&adapter, DateFilter::default());
    assert_eq!(first.new_events, 4);

    replace_file(dir.path(), "shrinkable_full.jsonl", "shrinkable_short.jsonl");
    let second = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(second.purged, 4, "the whole rewritten file is dropped");
    assert_eq!(second.new_events, 1);
    assert_eq!(idx.event_count().unwrap(), 1, "no double count after truncation");

    let kept = &idx.all_events().unwrap()[0];
    assert_eq!(kept.dedupe_key.as_deref(), Some("x-0"));
    assert_eq!(kept.counts.input, 100.0);
}

#[test]
fn rebuild_after_a_full_ingest_lands_on_the_same_totals() {
    let dir = staged(&["basic.jsonl", "second.jsonl", "quota.jsonl", "credits.jsonl", "parallel"]);
    let adapter = Mock::new("mocka", dir.path());
    let (mut idx, cold) = ingest(&adapter, DateFilter::default());
    assert_eq!(cold.total_events, 34, "5 + 2 + 2 + 1 + 8*3");

    let before = sorted(idx.all_events().unwrap());
    let per_tool = idx.per_tool_counts().unwrap();
    let rebuilt = idx.rebuild(&adapters_for(dir.path())).unwrap();
    assert_eq!(rebuilt.total_events, cold.total_events);
    assert_eq!(rebuilt.purged, 0, "rebuild clears the tables, it does not purge per file");
    assert_eq!(rebuilt.new_events, cold.total_events);
    assert_eq!(idx.per_tool_counts().unwrap(), per_tool);
    assert_eq!(sorted(idx.all_events().unwrap()), before);
}

#[test]
fn prune_removes_only_rows_older_than_the_cutoff() {
    let dir = staged(&["basic.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());
    let (mut idx, _) = ingest(&adapter, DateFilter::default());

    let cutoff = 1_789_100_000_000;
    let removed = idx.prune(cutoff).unwrap();
    assert_eq!(removed, 3);
    assert_eq!(idx.event_count().unwrap(), 2);
    let kept = idx.all_events().unwrap();
    assert_eq!(kept.iter().map(|e| e.ts_ms).collect::<Vec<_>>(), vec![1789172800000, 1789259200000]);
    assert!(kept.iter().all(|e| e.calls.is_empty()), "their call rows went with them");
    assert_eq!(idx.events_since(cutoff).unwrap().len(), 2);
    assert_eq!(idx.prune(cutoff).unwrap(), 0, "pruning twice is a no-op");
}

#[test]
fn parallel_ingest_matches_sequential_ingest() {
    let dir = staged(&["basic.jsonl", "second.jsonl", "parallel", "quota.jsonl", "credits.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());

    let mut seq = Index::open_in_memory().unwrap();
    seq.set_max_workers(1);
    assert_eq!(seq.max_workers(), 1);
    let mut par = Index::open_in_memory().unwrap();
    par.set_max_workers(8);

    let r_seq = seq.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    let r_par = par.ingest_adapter(&adapter, &DateFilter::default()).unwrap();

    assert!(r_seq.new_events > 20, "the fixture set must be big enough to fan out");
    assert_eq!(r_seq.files_scanned, r_par.files_scanned);
    assert_eq!(r_seq.files_changed, r_par.files_changed);
    assert_eq!(r_seq.new_events, r_par.new_events);
    assert_eq!(r_seq.deduped, r_par.deduped);
    assert_eq!(r_seq.per_tool, r_par.per_tool);
    assert_eq!(seq.event_count().unwrap(), par.event_count().unwrap());
    assert_eq!(sorted(seq.all_events().unwrap()), sorted(par.all_events().unwrap()));
    assert!(seq.errors().is_empty() && par.errors().is_empty());
}

#[test]
fn one_failing_file_never_aborts_the_pass() {
    let dir = staged(&["basic.jsonl", "second.jsonl"]);
    let mut idx = Index::open_in_memory().unwrap();
    let broken = Mock::new("mocka", dir.path()).failing("second.jsonl");

    let report = idx.ingest_adapter(&broken, &DateFilter::default()).unwrap();
    assert_eq!(report.new_events, 5, "the healthy file still lands");
    assert_eq!(idx.errors().len(), 1);
    assert!(idx.errors()[0].contains("second.jsonl"), "{:?}", idx.errors());

    // The failed file keeps no "done" marker, so the next pass retries it.
    let healthy = Mock::new("mocka", dir.path());
    let retry = idx.ingest_adapter(&healthy, &DateFilter::default()).unwrap();
    assert_eq!(retry.new_events, 2);
    assert_eq!(idx.event_count().unwrap(), 7);
    assert!(idx.errors().is_empty());
}

#[test]
fn source_statuses_merge_detection_with_index_counts() {
    let dir = staged(&["basic.jsonl"]);
    let dir_b = staged(&["second.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());
    let mut idx = ingest(&adapter, DateFilter::default()).0;
    idx.ingest_adapter(&Mock::new("mockb", dir_b.path()), &DateFilter::default()).unwrap();

    let detected = vec![
        DetectedSource {
            id: "mocka".into(),
            display: "Mock Source".into(),
            roots: vec![dir.path().to_path_buf()],
            hint: Some("fixture".into()),
        },
        DetectedSource {
            id: "absent".into(),
            display: "Absent Tool".into(),
            roots: vec![dir.path().join("nope")],
            hint: None,
        },
    ];
    let statuses = idx.source_statuses(&detected).unwrap();
    assert_eq!(statuses[0].id, "mocka");
    assert!(statuses[0].detected);
    assert_eq!(statuses[0].events_ingested, 5);
    assert_eq!(statuses[0].roots, vec![dir.path().to_path_buf()]);
    assert_eq!(statuses[1].events_ingested, 0, "detected but never ingested");
    assert_eq!(statuses[1].display, "Absent Tool");
    // An indexed tool nobody detected (source removed since) is still spend, so it
    // is reported rather than silently dropped from the list.
    assert_eq!(statuses.len(), 3);
    assert_eq!(statuses[2].id, "mockb");
    assert!(!statuses[2].detected);
    assert_eq!(statuses[2].events_ingested, 2);
}

#[test]
fn calls_credits_and_quota_survive_the_round_trip() {
    let dir = staged(&["basic.jsonl", "quota.jsonl", "credits.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());
    let (idx, _) = ingest(&adapter, DateFilter::default());
    let events = idx.all_events().unwrap();

    let first = events.iter().find(|e| e.dedupe_key.as_deref() == Some("evt-1")).unwrap();
    assert_eq!(first.calls.len(), 1);
    assert_eq!(first.calls[0].kind, CallKind::Mcp);
    assert_eq!(first.calls[0].name, "bugx");
    assert_eq!(first.project.as_deref(), Some("/work/alpha"));
    assert_eq!(first.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(first.meter, Meter::Tokens);
    assert_eq!(first.counts.cache_read, 5000.0);
    assert_eq!(first.counts.total(), 1000.0 + 200.0 + 5000.0 + 300.0);
    assert_eq!(first.source, dir.path().join("basic.jsonl").to_string_lossy());

    let skill = events.iter().find(|e| e.dedupe_key.as_deref() == Some("evt-3")).unwrap();
    assert_eq!(skill.calls[0].kind, CallKind::Skill);
    assert_eq!(skill.counts.reasoning, 256.0);

    let sampled = events.iter().filter(|e| e.quota.is_some()).count();
    assert_eq!(sampled, 2);
    assert_eq!(
        events.iter().find(|e| e.dedupe_key.as_deref() == Some("q-2")).unwrap().quota.as_ref().unwrap().used_percent,
        42.0
    );

    let credit = events.iter().find(|e| e.dedupe_key.as_deref() == Some("cr-1")).unwrap();
    assert_eq!(credit.meter, Meter::Credits);
    assert_eq!(credit.counts.credits, 0.27);
    assert_eq!(credit.counts.total(), 0.0, "a credits source reports no tokens");
    assert!(!credit.counts.is_zero(), "but it is still billable spend");
}

#[test]
fn a_narrow_filter_consumes_bytes_it_does_not_index() {
    let dir = staged(&["basic.jsonl"]);
    let adapter = Mock::new("mocka", dir.path());
    let (mut idx, report) =
        ingest(&adapter, DateFilter::new(Some(1789050000000), Some(1789200000000)));
    assert_eq!(report.new_events, 2, "only evt-3 and evt-4 are inside the window");

    // This is why callers ingest the retention window and filter at query time:
    // the skipped records' bytes are already behind the cursor.
    let again = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(again.new_events, 0);
    assert_eq!(idx.event_count().unwrap(), 2);
}

#[test]
fn an_on_disk_index_reopens_with_its_events() {
    let dir = staged(&["basic.jsonl", "second.jsonl"]);
    let db = dir.path().join("nested/dir/index.db");
    let adapter = Mock::new("mocka", dir.path());
    {
        let mut idx = Index::open(&db).unwrap();
        assert_eq!(idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap().new_events, 7);
        assert_eq!(idx.path(), Some(db.as_path()));
        assert_eq!(idx.files_tracked().unwrap(), 2);
    }
    let reopened = Index::open(&db).unwrap();
    assert_eq!(reopened.event_count().unwrap(), 7);
    assert!(reopened.meta_value("schema_version").unwrap().is_some());
    assert!(reopened.meta_value("last_ingest_ms").unwrap().is_some());
    assert!(reopened.meta_value("index_id").unwrap().is_some());
    // Nothing to redo: the manifest survived the reopen.
    let mut again = Index::open(&db).unwrap();
    assert_eq!(again.ingest_adapter(&adapter, &DateFilter::default()).unwrap().new_events, 0);
}

#[test]
fn retention_and_default_path_look_sane() {
    assert_eq!(RETENTION_DAYS, 750);
    if let Some(path) = Index::default_path() {
        assert!(path.ends_with("tokenme/index.db") || path.ends_with("tokenme\\index.db"), "{path:?}");
    }
    assert!(Index::open_in_memory().unwrap().path().is_none());
}

#[test]
fn cumulative_dedupe_replaces_with_greater_total_and_updates_rollup() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("live.jsonl");
    // Snapshot 1: 100 in, 20 out
    std::fs::write(&file, b"{\"id\":\"cum-1\",\"ts\":1789172800000,\"session\":\"s1\",\"in\":100,\"out\":20}\n").unwrap();

    let adapter = Mock { id: "mocka", dir: dir.path().to_path_buf(), fail_on: None, ignore_cursor: true };
    let (mut idx, r1) = ingest(&adapter, DateFilter::default());
    assert_eq!(r1.new_events, 1);
    assert_eq!(idx.event_count().unwrap(), 1);
    let ev1 = &idx.all_events().unwrap()[0];
    assert_eq!(ev1.counts.input, 100.0);
    assert_eq!(ev1.counts.output, 20.0);

    // Snapshot 2: session continues growing, now 250 in and 60 out
    // Every rewrite must move the file's (size, mtime_ms) fingerprint or the
    // pipeline skips it as unchanged: these snapshots swap equal-length digits
    // ("250"→"200"), so the size is identical and the mtime has to cross a
    // millisecond. Two writes can land inside one.
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(&file, b"{\"id\":\"cum-1\",\"ts\":1789172810000,\"session\":\"s1\",\"in\":250,\"out\":60}\n").unwrap();
    let _r2 = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(idx.event_count().unwrap(), 1, "still one event row");
    let ev2 = &idx.all_events().unwrap()[0];
    assert_eq!(ev2.counts.input, 250.0);
    assert_eq!(ev2.counts.output, 60.0);

    // Check event_rollup parity: updated to 250 in, 60 out, n = 1
    let (in_tok, out_tok, n): (f64, f64, i64) = idx
        .conn()
        .query_row(
            "SELECT sum(in_tok), sum(out_tok), sum(n) FROM event_rollup WHERE tool = 'mocka'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(in_tok, 250.0);
    assert_eq!(out_tok, 60.0);
    assert_eq!(n, 1);

    // Snapshot 3: stale/duplicate snapshot with lower tokens
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(&file, b"{\"id\":\"cum-1\",\"ts\":1789172810000,\"session\":\"s1\",\"in\":200,\"out\":50}\n").unwrap();
    let r3 = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(r3.deduped, 1, "stale lower total was ignored");
    let ev3 = &idx.all_events().unwrap()[0];
    assert_eq!(ev3.counts.input, 250.0);
    assert_eq!(ev3.counts.output, 60.0);
}

/// Cline's desktop build writes a sub-agent's transcript before its meta exists
/// at all, so the first read bills the call with no project and the second read
/// — after the meta landed — knows the workspace. The money must not move; the
/// label must.
#[test]
fn a_better_label_repairs_the_row_and_its_rollup_bucket_without_moving_the_money() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("label.jsonl");
    std::fs::write(&file, b"{\"id\":\"k1\",\"ts\":1789172800000,\"session\":\"s1\",\"in\":100,\"out\":20}\n").unwrap();
    let adapter = Mock { ignore_cursor: true, ..Mock::new("mocka", dir.path().to_path_buf()) };
    let (mut idx, first) = ingest(&adapter, DateFilter::default());
    assert_eq!(first.new_events, 1);
    assert_eq!(idx.all_events().unwrap()[0].project, None, "the first read could not name the workspace");

    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(
        &file,
        b"{\"id\":\"k1\",\"ts\":1789172800000,\"session\":\"s1\",\"project\":\"AutoRSA\",\"model\":\"deepseek\",\"in\":100,\"out\":20}\n",
    )
    .unwrap();
    let second = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(second.relabeled, 1, "same money, better label: the row is repaired, not swallowed");
    assert_eq!(second.deduped, 0);
    assert_eq!(second.new_events, 0, "one call stays one row");
    let ev = &idx.all_events().unwrap()[0];
    assert_eq!(ev.project.as_deref(), Some("AutoRSA"));
    assert_eq!(ev.counts.input, 100.0, "money never moves sideways either");

    let mut rows: Vec<(String, f64, i64)> = idx
        .conn()
        .prepare("SELECT project, in_tok, n FROM event_rollup WHERE tool = 'mocka'")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        rows,
        vec![("AutoRSA".to_string(), 100.0, 1)],
        "the rollup followed the label instead of leaving the old bucket holding the tokens"
    );

    let third = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(third.relabeled, 0, "a steady-state pass has nothing to repair");
}


/// Rows first-written before replace-on-grow (77f8d95) are frozen at their
/// first-write snapshot: the finished projection never changes again, so no
/// re-emit ever reaches the replace path. `setup`'s one-shot repair drops the
/// cumulative adapters' read cursors so the next pass re-reads every
/// projection from byte 0 and re-emits the final totals.
#[test]
fn the_cumulative_rescan_repair_drops_stale_cursors_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    // An index from the frozen-first-write era: cursors at EOF, marker absent.
    {
        let idx = Index::open(&db).unwrap();
        idx.conn()
            .execute("DELETE FROM meta WHERE key = 'repair:cumulative-rescan-1'", [])
            .unwrap();
        for tool in ["dsh", "hermes", "funide", "joycode", "claude", "atomcode", "codex"] {
            idx.conn()
                .execute(
                    &format!(
                        "INSERT INTO file_state(source_key, tool, size, mtime_ms, cursor, events, updated_ms) \
                         VALUES('{t}:{f}', '{t}', 10, 1, 10, 1, 1)",
                        t = tool,
                        f = tool,
                    ),
                    [],
                )
                .unwrap();
        }
    }
    let idx = Index::open(&db).unwrap();
    for tool in ["dsh", "hermes", "funide", "joycode", "claude", "atomcode"] {
        let left: i64 = idx
            .conn()
            .query_row(
                &format!("SELECT COUNT(*) FROM file_state WHERE tool = '{tool}'"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "{tool} cursor must be dropped for the rescan");
    }
    let codex: i64 = idx
        .conn()
        .query_row("SELECT COUNT(*) FROM file_state WHERE tool = 'codex'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(codex, 1, "cursor-idempotent adapters keep their cursors");
    assert_eq!(idx.meta_value("repair:cumulative-rescan-1").unwrap().as_deref(), Some("1"));

    // One-shot: cursors earned after the repair survive every reopen.
    drop(idx);
    let idx = Index::open(&db).unwrap();
    idx.conn()
        .execute(
            "INSERT INTO file_state(source_key, tool, size, mtime_ms, cursor, events, updated_ms) \
             VALUES('dsh:back', 'dsh', 10, 1, 10, 1, 1)",
            [],
        )
        .unwrap();
    drop(idx);
    let idx = Index::open(&db).unwrap();
    let back: i64 = idx
        .conn()
        .query_row("SELECT COUNT(*) FROM file_state WHERE tool = 'dsh'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(back, 1, "the repair never runs twice");
}

/// The repair's end-to-end shape: cursor dropped, projection unchanged since
/// its final write, re-emit lifts the frozen row through replace-on-grow and
/// the rollup lands on the same totals without double-counting.
#[test]
fn a_dropped_cursor_reemits_and_replace_on_grow_lands_the_final_total() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("live.jsonl");
    // Mid-flight snapshot, the shape a pre-77f8d95 index froze in place.
    std::fs::write(
        &file,
        b"{\"id\":\"cum-2\",\"ts\":1789172800000,\"session\":\"s1\",\"in\":100,\"out\":20}\n",
    )
    .unwrap();
    let adapter = Mock::new("mockb", dir.path());
    let (mut idx, first) = ingest(&adapter, DateFilter::default());
    assert_eq!(first.new_events, 1);

    // The session finished; the projection now holds the final totals. The
    // repair drops the cursor, so the unchanged file is re-read from byte 0.
    std::fs::write(
        &file,
        b"{\"id\":\"cum-2\",\"ts\":1789172810000,\"session\":\"s1\",\"in\":250,\"out\":60}\n",
    )
    .unwrap();
    idx.conn().execute("DELETE FROM file_state WHERE tool = 'mockb'", []).unwrap();
    let second = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(second.files_scanned, 1, "a cursor-less file is re-read whole");
    assert_eq!(idx.event_count().unwrap(), 1, "still one event row");
    let ev = &idx.all_events().unwrap()[0];
    assert_eq!(ev.counts.input, 250.0);
    assert_eq!(ev.counts.output, 60.0);

    let (in_tok, out_tok, n): (f64, f64, i64) = idx
        .conn()
        .query_row(
            "SELECT sum(in_tok), sum(out_tok), sum(n) FROM event_rollup WHERE tool = 'mockb'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(in_tok, 250.0, "the rollup holds the final total, not first + final");
    assert_eq!(out_tok, 60.0);
    assert_eq!(n, 1);

    // The cursor is re-earned: an unchanged projection costs nothing again.
    let third = idx.ingest_adapter(&adapter, &DateFilter::default()).unwrap();
    assert_eq!(third.new_events, 0);
    assert_eq!(idx.event_count().unwrap(), 1);
}

/// antigravity's buckets were redrawn by 61c4f96, so its pre-redraw local rows
/// hold inflated totals no re-emit can lift: grow-on-write refuses the smaller
/// same-key total, and a redrawn key strands the old rows beside the new ones.
/// The re-seed drops exactly the local rows — merged rows are another
/// machine's truth — with their calls, quotas and rollup, and lets the next
/// wholesale read re-write them under the new semantics.
#[test]
fn the_antigravity_reseed_drops_local_rows_and_keeps_merged_ones_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    {
        let idx = Index::open(&db).unwrap();
        // Undo the repair a fresh open already ran, to stage the pre-shape.
        idx.conn()
            .execute("DELETE FROM meta WHERE key = 'repair:antigravity-reseed-1'", [])
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO event(id, tool, ts_ms, session, meter, cr_tok, dedupe_key, source) \
                 VALUES(1, 'antigravity', 10, 's', 'tokens', 100.0, 'agy-a', '/u/x/agy.db')",
                [],
            )
            .unwrap();
        idx.conn().execute("INSERT INTO call(event_id, kind, name) VALUES(1, 'tool', 't1')", []).unwrap();
        idx.conn()
            .execute(
                "INSERT INTO quota(event_id, used_percent, window_minutes, resets_at_ms, label) \
                 VALUES(1, 5.0, 300, 20, 'w')",
                [],
            )
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, \
                     in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts) \
                 VALUES('2026-10-01', '', 'antigravity', 's', '', '', 'tokens', \
                     0, 0, 100, 0, 0, 0, 1, 1, 10, 10)",
                [],
            )
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO event(id, tool, ts_ms, session, meter, cr_tok, dedupe_key, source) \
                 VALUES(2, 'antigravity', 11, 's', 'tokens', 250.0, 'agy-b', 'linux:o1:/h/agy.db')",
                [],
            )
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, \
                     in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts) \
                 VALUES('2026-10-01', 'o1', 'antigravity', 's', '', '', 'tokens', \
                     0, 0, 250, 0, 0, 0, 1, 1, 11, 11)",
                [],
            )
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO file_state(source_key, tool, size, mtime_ms, cursor, events, updated_ms) \
                 VALUES('agy', 'antigravity', 10, 1, 10, 1, 1)",
                [],
            )
            .unwrap();
    }
    let idx = Index::open(&db).unwrap();
    let count = |sql: &str| -> i64 { idx.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT COUNT(*) FROM event WHERE tool='antigravity' AND source NOT LIKE 'linux:%'"),
        0,
        "local rows go for the re-seed"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM event WHERE source LIKE 'linux:%'"),
        1,
        "merged rows are another machine's truth and stay"
    );
    assert_eq!(count("SELECT COUNT(*) FROM call"), 0, "the local row's call goes with it");
    assert_eq!(count("SELECT COUNT(*) FROM quota"), 0, "the local row's quota goes with it");
    assert_eq!(
        count("SELECT COUNT(*) FROM event_rollup WHERE tool='antigravity' AND origin=''"),
        0,
        "the local rollup group goes"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM event_rollup WHERE tool='antigravity' AND origin='o1'"),
        1,
        "the merged rollup group stays"
    );
    assert_eq!(count("SELECT COUNT(*) FROM file_state WHERE tool='antigravity'"), 0);
    assert_eq!(idx.meta_value("repair:antigravity-reseed-1").unwrap().as_deref(), Some("1"));

    // One-shot: rows written after the re-seed survive every reopen.
    drop(idx);
    let idx = Index::open(&db).unwrap();
    idx.conn()
        .execute(
            "INSERT INTO event(id, tool, ts_ms, session, meter, cr_tok, dedupe_key, source) \
             VALUES(3, 'antigravity', 12, 's2', 'tokens', 7.0, 'agy-c', '/u/x/agy.db')",
            [],
        )
        .unwrap();
    drop(idx);
    let idx = Index::open(&db).unwrap();
    let back: i64 = idx
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM event WHERE tool='antigravity' AND source NOT LIKE 'linux:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(back, 1, "the re-seed never runs twice");
}

/// The Trae split re-homes rows that were read from a CN edition's store back
/// when they landed under `trae`. The trap the live index caught: a build that
/// already re-read the CN store holds `trae_cn#<turn>` beside the old
/// `trae#<turn>`, so a plain re-key of the old spelling dies on the unique
/// dedupe index — the twins delete first, the rest re-home in place, and
/// Windows source keys (backslashes) match too.
#[test]
fn the_trae_split_rehomes_cn_rows_without_colliding_on_twins() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    {
        let idx = Index::open(&db).unwrap();
        idx.conn()
            .execute("DELETE FROM meta WHERE key = 'repair:trae-split-1'", [])
            .unwrap();
        let ins = |id: i64, key: &str, source: &str| {
            idx.conn()
                .execute(
                    "INSERT INTO event(id, tool, ts_ms, session, meter, cr_tok, dedupe_key, source) \
                     VALUES(?1, 'trae', 10, 's', 'tokens', 100.0, ?2, ?3)",
                    rusqlite::params![id, key, source],
                )
                .unwrap();
        };
        // Mis-filed CN row whose trae_cn twin already exists — the collision.
        ins(1, "trae#tw", "/Users/x/Library/Application Support/Trae CN/ModularData/ai-agent/database.db");
        idx.conn()
            .execute(
                "INSERT INTO event(id, tool, ts_ms, session, meter, cr_tok, dedupe_key, source) \
                 VALUES(2, 'trae_cn', 10, 's', 'tokens', 100.0, 'trae_cn#tw', \
                     '/Users/x/Library/Application Support/Trae CN/ModularData/ai-agent/database.db')",
                [],
            )
            .unwrap();
        // The international store's own rows stay exactly as they are.
        ins(3, "trae#intl", "/Users/x/Library/Application Support/Trae/ModularData/ai-agent/database.db");
        // A Windows CN row re-homes too: its source key carries backslashes.
        ins(4, "trae#win", "C:\\Users\\x\\AppData\\Roaming\\Trae CN\\ModularData\\ai-agent\\database.db");
        idx.conn()
            .execute(
                "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, \
                     in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts) \
                 VALUES('2026-10-08', '', 'trae', 's', '', '', 'tokens', \
                     0, 0, 100, 0, 0, 0, 1, 1, 10, 10)",
                [],
            )
            .unwrap();
    }
    let idx = Index::open(&db).unwrap();
    let count = |sql: &str| -> i64 { idx.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT COUNT(*) FROM event WHERE dedupe_key = 'trae#tw'"),
        0,
        "the old-key twin is a pure duplicate and goes"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM event WHERE dedupe_key = 'trae_cn#tw'"),
        1,
        "the split tool's own row survives"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM event WHERE dedupe_key = 'trae#intl' AND tool = 'trae'"),
        1,
        "international rows are untouched"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM event WHERE dedupe_key = 'trae_cn#win' AND tool = 'trae_cn'"),
        1,
        "a Windows-path CN row re-homes (instr, not a slash-pinned LIKE)"
    );
    assert_eq!(count("SELECT COUNT(*) FROM event_rollup"), 0, "the rollup rebuilds lazily");
    assert_eq!(idx.meta_value("repair:trae-split-1").unwrap().as_deref(), Some("1"));

    // One-shot: rows written after the split survive every reopen.
    drop(idx);
    let idx = Index::open(&db).unwrap();
    idx.conn()
        .execute(
            "INSERT INTO event(id, tool, ts_ms, session, meter, cr_tok, dedupe_key, source) \
             VALUES(9, 'trae', 11, 's2', 'tokens', 7.0, 'trae#later', '/u/Trae/database.db')",
            [],
        )
        .unwrap();
    drop(idx);
    let idx = Index::open(&db).unwrap();
    let back: i64 = idx
        .conn()
        .query_row("SELECT COUNT(*) FROM event WHERE dedupe_key = 'trae#later'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(back, 1, "the split never runs twice");
}

/// The trae adapter keyed each event's session by turn id, so the panel's
/// 会话 count read one chat conversation as N one-turn sessions. The repair
/// drops the local trae rows wholesale — the turn→session mapping lives in
/// the encrypted store, and a re-emit under the same dedupe key would be
/// IGNOREd beside the old rows — with their file_state (an unchanged store
/// must still re-read, or Trae idle for days leaves the tools empty), while
/// merged rows are another machine's truth and stay.
#[test]
fn the_trae_turn_session_repair_drops_local_rows_and_keeps_merged_ones_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    {
        let idx = Index::open(&db).unwrap();
        // Undo the repair a fresh open already ran, to stage the pre-shape.
        idx.conn()
            .execute("DELETE FROM meta WHERE key = 'repair:trae-turn-session-1'", [])
            .unwrap();
        for (id, tool, key, source) in [
            (1, "trae", "trae#t1", "/Users/x/Library/Application Support/Trae/ModularData/ai-agent/database.db"),
            (2, "trae_cn", "trae_cn#t2", "/Users/x/Library/Application Support/Trae CN/ModularData/ai-agent/database.db"),
            (3, "trae_cn", "trae_cn#t3", "linux:o1:/home/x/.trae/database.db"),
        ] {
            idx.conn()
                .execute(
                    "INSERT INTO event(id, tool, ts_ms, session, meter, cr_tok, dedupe_key, source) \
                     VALUES(?1, ?2, 10, 's', 'tokens', 100.0, ?3, ?4)",
                    rusqlite::params![id, tool, key, source],
                )
                .unwrap();
        }
        idx.conn().execute("INSERT INTO call(event_id, kind, name) VALUES(1, 'tool', 't1')", []).unwrap();
        idx.conn()
            .execute(
                "INSERT INTO quota(event_id, used_percent, window_minutes, resets_at_ms, label) \
                 VALUES(2, 5.0, 300, 20, 'w')",
                [],
            )
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, \
                     in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts) \
                 VALUES('2026-10-11', '', 'trae', 't1', '', '', 'tokens', \
                     0, 0, 100, 0, 0, 0, 1, 1, 10, 10)",
                [],
            )
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, \
                     in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts) \
                 VALUES('2026-10-11', 'o1', 'trae_cn', 't3', '', '', 'tokens', \
                     0, 0, 100, 0, 0, 0, 1, 1, 10, 10)",
                [],
            )
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO file_state(source_key, tool, size, mtime_ms, cursor, events, updated_ms) \
                 VALUES('trae:db', 'trae', 10, 1, 10, 1, 1)",
                [],
            )
            .unwrap();
        idx.conn()
            .execute(
                "INSERT INTO file_state(source_key, tool, size, mtime_ms, cursor, events, updated_ms) \
                 VALUES('trae_cn:db', 'trae_cn', 10, 1, 10, 1, 1)",
                [],
            )
            .unwrap();
    }
    let idx = Index::open(&db).unwrap();
    let count = |sql: &str| -> i64 { idx.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT COUNT(*) FROM event WHERE tool IN ('trae','trae_cn') AND source NOT LIKE 'linux:%'"),
        0,
        "local rows go for the re-read"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM event WHERE source LIKE 'linux:%'"),
        1,
        "merged rows are another machine's truth and stay"
    );
    assert_eq!(count("SELECT COUNT(*) FROM call"), 0, "the local row's call goes with it");
    assert_eq!(count("SELECT COUNT(*) FROM quota"), 0, "the local row's quota goes with it");
    assert_eq!(
        count("SELECT COUNT(*) FROM event_rollup WHERE tool IN ('trae','trae_cn') AND origin=''"),
        0,
        "the local rollup groups go"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM event_rollup WHERE tool='trae_cn' AND origin='o1'"),
        1,
        "the merged rollup group stays"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM file_state WHERE tool IN ('trae','trae_cn')"),
        0,
        "an unchanged store must still re-read"
    );
    assert_eq!(idx.meta_value("repair:trae-turn-session-1").unwrap().as_deref(), Some("1"));

    // One-shot: rows written after the repair survive every reopen.
    drop(idx);
    let idx = Index::open(&db).unwrap();
    idx.conn()
        .execute(
            "INSERT INTO event(id, tool, ts_ms, session, meter, cr_tok, dedupe_key, source) \
             VALUES(9, 'trae_cn', 11, 's2', 'tokens', 7.0, 'trae_cn#later', '/u/Trae CN/database.db')",
            [],
        )
        .unwrap();
    drop(idx);
    let idx = Index::open(&db).unwrap();
    let back: i64 = idx
        .conn()
        .query_row("SELECT COUNT(*) FROM event WHERE dedupe_key = 'trae_cn#later'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(back, 1, "the repair never runs twice");
}

#[test]
fn the_workbuddy_split_preserves_other_editions_and_replays_domestic_cursors() {
    let dir = tempfile::tempdir().unwrap(); let db = dir.path().join("index.db");
    {
        let idx = Index::open(&db).unwrap();
        idx.conn().execute("DELETE FROM meta WHERE key = 'repair:workbuddy-split-1'", []).unwrap();
        for (id, tool, key, source) in [
            (1,"workbuddy","workbuddy#unix","/home/demo/.workbuddy/projects/session.jsonl"),
            (2,"workbuddy","workbuddy#win",r"C:\Users\demo\.workbuddy\projects\session.jsonl"),
            (3,"workbuddy","workbuddy#global","/home/demo/.workbuddy-ai/projects/session.jsonl"),
            (4,"workbuddy","workbuddy#remote","linux:server:/home/demo/.workbuddy/projects/session.jsonl"),
            (5,"workbuddy","workbuddy#twin","/home/demo/.workbuddy/projects/twin.jsonl"),
            (6,"workbuddy_cn","workbuddy_cn#twin","/home/demo/.workbuddy/projects/twin.jsonl"),
        ] {
            idx.conn().execute("INSERT INTO event(id,tool,ts_ms,session,meter,in_tok,dedupe_key,source) VALUES(?1,?2,10,'s','tokens',100,?3,?4)", rusqlite::params![id,tool,key,source]).unwrap();
            idx.conn().execute("INSERT OR REPLACE INTO file_state(source_key,tool,cursor) VALUES(?1,?2,999)", rusqlite::params![source,tool]).unwrap();
        }
        idx.conn().execute("INSERT INTO call(event_id,kind,name) VALUES(5,'tool','test')", []).unwrap();
        idx.conn().execute("INSERT INTO quota(event_id,used_percent,window_minutes,resets_at_ms) VALUES(5,10,60,100)", []).unwrap();
        idx.conn().execute("INSERT INTO event_rollup(day,origin,tool,session,project,model,meter,in_tok,cc_tok,cr_tok,out_tok,reason_tok,credits,n,nonzero,min_ts,max_ts) VALUES('2026-10-10','','claude','s','','','tokens',1,0,0,0,0,0,1,1,10,10)", []).unwrap();
        idx.conn().execute("INSERT INTO meta(key,value) VALUES('sync:linux:server','keep')", []).unwrap();
    }
    let idx = Index::open(&db).unwrap();
    let count = |sql: &str| -> i64 { idx.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT count(*) FROM event WHERE tool='workbuddy_cn'"),3);
    assert_eq!(count("SELECT count(*) FROM event WHERE tool='workbuddy'"),2);
    assert_eq!(count("SELECT count(*) FROM event WHERE dedupe_key IN ('workbuddy_cn#unix','workbuddy_cn#win','workbuddy_cn#twin')"),3);
    assert_eq!(count("SELECT count(*) FROM event WHERE id=5"),0);
    assert_eq!(count("SELECT count(*) FROM call WHERE event_id=5"),0);
    assert_eq!(count("SELECT count(*) FROM quota WHERE event_id=5"),0);
    assert_eq!(count("SELECT count(*) FROM file_state WHERE tool='workbuddy' AND instr(replace(source_key,char(92),'/'),'/.workbuddy/')>0 AND source_key NOT LIKE 'linux:%'"),0);
    assert_eq!(count("SELECT count(*) FROM file_state WHERE source_key LIKE '%/.workbuddy-ai/%'"),1);
    assert_eq!(count("SELECT count(*) FROM file_state WHERE source_key LIKE 'linux:%'"),1,"remote cursors survive");
    assert_eq!(count("SELECT count(*) FROM event_rollup"),0,"rollup must rebuild all groups together");
    assert_eq!(idx.meta_value("sync:linux:server").unwrap().as_deref(),Some("keep"));
    assert_eq!(idx.meta_value("repair:workbuddy-split-1").unwrap().as_deref(),Some("1"));
    drop(idx);
    let idx = Index::open(&db).unwrap();
    assert_eq!(idx.conn().query_row("SELECT count(*) FROM event", [], |r| r.get::<_,i64>(0)).unwrap(),5);
}

#[test]
fn the_qoder_split_preserves_other_editions_and_replays_domestic_cursors() {
    let dir = tempfile::tempdir().unwrap(); let db = dir.path().join("index.db");
    {
        let idx = Index::open(&db).unwrap();
        idx.conn().execute("DELETE FROM meta WHERE key = 'repair:qoder-split-1'", []).unwrap();
        for (id, tool, key, source) in [
            (1,"qoder","qoder#unix","/home/demo/.qoder-cn/projects/session.jsonl"),
            (2,"qoder","qoder#win",r"C:\Users\demo\QoderCN\SharedClientCache\session.jsonl"),
            (3,"qoder","qoder#global","/home/demo/.qoder/projects/session.jsonl"),
            (4,"qoder","qoder#remote","linux:server:/home/demo/.qoder-cn/projects/session.jsonl"),
            (5,"qoder","qoder#twin","/home/demo/.qoder-cn/projects/twin.jsonl"),
            (6,"qoder_cn","qoder_cn#twin","/home/demo/.qoder-cn/projects/twin.jsonl"),
            (7,"qoder","session#message","/home/demo/.qoder-cn/projects/fallback.jsonl"),
        ] {
            idx.conn().execute("INSERT INTO event(id,tool,ts_ms,session,meter,in_tok,dedupe_key,source) VALUES(?1,?2,10,'s','tokens',100,?3,?4)", rusqlite::params![id,tool,key,source]).unwrap();
            idx.conn().execute("INSERT OR REPLACE INTO file_state(source_key,tool,cursor) VALUES(?1,?2,999)", rusqlite::params![source,tool]).unwrap();
        }
        idx.conn().execute("INSERT INTO call(event_id,kind,name) VALUES(5,'tool','test')", []).unwrap();
        idx.conn().execute("INSERT INTO quota(event_id,used_percent,window_minutes,resets_at_ms) VALUES(5,10,60,100)", []).unwrap();
        idx.conn().execute("INSERT INTO event_rollup(day,origin,tool,session,project,model,meter,in_tok,cc_tok,cr_tok,out_tok,reason_tok,credits,n,nonzero,min_ts,max_ts) VALUES('2026-10-10','','claude','s','','','tokens',1,0,0,0,0,0,1,1,10,10)", []).unwrap();
        idx.conn().execute("INSERT INTO meta(key,value) VALUES('sync:linux:server','keep')", []).unwrap();
    }
    let idx = Index::open(&db).unwrap();
    let count = |sql: &str| -> i64 { idx.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT count(*) FROM event WHERE tool='qoder_cn'"),4);
    assert_eq!(count("SELECT count(*) FROM event WHERE dedupe_key='qoder_cn#session#message'"),1);
    assert_eq!(count("SELECT count(*) FROM event WHERE tool='qoder'"),2);
    assert_eq!(count("SELECT count(*) FROM event WHERE dedupe_key IN ('qoder_cn#unix','qoder_cn#win','qoder_cn#twin')"),3);
    assert_eq!(count("SELECT count(*) FROM event WHERE id=5"),0);
    assert_eq!(count("SELECT count(*) FROM call WHERE event_id=5"),0);
    assert_eq!(count("SELECT count(*) FROM quota WHERE event_id=5"),0);
    assert_eq!(count("SELECT count(*) FROM file_state WHERE tool='qoder' AND instr(replace(source_key,char(92),'/'),'/.qoder-cn/')>0 AND source_key NOT LIKE 'linux:%'"),0);
    assert_eq!(count("SELECT count(*) FROM file_state WHERE source_key LIKE '%/.qoder/%'"),1);
    assert_eq!(count("SELECT count(*) FROM file_state WHERE source_key LIKE 'linux:%'"),1,"remote cursors survive");
    assert_eq!(count("SELECT count(*) FROM event_rollup"),0,"rollup must rebuild all groups together");
    assert_eq!(idx.meta_value("sync:linux:server").unwrap().as_deref(),Some("keep"));
    assert_eq!(idx.meta_value("repair:qoder-split-1").unwrap().as_deref(),Some("1"));
    drop(idx);
    let idx = Index::open(&db).unwrap();
    assert_eq!(idx.conn().query_row("SELECT count(*) FROM event", [], |r| r.get::<_,i64>(0)).unwrap(),6);
}
