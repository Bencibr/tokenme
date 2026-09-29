mod common;

/// The heatmap in `usage-core::report` spans 371 days; an index that forgets
/// more than that would show holes at the left edge, forever.
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
    assert_eq!(RETENTION_DAYS, 400);
    if let Some(path) = Index::default_path() {
        assert!(path.ends_with("tokenme/index.db") || path.ends_with("tokenme\\index.db"), "{path:?}");
    }
    assert!(Index::open_in_memory().unwrap().path().is_none());
}
