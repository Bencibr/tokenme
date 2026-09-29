//! One real-data pass over this machine's Antigravity CLI conversations, with
//! the numbers a reviewer needs to audit the field mapping.
//!
//! Run with: `cargo test -p usage-adapter-antigravity -- --ignored --nocapture`

use std::collections::BTreeMap;

use usage_adapter_antigravity::{AntigravityAdapter, TOOL_ID};
use usage_core::{DateFilter, ReadCursor, SourceAdapter};

#[test]
#[ignore = "reads ~/.gemini/antigravity-cli, which only exists on a machine with the CLI installed"]
fn real_antigravity_cli_conversations() {
    // The suite's other tests point this variable at temp installs; this one has
    // to see the real home directory.
    std::env::remove_var("ANTIGRAVITY_DATA_DIR");

    let a = AntigravityAdapter;
    let det = a.probe().expect("Antigravity CLI detected on this machine");
    eprintln!("probe: {} roots={:?} hint={:?}", det.display, det.roots, det.hint);
    assert_eq!(det.id, TOOL_ID);

    let files = a.discover(&DateFilter::default());
    let bytes: u64 = files.iter().map(|f| f.size).sum();
    eprintln!("discover: {} conversation databases, {bytes} bytes", files.len());
    assert!(!files.is_empty(), "this machine has conversations to read");

    // `audit` walks the same `read` path the indexer uses, row by row.
    let audit = a.audit(&files);
    eprintln!(
        "read: dbs={} unusable={} rows={} decoded={} merged={} events-total={:.0}",
        audit.dbs, audit.unusable, audit.rows, audit.decoded, audit.merged, audit.event_grand_total()
    );
    eprintln!(
        "raw wire stages over decoded rows: #4.1 prefix={} #4.2 input={} #4.3 total_output={} #4.5 cache_read={} #4.9={} #4.10={}",
        audit.raw.prefix, audit.raw.input, audit.raw.total_output, audit.raw.cache_read, audit.raw.out_a, audit.raw.out_b
    );
    eprintln!(
        "events: input={:.0} cache_creation={:.0} cache_read={:.0} output={:.0} (reasoning={:.0}) credits={:.0}",
        audit.counts.input,
        audit.counts.cache_creation,
        audit.counts.cache_read,
        audit.counts.output,
        audit.counts.reasoning,
        audit.counts.credits
    );
    eprintln!("models: {:?}", audit.models);

    // Undecodable rows, grouped by reason, with the byte offset each one failed
    // at — the audit trail for the field mapping if a CLI update moves it.
    let mut by_reason: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for row in &audit.skipped {
        let source = row.source.chars().take(8).collect::<String>();
        by_reason.entry(row.reason).or_default().push(match row.offset {
            Some(offset) => format!("{source}#{}@{offset}", row.idx),
            None => format!("{source}#{}", row.idx),
        });
    }
    eprintln!("undecoded: {} rows", audit.skipped.len());
    for (reason, rows) in &by_reason {
        let shown = if rows.len() > 12 { format!("{} …", rows[..12].join(", ")) } else { rows.join(", ") };
        eprintln!("  {reason}: {} [{}]", rows.len(), shown);
    }

    // The totals identity: what the blobs add up to, and what we emitted. With no
    // streamed partials merged the two must agree exactly; every merged row is
    // accounted for separately, and every dropped row contributed nothing to
    // either side because it never produced an event.
    let raw = audit.raw_grand_total();
    let events = audit.event_grand_total();
    eprintln!("Σ(input+cache_read+output): raw wire {raw:.0} vs emitted events {events:.0}");
    let drift = (raw - events).abs();
    eprintln!("drift between the two sides: {drift:.0} over {} merged rows", audit.merged);
    if audit.merged == 0 {
        assert_eq!(raw, events, "nothing merged, so both sides must be identical");
    } else {
        // A merge replaces two snapshots of one call with their per-field max, so
        // the emitted total can only shrink relative to the raw sum, never grow.
        assert!(events <= raw, "emitted {events} exceeded the raw sum {raw}");
    }
    assert!(audit.counts.output > 0.0, "the CLI recorded output tokens, got {:?}", audit.counts);
    assert_eq!(audit.split_mismatches, 0, "#4.3 == #4.9 + #4.10 on every row of every database");
    assert_eq!(audit.counts.cache_creation, 0.0, "no source writes a cache-write stage");
    assert_eq!(audit.counts.credits, 0.0, "the blobs' own cost numbers never become money");
    assert_eq!(audit.dbs, files.len() - audit.unusable, "every database was opened once");
    assert!(!audit.models.is_empty());
    for path in &files {
        let outcome = a.read(path, ReadCursor(0)).expect("read is infallible");
        assert_eq!(outcome.cursor, ReadCursor(0), "a Tree file never advances a cursor");
    }
}
