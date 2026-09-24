//! Test-only JSONL adapter. The real adapters are owned by other workstreams
//! and may still be `unimplemented!()`, so the index is tested against mocks
//! that follow the same `SourceAdapter` contract.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;
use usage_core::{
    Call, CallKind, DateFilter, DetectedSource, Error, FileKind, Meter, ReadCursor, ReadOutcome,
    Semantics, SourceAdapter, SourceFile, TokenCounts, UsageEvent,
};

pub struct Mock {
    pub id: &'static str,
    pub dir: PathBuf,
    /// `read` fails for any file whose path contains this substring.
    pub fail_on: Option<&'static str>,
    /// Behave like a `Tree` source: ignore the cursor, re-read the whole file.
    pub ignore_cursor: bool,
}

impl Mock {
    pub fn new(id: &'static str, dir: impl Into<PathBuf>) -> Self {
        Self { id, dir: dir.into(), fail_on: None, ignore_cursor: false }
    }

    pub fn failing(mut self, needle: &'static str) -> Self {
        self.fail_on = Some(needle);
        self
    }
}

impl SourceAdapter for Mock {
    fn id(&self) -> &'static str {
        self.id
    }

    fn display_name(&self) -> &'static str {
        "Mock Source"
    }

    fn semantics(&self) -> Semantics {
        Semantics::TOKENS_PER_CALL_INLINE
    }

    fn probe(&self) -> Option<DetectedSource> {
        if self.dir.is_dir() {
            Some(DetectedSource {
                id: self.id.to_string(),
                display: "Mock Source".into(),
                roots: vec![self.dir.clone()],
                hint: Some("fixture".into()),
            })
        } else {
            None
        }
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        let mut out = Vec::new();
        collect(&self.dir, &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        if let Some(needle) = self.fail_on {
            if file.key().contains(needle) {
                return Err(Error::adapter(self.id, format!("injected failure for {}", file.key())));
            }
        }
        let bytes = std::fs::read(&file.path).map_err(|e| Error::io(&file.path, e))?;
        let start = if self.ignore_cursor { 0 } else { (cursor.0 as usize).min(bytes.len()) };
        let mut events = Vec::new();
        for line in bytes[start..].split(|b| *b == b'\n') {
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let text = String::from_utf8_lossy(line);
            if let Some(ev) = parse_line(self.id, &file.path, &text) {
                events.push(ev);
            }
        }
        // A `Tree` source answers with the cursor it was handed: the indexer
        // stops when the cursor fails to advance, which is what keeps a
        // whole-file re-read from being counted twice in one pass.
        let end = if self.ignore_cursor { cursor.0 } else { bytes.len() as u64 };
        Ok(ReadOutcome { events, cursor: ReadCursor(end) })
    }
}

fn collect(dir: &Path, out: &mut Vec<SourceFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            out.push(SourceFile {
                size: meta.len(),
                mtime_ms: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                kind: FileKind::Jsonl,
                path,
            });
        }
    }
}

fn parse_line(tool: &str, source: &Path, text: &str) -> Option<UsageEvent> {
    let v: Value = serde_json::from_str(text).ok()?;
    let num = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let mut ev = UsageEvent::new(tool, v.get("ts")?.as_i64()?, v.get("session")?.as_str()?);
    ev.project = v.get("project").and_then(Value::as_str).map(|s| s.to_string());
    ev.model = v.get("model").and_then(Value::as_str).map(|s| s.to_string());
    ev.counts = TokenCounts {
        input: num("in"),
        cache_creation: num("cc"),
        cache_read: num("cr"),
        output: num("out"),
        reasoning: num("reason"),
        credits: num("credits"),
    };
    ev.meter = match v.get("meter").and_then(Value::as_str) {
        Some("credits") => Meter::Credits,
        _ => Meter::Tokens,
    };
    ev.dedupe_key = v.get("id").and_then(Value::as_str).map(|s| s.to_string());
    ev.source = source.to_string_lossy().into_owned();
    if let Some(calls) = v.get("calls").and_then(Value::as_array) {
        ev.calls = calls
            .iter()
            .filter_map(|c| {
                Some(Call {
                    kind: match c.get("kind").and_then(Value::as_str)? {
                        "skill" => CallKind::Skill,
                        _ => CallKind::Mcp,
                    },
                    name: c.get("name").and_then(Value::as_str)?.to_string(),
                })
            })
            .collect();
    }
    if let Some(q) = v.get("quota") {
        ev.quota = Some(usage_core::QuotaSample {
            used_percent: q.get("used_percent")?.as_f64()?,
            window_minutes: q.get("window_minutes")?.as_i64()?,
            resets_at_ms: q.get("resets_at_ms")?.as_i64()?,
            label: q.get("label").and_then(Value::as_str).map(str::to_string),
            id: None,
        });
    }
    Some(ev)
}

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Copies the named fixture files or directories into a fresh temp dir.
pub fn staged(names: &[&str]) -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    for name in names {
        copy(&fixtures().join(name), &tmp.path().join(name));
    }
    tmp
}

fn copy(from: &Path, to: &Path) {
    if from.is_dir() {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            copy(&entry.path(), &to.join(entry.file_name()));
        }
        return;
    }
    std::fs::copy(from, to).unwrap();
}

pub fn adapters_for(dir: &Path) -> Vec<Box<dyn SourceAdapter>> {
    vec![Box::new(Mock::new("mocka", dir)) as Box<dyn SourceAdapter>]
}

pub fn append_fixture(dir: &Path, target: &str, fixture: &str) {
    let extra = std::fs::read(fixtures().join(fixture)).unwrap();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(dir.join(target)).unwrap();
    file.write_all(&extra).unwrap();
    file.flush().unwrap();
}

pub fn replace_file(dir: &Path, target: &str, fixture: &str) {
    let body = std::fs::read(fixtures().join(fixture)).unwrap();
    std::fs::write(dir.join(target), body).unwrap();
}

pub fn count_of(idx: &usage_index::Index, tool: &str) -> u64 {
    idx.per_tool_counts().unwrap().get(tool).copied().unwrap_or(0)
}
