//! JoyCode adapter (JD's VS Code-family AI IDE, extension `joycode.joycoder-editor`).
//!
//! One source per task: `<storage-root>/<task-id>/task_history/task-<uuid>.json`
//! — the agent's own cumulative billing record, carrying `tokensIn`, `tokensOut`,
//! `cacheWrites`, `cacheReads` and `totalCost` alongside the task's text,
//! creation time and workspace path. The file is written when a task closes and
//! updated in place while it runs, so each file is one Cumulative event replaced
//! on the stable `joycode#<task-id>` key — the same contract the hermes slices
//! use. `totalCost` stays 0 on subscription plans, so tokens are the only meter.
//!
//! Two storage trees exist and both are read: the VS Code per-window
//! `workspaceStorage/<hash>/JoyCode.joycoder-editor/` (where real project tasks
//! land) and the agent home `~/.joycode/<workspace>/…` (where scratch tasks
//! land). A task id seen in a second tree is skipped — first root wins.
//!
//! Tasks whose usage is all zero (a prompt the model answered without a billable
//! call, or a model that reports no usage) are skipped: zero tokens is silence,
//! not a session.

use std::path::PathBuf;

use serde_json::Value;
use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, Meter, ReadCursor, ReadOutcome, Semantics,
    SourceAdapter, SourceFile, TokenCounts, UsageEvent, UsageForm,
};
use walkdir::WalkDir;

pub const TOOL_ID: &str = "joycode";

/// Overrides both storage trees with a single task-history directory — the
/// fixture and relocated-install escape hatch.
pub const ENV_JOYCODE_TASK_DIR: &str = "JOYCODE_TASK_DIR";

#[derive(Debug, Default, Clone, Copy)]
pub struct JoycodeAdapter;

/// The task-history directories, priority order. `workspaceStorage` first: it
/// is where the IDE writes the tasks that belong to real projects; the agent
/// home tree is where scratch tasks land. `JOYCODE_TASK_DIR` (the env override)
/// replaces both with a single directory.
fn task_dirs() -> Vec<PathBuf> {
    if let Some(dir) = env_dir(ENV_JOYCODE_TASK_DIR) {
        return dir.is_dir().then_some(dir).into_iter().collect();
    }
    let mut patterns: Vec<PathBuf> = Vec::new();
    if let Some(data) = dirs::data_dir() {
        patterns.push(data.join("JoyCode/User/workspaceStorage"));
    }
    if let Some(home) = dirs::home_dir() {
        patterns.push(home.join(".joycode"));
    }
    let fixed = PathBuf::from("JoyCode.joycoder-editor").join("task_history");
    let mut out = Vec::new();
    for base in patterns {
        let entries = match std::fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let dir = entry.path().join(&fixed);
            if dir.is_dir() {
                out.push(dir);
            }
        }
    }
    out
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|d| !d.is_empty()).map(PathBuf::from)
}

/// `task-<uuid>.json` files across every root, one per task id, first root wins.
fn task_files() -> Vec<PathBuf> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in task_dirs() {
        let entries = WalkDir::new(&dir)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("task-") && n.ends_with(".json"))
            });
        for path in entries {
            if seen.insert(path.file_name().unwrap().to_string_lossy().into_owned()) {
                out.push(path);
            }
        }
    }
    out
}

impl SourceAdapter for JoycodeAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "JoyCode"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One event per task carrying the task's whole-to-date totals; the
            // indexer replaces on the dedupe key as the file grows.
            usage_form: UsageForm::Cumulative,
            meter: Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let tasks = task_files();
        let count = tasks.len();
        (count > 0).then(|| DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: task_dirs(),
            hint: Some(format!("{count} tasks")),
        })
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        // One file holds one task's whole life, so `DateFilter` cannot prune
        // the listing; the cumulative event is what the windows slice.
        task_files()
            .into_iter()
            .filter_map(|path| {
                let meta = std::fs::metadata(&path).ok()?;
                let mtime_ms = meta
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis() as i64;
                Some(SourceFile { path, kind: FileKind::Tree, size: meta.len(), mtime_ms })
            })
            .collect()
    }

    fn read(&self, file: &SourceFile, _cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        match file.kind {
            FileKind::Tree => Ok(read_task(file)?),
            _ => Ok(ReadOutcome { events: Vec::new(), cursor: ReadCursor(file.size as u64) }),
        }
    }
}

