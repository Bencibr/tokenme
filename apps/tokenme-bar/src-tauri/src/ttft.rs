//! Opt-in TTFT collection and a small independent snapshot channel.
//!
//! Codex is sampled from local rollout completion records. Claude Code may
//! POST OTLP/HTTP JSON to a loopback-only receiver. When enabled, TokenMe
//! safely merges the required trace variables into the user's Claude settings
//! and removes only its own unchanged values when disabled.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use usage_core::{DateFilter, SourceAdapter};

use crate::claude_otel_ttft::{MetricField, ServerHandle};

const EVENT: &str = "ttft-updated";
const CLAUDE_PORT: u16 = 4319;
const SAMPLE_DAYS: i64 = 30;
const MAX_SAMPLES: usize = 1000;
const INITIAL_SCAN_LIMIT: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TtftSample {
    pub provider: String,
    pub granularity: String,
    pub metric: String,
    pub ttft_ms: f64,
    pub sampled_at_ms: i64,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TtftSnapshot {
    pub revision: u64,
    pub enabled: bool,
    pub claude_receiver: String,
    pub claude_config_status: String,
    pub samples: Vec<TtftSample>,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredSamples {
    samples: Vec<TtftSample>,
}

struct Inner {
    enabled: bool,
    claude_receiver: String,
    claude_config_status: String,
    revision: u64,
    samples: Vec<TtftSample>,
    server: Option<ServerHandle>,
}

#[derive(Clone)]
pub struct TtftState {
    inner: Arc<Mutex<Inner>>,
    path: PathBuf,
    app: AppHandle,
}

impl TtftState {
    pub fn new(app: AppHandle, path: PathBuf, enabled: bool) -> Self {
        let stored = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StoredSamples>(&bytes).ok())
            .unwrap_or_default();
        let now = now_ms();
        let samples = stored.samples.into_iter()
            .filter(|sample| sample.sampled_at_ms >= now - SAMPLE_DAYS * 86_400_000 && valid_sample(sample))
            .take(MAX_SAMPLES).collect::<Vec<_>>();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                enabled,
                claude_receiver: if enabled { "unavailable".into() } else { "disabled".into() },
                claude_config_status: if enabled { "pending".into() } else { "disabled".into() },
                revision: 0,
                samples,
                server: None,
            })),
            path,
            app,
        }
    }

    pub fn start(&self) {
        let enabled = if let Ok(mut inner) = self.inner.lock() {
            if inner.enabled { self.start_receiver(&mut inner); }
            inner.enabled
        } else { false };
        if enabled { self.configure_claude_on_start(); }
        start_codex_scanner(self.clone());
    }

    fn configure_claude_on_start(&self) {
        let listening = self.inner.lock().is_ok_and(|inner| inner.claude_receiver == "listening");
        let status = if !listening {
            match crate::claude_otel_config::disable(&self.owner_path()) {
                Ok(()) => "receiver_unavailable",
                Err(error) => {
                    crate::logging::error(&format!("ttft: stale Claude OTLP settings could not be removed: {error}"));
                    "error"
                }
            }
        } else if let Err(error) = crate::claude_otel_config::enable(&self.owner_path()) {
            crate::logging::error(&format!("ttft: Claude settings were not changed: {error}"));
            "error"
        } else {
            "configured"
        };
        self.set_config_status(status);
    }

    fn owner_path(&self) -> PathBuf {
        self.path.with_file_name("ttft-claude-otel-ownership.json")
    }

    fn set_config_status(&self, status: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.claude_config_status = status.into();
            inner.revision = inner.revision.wrapping_add(1);
        }
        self.publish();
    }

    pub fn snapshot(&self) -> TtftSnapshot {
        self.inner.lock().map(|inner| TtftSnapshot {
            revision: inner.revision,
            enabled: inner.enabled,
            claude_receiver: inner.claude_receiver.clone(),
            claude_config_status: inner.claude_config_status.clone(),
            samples: inner.samples.clone(),
        }).unwrap_or(TtftSnapshot { revision: 0, enabled: false, claude_receiver: "error".into(), claude_config_status: "error".into(), samples: Vec::new() })
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        if enabled {
            let Ok(mut inner) = self.inner.lock() else { return Err("TTFT service is busy.".into()) };
            if !inner.enabled {
                inner.enabled = true;
                self.start_receiver(&mut inner);
            }
            let listening = inner.claude_receiver == "listening";
            drop(inner);
            if listening {
                if let Err(error) = crate::claude_otel_config::enable(&self.owner_path()) {
                    let server = if let Ok(mut inner) = self.inner.lock() {
                        inner.enabled = false;
                        inner.claude_config_status = "error".into();
                        inner.claude_receiver = "disabled".into();
                        inner.revision = inner.revision.wrapping_add(1);
                        inner.server.take()
                    } else { None };
                    if let Some(server) = server { server.shutdown(); }
                    self.publish();
                    return Err(error);
                }
                self.set_config_status("configured");
            } else {
                if let Err(error) = crate::claude_otel_config::disable(&self.owner_path()) {
                    let Ok(mut inner) = self.inner.lock() else { return Err(error) };
                    inner.enabled = false;
                    inner.claude_config_status = "error".into();
                    inner.revision = inner.revision.wrapping_add(1);
                    drop(inner);
                    self.publish();
                    return Err(error);
                }
                self.set_config_status("receiver_unavailable");
            }
            self.publish();
            Ok(())
        } else {
            crate::claude_otel_config::disable(&self.owner_path())?;
            let Ok(mut inner) = self.inner.lock() else { return Err("TTFT service is busy.".into()) };
            if !inner.enabled { return Ok(()); }
            inner.enabled = false;
            inner.claude_receiver = "disabled".into();
            inner.claude_config_status = "disabled".into();
            inner.revision = inner.revision.wrapping_add(1);
            let server = inner.server.take();
            drop(inner);
            if let Some(server) = server { server.shutdown(); }
            self.publish();
            Ok(())
        }
    }

    fn start_receiver(&self, inner: &mut Inner) {
        let state = self.clone();
        match crate::claude_otel_ttft::start_server(CLAUDE_PORT, move |received| {
            let samples = received.into_iter().map(|sample| {
                let (metric, millis) = match sample.field {
                    MetricField::TtftMs => ("ttft", sample.millis),
                    MetricField::FirstContentMs => ("first_content", sample.millis),
                };
                TtftSample {
                    provider: "claude_code".into(), granularity: "request".into(), metric: metric.into(),
                    ttft_ms: millis, sampled_at_ms: sample.sampled_at_ms.min(i64::MAX as u64) as i64, model: sample.model,
                }
            }).collect();
            state.add_samples(samples);
        }) {
            Ok(server) => {
                crate::logging::info(&format!("ttft: Claude OTLP receiver listening on {}", server.local_addr()));
                inner.server = Some(server);
                inner.claude_receiver = "listening".into();
            }
            Err(error) => {
                crate::logging::error(&format!("ttft: Claude OTLP receiver unavailable: {error}"));
                inner.server = None;
                inner.claude_receiver = "unavailable".into();
            }
        }
    }

    fn add_samples(&self, samples: Vec<TtftSample>) {
        let samples = samples.into_iter().filter(valid_sample).collect::<Vec<_>>();
        if samples.is_empty() { return; }
        let Ok(mut inner) = self.inner.lock() else { return };
        if !inner.enabled { return; }
        for sample in samples.into_iter().rev() { inner.samples.insert(0, sample); }
        let cutoff = now_ms().saturating_sub(SAMPLE_DAYS * 86_400_000);
        inner.samples.retain(|sample| sample.sampled_at_ms >= cutoff);
        inner.samples.truncate(MAX_SAMPLES);
        inner.revision = inner.revision.wrapping_add(1);
        let snapshot = TtftSnapshot {
            revision: inner.revision, enabled: inner.enabled,
            claude_receiver: inner.claude_receiver.clone(),
            claude_config_status: inner.claude_config_status.clone(),
            samples: inner.samples.clone(),
        };
        drop(inner);
        self.persist();
        let _ = self.app.emit(EVENT, snapshot);
    }

    fn publish(&self) {
        let _ = self.app.emit(EVENT, self.snapshot());
    }

    fn persist(&self) {
        let samples = self.snapshot().samples;
        if let Some(parent) = self.path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                crate::logging::error(&format!("ttft: create sample directory failed: {error}"));
                return;
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        let write = serde_json::to_vec(&StoredSamples { samples })
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(error) = write { crate::logging::error(&format!("ttft: persist samples failed: {error}")); }
    }
}

