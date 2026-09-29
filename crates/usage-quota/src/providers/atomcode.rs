//! AtomCode: the AtomGit CodingPlan quota, read from the tool's own daemon.
//!
//! ## Why the daemon and not an HTTP endpoint
//!
//! The CodingPlan quota has no direct web endpoint — the numbers live behind
//! the tool's IDE-integration daemon (`atomcode daemon`, which serves
//! `GET /codingplan/usage/summary` on loopback, bearer-authenticated by a
//! per-instance token it writes to `<home>/daemon-<port>.json`). Measured live
//! 2026-09-29: `{"plan":{"name":"CodingPlan Lite-体验版",…},"windows":[
//! {"metric":"calls","window_hours":5,"limit":200,"used":7,"usage_percent":3.0,
//!  "next_reset_at":"2026-09-30T00:48:14",…}]}`.
//!
//! Probe order: reuse a daemon the user already runs (any `daemon-*.json`
//! whose summary answers), else spawn `atomcode daemon` on an ephemeral port,
//! wait for its info file, ask, and kill our own child. The spawn is
//! CREATE_NO_WINDOW — atomcode is a console binary and this runs from the GUI.
//!
//! `next_reset_at` is local wall-clock without a zone; parse as local.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use usage_core::QuotaSample;

use crate::QuotaProbe;

pub struct AtomCodeQuota;

impl QuotaProbe for AtomCodeQuota {
    fn tool(&self) -> &'static str {
        "atomcode"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        // A daemon the user already runs first; else our own, killed afterwards.
        for info in daemon_infos() {
            if let Some(body) = query(&info) {
                return samples_from(&body);
            }
        }
        let Some(mut child) = spawn_daemon() else { return Vec::new() };
        // spawn_daemon only returns once the child published its info file.
        let mut mine: Vec<DaemonInfo> =
            daemon_infos().into_iter().filter(|i| i._pid == child.id()).collect();
        let body = mine.pop().and_then(|i| query(&i));
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        match body {
            Some(b) => samples_from(&b),
            None => Vec::new(),
        }
    }
}

struct DaemonInfo {
    port: u16,
    token: String,
    _pid: u32,
}

/// Every `daemon-*.json` in the AtomCode home, freshest last. The home is the
/// writer's own: `$ATOMCODE_HOME`, else `~/.atomcode`.
fn daemon_infos() -> Vec<DaemonInfo> {
    let home = std::env::var_os("ATOMCODE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| dirs::home_dir().map(|h| h.join(".atomcode")));
    let Some(home) = home else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(home) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("daemon-") || !name.ends_with(".json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let (Some(port), Some(token)) = (
            v.get("port").and_then(Value::as_u64).map(|p| p as u16),
            v.get("token").and_then(Value::as_str).map(str::to_string),
        ) else {
            continue;
        };
        out.push(DaemonInfo { port, token, _pid: v.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32 });
    }
    out
}

fn query(info: &DaemonInfo) -> Option<Value> {
    crate::http::get_json(
        &format!("http://127.0.0.1:{}/codingplan/usage/summary", info.port),
        &[
            ("authorization", format!("Bearer {}", info.token).as_str()),
            ("accept", "application/json"),
        ],
    )
}

/// Spawn `atomcode daemon` (ephemeral port) and wait for its info file. The
/// child is returned for the caller to kill after use; a daemon that fails to
/// start answers `None`.
fn spawn_daemon() -> Option<Child> {
    let exe = atomcode_exe()?;
    let before: Vec<PathBuf> = daemon_files();
    let mut command = Command::new(exe);
    command.arg("daemon").stdout(Stdio::null()).stderr(Stdio::null()).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        for path in daemon_files() {
            if before.contains(&path) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if v.get("pid").and_then(Value::as_u64) == Some(child.id() as u64) {
                        return Some(child);
                    }
                }
            }
        }
    }
    let _ = child.kill();
    None
}

