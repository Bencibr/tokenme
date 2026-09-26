//! Antigravity: the quota the CLI already computes for its own `/usage` command.
//!
//! ## Why this runs the vendor's CLI instead of calling Google
//!
//! The quota is only served over OAuth'd endpoints
//! (`cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary` and the
//! language server's local twin), and this machine's stored credentials are
//! expired. Refreshing them is *not* what this probe does: Google can rotate the
//! `refresh_token` on refresh, and every tool that refreshes persists the new one
//! (`orrisroot/agy-usage:src/api/client.rs:256-258`,
//! `skainguyen1412/antigravity-usage:src/google/oauth.ts` →
//! `token-manager.ts:157`). A probe that refreshed without writing back would
//! orphan the user's Antigravity login. So we ask the CLI, which owns that
//! lifecycle, and read its answer:
//!
//! ```text
//! agy -p /usage --output-format json --print-timeout 10s
//! ```
//!
//! answering `{"status":"SUCCESS","command":{"name":"usage","data":{"groups":
//! [{"name":"Gemini Models","buckets":[{"id":"gemini-weekly","name":"Weekly Limit
//! Remaining","window":"weekly","remaining_fraction":0.9499,
//! "reset_time":"2026-09-30T03:34:13Z"}]}]}}}` — the same shape
//! `steipete/CodexBar:Sources/CodexBarCore/Providers/Antigravity/AntigravityStatusProbe.swift:259-317`
//! and `simota/llm-usage:Sources/LLMUsage/AgyProvider.swift:170-229` parse.
//!
//! Costs: one subprocess per cache miss (5 min, [`crate::TTL`]), and the CLI's own
//! quota manager is throttled upstream, so this cannot hammer it. Zero token
//! counts come back from it (`usage.input_tokens` is 0 by design) — the spend side
//! stays with the protobuf conversations adapter, as ever.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::QuotaProbe;

pub struct AntigravityQuota;

/// The CLI's own window names, and the one hard requirement for the JSON shape:
/// `/usage --output-format json` only exists from 1.1.11 (`CodexBar`
/// `AgyProvider.swift:186-193` gates on it the same way).
const WINDOW_MINUTES: [(&str, i64); 2] = [("weekly", 10_080), ("5h", 300)];
/// Measured on this machine: 10.4 s warm, and a run that needed the full
/// `--print-timeout` was killed at 12 s. The wait has to outlast the CLI's own
/// budget or the answer is thrown away, so this sits above its 10 s print timeout
/// and below the 30 s a one-shot CLI command is willing to spend.
const RUN_BUDGET: Duration = Duration::from_secs(25);

impl QuotaProbe for AntigravityQuota {
    fn tool(&self) -> &'static str {
        "antigravity"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(body) = run_usage() else { return Vec::new() };
        match serde_json::from_str(&body) {
            Ok(value) => samples_from(&value),
            Err(_) => Vec::new(),
        }
    }
}

/// Locate the binary the same way the installer does: `~/.local/bin/agy`, else PATH.
fn binary() -> Option<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let candidate = home.join(".local").join("bin").join("agy");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    which("agy")
}

