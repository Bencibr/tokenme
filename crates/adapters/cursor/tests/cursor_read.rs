use usage_adapter_cursor::CursorAdapter;
use usage_core::{FileKind, ReadCursor, SourceAdapter, SourceFile, TokenCounts};

#[test]
fn ledger_lines_resume_at_the_byte_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cursor-usage.jsonl");
    let line = r#"{"ts":1700000002000,"model":"composer-2.5","input":10,"output":4,"cache_read":8,"cache_write":2,"dedupe":"cursor#1700000002000#composer-2.5#10#4#8#2"}"#;
    std::fs::write(&path, format!("{line}\n")).unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let file = SourceFile { path, kind: FileKind::Jsonl, size: meta.len(), mtime_ms: 1 };
    let adapter = CursorAdapter;
    let first = adapter.read(&file, ReadCursor(0)).unwrap();
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.events[0].tool, "cursor");
    assert_eq!(first.events[0].model.as_deref(), Some("composer-2.5"));
    assert_eq!(
        first.events[0].counts,
        TokenCounts { input: 10.0, output: 4.0, cache_read: 8.0, cache_creation: 2.0, ..TokenCounts::default() }
    );
    assert_eq!(first.events[0].session, "cursor-2023-11-14");
    let again = adapter.read(&file, first.cursor).unwrap();
    assert!(again.events.is_empty(), "the byte cursor does not replay the line");
}