fn valid_sample(sample: &TtftSample) -> bool {
    sample.ttft_ms.is_finite() && (0.0..=600_000.0).contains(&sample.ttft_ms)
        && matches!(sample.provider.as_str(), "codex" | "claude_code")
        && matches!(sample.metric.as_str(), "ttft" | "first_content")
        && sample.model.as_ref().is_none_or(|model| model.len() <= 128 && !model.chars().any(char::is_control))
        && sample.sampled_at_ms <= now_ms().saturating_add(300_000)
}

fn start_codex_scanner(state: TtftState) {
    let _ = std::thread::Builder::new().name("codex-ttft-scan".into()).spawn(move || {
        let adapter = usage_adapter_codex::CodexAdapter;
        let mut cursors = HashMap::<String, usage_adapter_codex::CodexTtftCursor>::new();
        let mut files = Vec::<usage_core::SourceFile>::new();
        let mut last_discover = std::time::Instant::now() - Duration::from_secs(60);
        let mut initial_discovery = true;
        let mut was_enabled = false;
        loop {
            let enabled = state.snapshot().enabled;
            if enabled {
                if !was_enabled {
                    initial_discovery = true;
                    files.clear();
                    cursors.clear();
                    last_discover = std::time::Instant::now() - Duration::from_secs(60);
                }
                let mut skip_existing = false;
                if last_discover.elapsed() >= Duration::from_secs(30) {
                    skip_existing = initial_discovery;
                    let since = now_ms().saturating_sub(SAMPLE_DAYS * 86_400_000);
                    files = adapter.discover(&DateFilter::new(Some(since), None));
                    let keys = files.iter().map(|file| file.key()).collect::<std::collections::HashSet<_>>();
                    cursors.retain(|key, _| keys.contains(key));
                    last_discover = std::time::Instant::now();
                    initial_discovery = false;
                }
                for file in &files {
                    let key = file.key();
                    let Some(file) = file.clone().restat() else { continue };
                    let cursor = cursors.entry(key).or_insert_with(|| {
                        // Existing files may be multi-gigabyte and include old
                        // sessions. Collection starts at enable/start time; new
                        // rollout files are read from their first complete line.
                        if skip_existing || file.size > INITIAL_SCAN_LIMIT { usage_adapter_codex::CodexTtftCursor::at_offset(file.size) }
                        else { usage_adapter_codex::CodexTtftCursor::default() }
                    }).clone();
                    match usage_adapter_codex::read_ttft(&file.path, cursor) {
                        Ok(result) => {
                            cursors.insert(file.key(), result.cursor);
                            let samples = result.samples.into_iter().map(|sample| TtftSample {
                                    provider: "codex".into(), granularity: "turn".into(), metric: "ttft".into(),
                                    ttft_ms: sample.ttft_ms as f64,
                                    sampled_at_ms: sample.completed_at_ms.unwrap_or_else(now_ms),
                                    model: sample.model.filter(|model| model.len() <= 128 && !model.chars().any(char::is_control)),
                                }).collect();
                            state.add_samples(samples);
                        }
                        Err(error) => crate::logging::error(&format!("ttft: Codex rollout read failed: {error}")),
                    }
                }
                was_enabled = true;
            } else {
                was_enabled = false;
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64
}