/// One cumulative event per task file, replaced on the stable key each pass.
fn read_task(file: &SourceFile) -> Result<ReadOutcome, Error> {
    let empty = ReadOutcome { events: Vec::new(), cursor: ReadCursor(file.size as u64) };
    let Ok(text) = std::fs::read_to_string(&file.path) else { return Ok(empty) };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Ok(empty) };
    let num = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
    let counts = TokenCounts {
        input: num("tokensIn"),
        output: num("tokensOut"),
        cache_creation: num("cacheWrites"),
        cache_read: num("cacheReads"),
        reasoning: 0.0,
        credits: 0.0,
    };
    if counts.is_zero() {
        return Ok(empty);
    }
    let id = v.get("id").and_then(Value::as_str).unwrap_or_else(|| {
        file.path.file_stem().and_then(|s| s.to_str()).unwrap_or_default()
    });
    let mut event = UsageEvent::new(TOOL_ID, file.mtime_ms, id);
    event.counts = counts;
    event.meter = Meter::Tokens;
    // JoyCode writes no model name into the task record — the row stays
    // model-less rather than guessing the gateway's current default.
    event.project = v
        .get("workspacePath")
        .and_then(Value::as_str)
        .and_then(|p| p.rsplit('/').find(|s| !s.is_empty()))
        .map(str::to_string);
    event.dedupe_key = Some(format!("joycode#{id}"));
    event.source = file.key();
    Ok(ReadOutcome { events: vec![event], cursor: ReadCursor(file.size as u64) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: &str = r#"{
  "id": "task-3783df03-b15d-4e97-b998-87679e17fd36",
  "ts": 1790052055000,
  "task": "审计CLI面域20个模块",
  "tokensIn": 87315,
  "tokensOut": 8210,
  "cacheWrites": 0,
  "cacheReads": 0,
  "totalCost": 0,
  "workspacePath": "/Users/sp/workspace/bug-hunter",
  "modeName": "编码"
}"#;

    fn fixture_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("ws1").join("JoyCode.joycoder-editor").join("task_history");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::write(history.join("task-3783df03.json"), RECORD).unwrap();
        dir
    }

    #[test]
    fn a_task_record_becomes_one_cumulative_event() {
        let _env = crate::lock_env();
        let dir = fixture_dir();
        std::env::set_var(ENV_JOYCODE_TASK_DIR, dir.path().join("ws1").join("JoyCode.joycoder-editor").join("task_history"));
        let adapter = JoycodeAdapter;
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1);
        let outcome = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 1);
        let e = &outcome.events[0];
        assert_eq!(e.tool, "joycode");
        assert_eq!(e.session, "task-3783df03-b15d-4e97-b998-87679e17fd36");
        assert_eq!(
            e.counts,
            TokenCounts { input: 87315.0, cache_creation: 0.0, cache_read: 0.0, output: 8210.0, reasoning: 0.0, credits: 0.0 }
        );
        assert_eq!(e.project.as_deref(), Some("bug-hunter"));
        assert_eq!(e.dedupe_key.as_deref(), Some("joycode#task-3783df03-b15d-4e97-b998-87679e17fd36"));
        assert_eq!(e.meter, Meter::Tokens);
        // A re-read re-emits the same key: the indexer replaces, never doubles.
        let again = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(again.events[0].dedupe_key, e.dedupe_key);
        std::env::remove_var(ENV_JOYCODE_TASK_DIR);
    }

    #[test]
    fn a_zero_usage_task_is_silence_not_a_session() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("task_history");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::write(
            history.join("task-zero.json"),
            r#"{"id":"task-zero","tokensIn":0,"tokensOut":0,"cacheWrites":0,"cacheReads":0,"totalCost":0,"task":"hi"}"#,
        )
        .unwrap();
        std::env::set_var(ENV_JOYCODE_TASK_DIR, history);
        let adapter = JoycodeAdapter;
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1, "the file is discovered");
        let outcome = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert!(outcome.events.is_empty(), "zero usage is not a session");
        std::env::remove_var(ENV_JOYCODE_TASK_DIR);
    }
}

#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