fn which(name: &str) -> Option<PathBuf> {
    let out = Command::new("/usr/bin/env").args(["sh", "-c", &format!("command -v {name}")]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(PathBuf::from(path))
}

/// `{}` on every failure path: no CLI, no network, an old CLI, a timeout.
fn run_usage() -> Option<String> {
    let bin = binary()?;
    let mut child = Command::new(bin)
        .args(["-p", "/usage", "--output-format", "json", "--print-timeout", "10s"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + RUN_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let mut buf = Vec::new();
    use std::io::Read as _;
    child.stdout.as_mut().and_then(|s| s.read_to_end(&mut buf).ok())?;
    let text = String::from_utf8_lossy(&buf).to_string();
    (!text.is_empty()).then_some(text)
}

/// `remaining_fraction` is the share *left*, so it inverts. A bucket without one is
/// unknown, not free: protobuf drops a zero, so the field is simply absent.
pub(crate) fn samples_from(value: &Value) -> Vec<QuotaSample> {
    if value.get("status").and_then(Value::as_str) != Some("SUCCESS") {
        return Vec::new();
    }
    let Some(groups) = value.pointer("/command/data/groups").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for group in groups.iter().filter_map(Value::as_object) {
        // `groups[].buckets` — the array shape above is the CLI's, keep tolerant.
        let name = group.get("name").and_then(Value::as_str).unwrap_or("Antigravity");
        let Some(buckets) = group.get("buckets").and_then(Value::as_array) else { continue };
        let mut rows: Vec<QuotaSample> = buckets
            .iter()
            .filter_map(Value::as_object)
            .filter(|bucket| !bucket.get("disabled").and_then(Value::as_bool).unwrap_or(false))
            .filter_map(|bucket| {
                let remaining = bucket.get("remaining_fraction").and_then(Value::as_f64)?;
                if !(0.0..=1.0).contains(&remaining) {
                    return None;
                }
                let window = bucket.get("window").and_then(Value::as_str).unwrap_or_default();
                let minutes = WINDOW_MINUTES.iter().find(|(k, _)| *k == window).map(|(_, m)| *m).unwrap_or(0);
                Some(QuotaSample {
                    used_percent: (1.0 - remaining) * 100.0,
                    window_minutes: minutes,
                    resets_at_ms: bucket.get("reset_time").and_then(Value::as_str).and_then(parse_ts_ms).unwrap_or(0),
                    label: Some(match bucket.get("name").and_then(Value::as_str) {
                        Some(n) if !n.is_empty() => format!("{} · {}", group_label(name), short(n, window)),
                        _ => format!("{} · {window}", group_label(name)),
                    }),
                    id: None,
                })
            })
            .collect();
        // The vendor groups by model and answers with a 5-hour and a weekly
        // window per group; keep the groups in the vendor's own order and the
        // windows shortest-first inside a group, so the panel can show one
        // model's two windows together instead of interleaving the models.
        rows.sort_by_key(|s| s.window_minutes);
        out.extend(rows);
    }
    out
}

/// The vendor names its buckets "Weekly Limit Remaining" — a *remaining* share —
/// while this bar shows the used share, so "剩余" would read backwards. Keep the
/// vendor's group name (it is what the IDE shows) and label the window only.
fn short(name: &str, window: &str) -> String {
    match window {
        "weekly" => "周",
        "5h" => "5 小时",
        _ => return name.to_string(),
    }
    .to_string()
}

/// The CLI's group names are sentences ("Claude and GPT models"); the panel has
/// ~130 px for the whole label, so groups get their shortest honest form.
/// Named `group_label` because the loop below binds `group`.
fn group_label(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("gemini") {
        "Gemini".to_string()
    } else if lower.contains("claude") && lower.contains("gpt") {
        "Claude/GPT".to_string()
    } else {
        name.split_whitespace().next().unwrap_or(name).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: &str) -> Value {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("fixture parses: {e}"))
    }

    /// Trimmed copy of what `agy -p /usage --output-format json` answers on this
    /// machine, field names and all.
    const LIVE: &str = r#"{
      "status":"SUCCESS","num_turns":0,
      "usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0},
      "command":{"name":"usage","data":{"description":"Within each group...","groups":[
        {"name":"Gemini Models","buckets":[
          {"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.9499499797821045,"reset_time":"2026-09-30T03:34:13Z"},
          {"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":0.7587900161743164,"reset_time":"2026-09-23T20:16:54Z"}]},
        {"name":"Claude and GPT models","buckets":[
          {"id":"other-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":1.0,"reset_time":"2026-09-30T16:12:03Z"},
          {"id":"other-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":1.0,"reset_time":"2026-09-23T21:12:03Z"}]}
      ]}}}"#;

    #[test]
    fn a_grouped_answer_becomes_one_bar_per_window() {
        let s = samples_from(&body(LIVE));
        assert_eq!(s.len(), 4);
        assert_eq!(s[0].window_minutes, 300, "5-hour windows sort first");
        assert!((s[0].used_percent - 24.12).abs() < 0.01, "1 - 0.7588: {:?}", s[0]);
        assert_eq!(s[0].label.as_deref(), Some("Gemini · 5 小时"));
        // The vendor's own group order — Gemini first — and each model's two
        // windows adjacent, because the panel groups on this arrangement.
        assert_eq!(s[1].label.as_deref(), Some("Gemini · 周"));
        assert_eq!(s[1].window_minutes, 10_080);
        assert!((s[1].used_percent - 5.005).abs() < 0.01, "{:?}", s[1]);
        assert_eq!(s[2].label.as_deref(), Some("Claude/GPT · 5 小时"));
        assert!(s.iter().all(|x| x.resets_at_ms > 1_790_000_000_000), "{s:?}");
        assert!(s.iter().all(|x| x.used_percent >= 0.0 && x.used_percent <= 100.0));
    }

    #[test]
    fn an_unused_bucket_is_still_a_zero_bar() {
        let s = samples_from(&body(LIVE));
        let untouched = s.iter().filter(|x| (x.used_percent - 0.0).abs() < 1e-9).count();
        assert_eq!(untouched, 2, "the Claude and GPT group is untouched");
    }

    #[test]
    fn missing_impossible_and_disabled_buckets_are_dropped_not_invented() {
        let s = samples_from(&body(
            r#"{"status":"SUCCESS","command":{"data":{"groups":[{"name":"G","buckets":[
                 {"id":"a","window":"weekly","remaining_fraction":0.5},
                 {"id":"b","window":"5h","remaining_fraction":3.5},
                 {"id":"c","window":"5h","remaining_fraction":0.2,"disabled":true},
                 {"id":"d","window":"5h"},
                 {"id":"e","window":"weird","name":"Odd","remaining_fraction":0.25} ]}]}}}"#,
        ));
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[0].used_percent, 75.0, "an unknown window sorts first and stays unlabelled");
        assert_eq!(s[0].window_minutes, 0);
        assert_eq!(s[0].label.as_deref(), Some("G · Odd"), "the vendor's own bucket name is all we have");
        assert_eq!(s[1].used_percent, 50.0);
        assert_eq!(s[1].window_minutes, 10_080);
        assert_eq!(s[1].label.as_deref(), Some("G · weekly"), "a bucket with no name shows the raw window");
        assert_eq!(s[1].resets_at_ms, 0, "a missing reset_time is unknown, not now");
    }

    #[test]
    fn anything_other_than_success_is_nothing() {
        for raw in [
            r#"{"status":"ERROR","command":{"data":{"groups":[]}}}"#,
            r#"{"status":"SUCCESS"}"#,
            r#"{"status":"SUCCESS","command":{"data":{"groups":null}}}"#,
            "not json at all",
        ] {
            let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            assert!(samples_from(&v).is_empty(), "{raw} must answer nothing");
        }
    }

    #[test]
    #[ignore = "runs the real Antigravity CLI"]
    fn the_live_cli_answers_with_real_windows() {
        let Some(bin) = binary() else { println!("[antigravity] no agy binary here"); return };
        println!("[antigravity] {bin:?}");
        let version = Command::new(&bin).args(["--version"]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        println!("[antigravity] version = {version}");
        let body = run_usage().expect("the CLI answers within the budget");
        let value: Value = serde_json::from_str(&body).unwrap();
        println!("[antigravity] response text = {:?}", value.get("response").and_then(Value::as_str).unwrap_or_default().replace('\n', " / "));
        let s = samples_from(&value);
        assert!(!s.is_empty(), "an account always has at least one window: {body}");
        for q in &s {
            println!("[antigravity] {:>6} {:.1}%  {}", q.label.as_deref().unwrap_or_default(), q.used_percent, q.resets_at_ms);
        }
    }
}
