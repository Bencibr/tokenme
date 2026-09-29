//! One real-data smoke pass over this machine's `~/.codex/sessions`.
//! Run with: cargo test -p usage-adapter-codex -- --ignored --nocapture

use std::collections::HashSet;
use std::time::Instant;

use usage_adapter_codex::CodexAdapter;
use usage_core::{DateFilter, ReadCursor, SourceAdapter};

#[test]
#[ignore = "reads ~/.codex, which only exists on a machine with Codex installed"]
fn real_codex_rollouts() {
    let a = CodexAdapter;
    let det = a.probe().expect("Codex detected on this machine");
    eprintln!("probe: {} roots={:?} hint={:?}", det.display, det.roots, det.hint);

    let started = Instant::now();
    let files = a.discover(&DateFilter::default());
    let bytes: u64 = files.iter().map(|f| f.size).sum();
    eprintln!("discover: {} rollout files, {:.1} GiB in {:?}", files.len(), bytes as f64 / (1 << 30) as f64, started.elapsed());

    let mut events = 0usize;
    let mut tokens = 0.0f64;
    let mut models: HashSet<String> = HashSet::new();
    let mut no_model = 0usize;
    let mut quota = 0usize;
    let mut consumed = 0u64;
    let started = Instant::now();
    for f in &files {
        let out = a.read(f, ReadCursor(0)).expect("read is infallible");
        consumed += out.cursor.0;
        events += out.events.len();
        for e in &out.events {
            tokens += e.counts.total();
            match &e.model {
                Some(m) => {
                    models.insert(m.clone());
                }
                None => no_model += 1,
            }
            if e.quota.is_some() {
                quota += 1;
            }
        }
    }
    eprintln!(
        "read all: {events} events, {tokens:.0} tokens, {} distinct models, {no_model} without a model, {quota} quota samples, {:.1} GiB in {:?}",
        models.len(),
        consumed as f64 / (1 << 30) as f64,
        started.elapsed()
    );
    let mut models: Vec<_> = models.into_iter().collect();
    models.sort();
    eprintln!("models: {models:?}");
    assert!(events > 1000, "expected a real backlog, got {events}");
    assert!(tokens > 1.0e8, "expected hundreds of millions of tokens, got {tokens}");
}
