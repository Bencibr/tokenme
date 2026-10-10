//! Read-through TTL cache for quota probes.
//!
//! A probe may shell out to `security`, spawn the vendor CLI, or make an HTTPS
//! call. The menu-bar process refreshes every couple of seconds on log activity,
//! so without this cache every keystroke in a session would re-probe every
//! vendor. One JSON file per tool under the app-support dir; writes are
//! tmp+rename so a crash mid-write can only leave the previous good file.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use usage_core::{replace_file, QuotaSample, QuotaView};

use crate::QuotaProbe;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Entry {
    used_percent: f64,
    window_minutes: i64,
    resets_at_ms: i64,
    #[serde(default)]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Cached {
    /// v2 = samples whose credit packs carry their own window id. v1 files
    /// (no version field) packed all of one tool's packs under a shared id,
    /// and replaying them re-fired the exhaustion banner through the
    /// collision — old files are ignored on read, never upgraded in place.
    #[serde(default)]
    version: u32,
    captured_at_ms: i64,
    entries: Vec<Entry>,
}

/// See [`Cached::version`]. Bump when the cached shape's identity semantics
/// change, so an upgrade never replays samples written under the old rules.
const VERSION: u32 = 2;

pub struct Cache {
    dir: PathBuf,
    ttl: Duration,
}

impl Cache {
    /// `None` when the platform has no app-support dir; probes then run uncached.
    pub fn open(subdir: &str, ttl: Duration) -> Option<Self> {
        let dir = dirs::config_dir()?.join(subdir);
        fs::create_dir_all(&dir).ok()?;
        Some(Self { dir, ttl })
    }

    pub fn in_dir(dir: PathBuf, ttl: Duration) -> Self {
        Self { dir, ttl }
    }

    fn path(&self, tool: &str) -> PathBuf {
        self.dir.join(format!("{tool}.json"))
    }

    fn read(&self, tool: &str) -> Option<Cached> {
        let text = fs::read_to_string(self.path(tool)).ok()?;
        let cached = serde_json::from_str::<Cached>(&text).ok()?;
        (cached.version == VERSION).then_some(cached)
    }

    fn write(&self, tool: &str, cached: &Cached) {
        let Ok(text) = serde_json::to_string(cached) else { return };
        let path = self.path(tool);
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, text.as_bytes()).is_ok() {
            let _ = replace_file(&tmp, &path);
        }
    }

    /// Fresh samples straight from disk, if the last probe is still valid.
    pub fn fresh(&self, tool: &str) -> Option<Vec<QuotaView>> {
        let cached = self.read(tool)?;
        if now_ms() - cached.captured_at_ms > self.ttl.as_millis() as i64 {
            return None;
        }
        Some(views(tool, cached.captured_at_ms, &cached.entries))
    }

    /// A cached answer no older than `max`, regardless of the probe TTL. Used to
    /// decide whether a failed fetch may overwrite good values.
    pub fn within(&self, tool: &str, max: Duration) -> Option<Vec<QuotaView>> {
        let cached = self.read(tool)?;
        if now_ms() - cached.captured_at_ms > max.as_millis() as i64 {
            return None;
        }
        Some(views(tool, cached.captured_at_ms, &cached.entries))
    }

    /// Drop every cached answer. The manual "refresh everything" path calls
    /// this before collecting, so the next pass re-probes every vendor for
    /// real instead of reading a still-fresh TTL entry.
    pub fn clear(&self) {
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    /// Cached samples regardless of age, used when a probe fails or is offline.
    pub fn stale(&self, tool: &str) -> Option<Vec<QuotaView>> {
        let cached = self.read(tool)?;
        Some(views(tool, cached.captured_at_ms, &cached.entries))
    }

    pub fn store(&self, tool: &str, samples: &[QuotaSample]) {
        let at = now_ms();
        let entries = samples
            .iter()
            .map(|s| Entry {
                used_percent: s.used_percent,
                window_minutes: s.window_minutes,
                resets_at_ms: s.resets_at_ms,
                label: s.label.clone(),
                id: s.id.clone(),
            })
            .collect();
        self.write(tool, &Cached { version: VERSION, captured_at_ms: at, entries });
    }
}