fn daemon_files() -> Vec<PathBuf> {
    let home = std::env::var_os("ATOMCODE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| dirs::home_dir().map(|h| h.join(".atomcode")));
    let Some(home) = home else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(home) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("daemon-") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect()
}

/// The installed CLI: the per-user install first, then PATH.
fn atomcode_exe() -> Option<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let candidate = home
            .join("AppData")
            .join("Local")
            .join("AtomCode")
            .join("atomcode.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    which("atomcode")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for extension in ["", ".exe"] {
            let candidate = dir.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// One sample per window, vendor order; the label follows the zcode idiom
/// (`5 小时 · <plan>`), the group header hoists the plan.
fn samples_from(body: &Value) -> Vec<QuotaSample> {
    if body.get("available").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let plan = body
        .pointer("/plan/name")
        .and_then(Value::as_str)
        .unwrap_or("CodingPlan");
    let windows = body.get("windows").and_then(Value::as_array);
    let Some(windows) = windows else { return Vec::new() };
    windows
        .iter()
        .filter_map(|w| {
            let hours = w.get("window_hours").and_then(Value::as_f64)?;
            let used = w.get("used").and_then(Value::as_f64)?;
            let limit = w.get("limit").and_then(Value::as_f64)?;
            if limit <= 0.0 {
                return None;
            }
            let used_percent = w
                .get("usage_percent")
                .and_then(Value::as_f64)
                .unwrap_or_else(|| used / limit * 100.0)
                .clamp(0.0, 100.0);
            // `next_reset_at` is local wall-clock, no zone — the same shape
            // `parse_ts_ms` treats as seconds would misread; parse it as local.
            let resets_at_ms = w
                .get("next_reset_at")
                .and_then(Value::as_str)
                .and_then(|s| parse_local_ms(s));
            let label = format!(
                "{} 小时 · {} · 已用 {}/{}",
                hours as i64,
                plan,
                used as i64,
                limit as i64
            );
            Some(QuotaSample {
                used_percent,
                window_minutes: (hours * 60.0) as i64,
                resets_at_ms: resets_at_ms.unwrap_or(0),
                label: Some(label),
                id: Some(format!("codingplan:{}h", hours as i64)),
            })
        })
        .collect()
}

/// `2026-09-30T00:48:14` — local wall-clock, no zone. Treat it as local the
/// same way the writer does.
fn parse_local_ms(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    // Reuse the vendor-order parser for the naive-local shape by passing it
    // through with an explicit local offset applied later in the panel? No:
    // chrono is not a dependency here — hand-roll the civil-to-epoch math.
    let (date, time) = trimmed.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: f64 = t.next().unwrap_or("0").parse().ok()?;
    // days from civil (Howard Hinnant)
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60) * 1000 + (second * 1000.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{
      "schema_version": 1, "available": true,
      "plan": {"name": "CodingPlan Lite-体验版", "status": 1, "claimed_at": "2026-09-29",
               "expires_at": "2026-10-06", "total_days": 7, "remaining_days": 7},
      "primary_window": {"metric": "calls", "window_hours": 5, "limit": 200, "used": 7,
                         "usage_percent": 3.0, "next_reset_at": "2026-09-30T00:48:14"},
      "windows": [{"metric": "calls", "window_hours": 5, "window_size_seconds": 18000,
                   "limit": 200, "used": 7, "remaining": 193, "usage_percent": 3.0,
                   "quota_exhausted": false, "next_reset_at": "2026-09-30T00:48:14"}],
      "quota_hint": null
    }"#;

    #[test]
    fn the_summary_becomes_one_bar_per_window() {
        let body: Value = serde_json::from_str(LIVE).unwrap();
        let s = samples_from(&body);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].used_percent, 3.0);
        assert_eq!(s[0].window_minutes, 300);
        assert_eq!(s[0].id.as_deref(), Some("codingplan:5h"));
        let label = s[0].label.as_deref().unwrap();
        assert!(label.contains("5 小时") && label.contains("Lite-体验版") && label.contains("7/200"), "{label}");
        // Local wall-clock 2026-09-30T00:48:14 lands in the future, same day.
        assert!(s[0].resets_at_ms > 0);
    }

    #[test]
    fn an_unavailable_plan_answers_nothing() {
        for raw in [
            r#"{"available":false,"windows":[]}"#,
            r#"{"available":true}"#,
            r#"{"available":true,"windows":[{"window_hours":5,"limit":0,"used":0}]}"#,
        ] {
            let v: Value = serde_json::from_str(raw).unwrap();
            assert!(samples_from(&v).is_empty(), "{raw}");
        }
    }
}