fn views(tool: &str, captured_at_ms: i64, entries: &[Entry]) -> Vec<QuotaView> {
    entries
        .iter()
        .map(|e| QuotaView {
            tool: tool.to_string(),
            used_percent: e.used_percent,
            window_minutes: e.window_minutes,
            resets_at_ms: e.resets_at_ms,
            sampled_at_ms: captured_at_ms,
            label: e.label.clone(),
            id: e.id.clone(),
            origin: usage_core::QuotaOrigin::Probe,
        })
        .collect()
}

/// Cached-then-live probe: disk if fresh, otherwise fetch and remember.
///
/// A *negative* answer is cached too. Without that, a tool that legitimately
/// has no quota surface (not signed in, not subscribed, proxied credentials)
/// would pay its full connect+read timeout on every single refresh, which in
/// aggregate is what made `quota` take half a minute.
pub fn probe(cache: Option<&Cache>, source: &dyn QuotaProbe) -> Vec<QuotaView> {
    if let Some(cache) = cache {
        if let Some(fresh) = cache.fresh(source.tool()) {
            return fresh;
        }
    }
    // One attempt per tool at a time. A probe that blocks longer than the
    // caller's budget (a keychain prompt nobody answered, a TLS hang) is still
    // running when the next refresh cycle starts, and without this gate every
    // cycle would start another one — dozens of stuck `agy` / `security`
    // processes. While one is in flight the caller gets whatever is cached.
    let Some(_flight) = Flight::claim(source.tool()) else {
        return cache.and_then(|c| c.within(source.tool(), GRACE)).unwrap_or_default();
    };
    let samples = source.guarded_fetch();
    if samples.is_empty() {
        // Keep a good answer for a grace period rather than blanking the bar
        // because one request timed out; after that, remember the "nothing here"
        // verdict so the next cycle costs nothing.
        if let Some(cache) = cache {
            if let Some(recent) = cache.within(source.tool(), GRACE) {
                if !recent.is_empty() {
                    return recent;
                }
            }
            cache.store(source.tool(), &samples);
        }
        return Vec::new();
    }
    if let Some(cache) = cache {
        cache.store(source.tool(), &samples);
    }
    views(source.tool(), now_ms(), &entries_of(&samples))
}

/// RAII registration of "this tool's probe is running right now".
struct Flight(&'static str);

static IN_FLIGHT: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();

fn table() -> &'static Mutex<HashSet<String>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

impl Flight {
    /// `None` when a probe for this tool is already running.
    fn claim(tool: &'static str) -> Option<Self> {
        // A panic inside a probe leaves the mutex poisoned, not the data wrong;
        // losing the gate is worse than recovering the set.
        let mut guard = table().lock().unwrap_or_else(|e| e.into_inner());
        if !guard.insert(tool.to_string()) {
            return None;
        }
        Some(Self(tool))
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if let Ok(mut guard) = table().lock() {
            guard.remove(self.0);
        }
    }
}

/// How long a failed fetch may keep showing the previous numbers.
pub const GRACE: Duration = Duration::from_secs(6 * 60 * 60);

fn entries_of(samples: &[QuotaSample]) -> Vec<Entry> {
    samples
        .iter()
        .map(|s| Entry {
            used_percent: s.used_percent,
            window_minutes: s.window_minutes,
            resets_at_ms: s.resets_at_ms,
            label: s.label.clone(),
            id: s.id.clone(),
        })
        .collect()
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn cache_dir(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

#[cfg(test)]
mod cache_clear_tests {
    use super::Cache;

    #[test]
    fn clear_forces_the_next_read_to_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::in_dir(dir.path().to_path_buf(), crate::TTL);
        cache.store("clear-test", &[]);
        assert!(cache.fresh("clear-test").is_some(), "stored answer reads back fresh");

        cache.clear();
        assert!(
            cache.fresh("clear-test").is_none(),
            "after clear the read misses, so the next collect re-probes for real"
        );
    }
}

#[cfg(test)]
mod flight_tests {
    use super::Flight;

    #[test]
    fn a_probe_is_only_in_flight_once() {
        let first = Flight::claim("flight-test-probe");
        assert!(first.is_some(), "an idle tool claims");
        assert!(Flight::claim("flight-test-probe").is_none(), "a second attempt must not start a duplicate");
        drop(first);
        assert!(Flight::claim("flight-test-probe").is_some(), "the slot frees when the probe returns");
    }

    #[test]
    fn one_tools_flight_does_not_block_another() {
        let a = Flight::claim("flight-a");
        assert!(a.is_some());
        assert!(Flight::claim("flight-b").is_some());
    }
}
