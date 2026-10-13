//! User-triggered, one-shot speed checks for supported local CLIs.
//!
//! These checks send one fixed, short prompt only after an explicit click.
//! Codex uses an ephemeral read-only app-server thread; Claude Code receives
//! per-process OTLP settings and exports to a temporary loopback receiver. No
//! prompt, response text, credentials, or active result is persisted.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::claude_otel_ttft::{self, MetricField};

const PROMPT: &str = "For a short speed check, reply with exactly this word and nothing else: READY";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const EXPORT_GRACE: Duration = Duration::from_secs(10);
const USAGE_GRACE: Duration = Duration::from_millis(750);

static ACTIVE_TEST: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSpeedTest {
    pub provider: String,
    pub model: Option<String>,
    pub ttft_ms: f64,
    pub tps: Option<f64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: Option<f64>,
    pub sampled_at_ms: i64,
    /// `codex_stream` is measured from the app-server turn request to the
    /// first text delta. `claude_otel` is the CLI's request-level span.
    pub measurement: String,
}

#[tauri::command]
pub async fn run_active_speed_test(provider: String) -> Result<ActiveSpeedTest, String> {
    let provider = provider.trim().to_ascii_lowercase();
    if !matches!(provider.as_str(), "codex" | "claude_code") {
        return Err("Active speed checks are currently available for Codex and Claude Code.".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let gate = ACTIVE_TEST.get_or_init(|| Mutex::new(()));
        let _guard = gate.try_lock().map_err(|_| "A speed check is already running.".to_string())?;
        crate::logging::info(&format!("active speed: starting {provider}"));
        let result = match provider.as_str() {
            "codex" => run_codex(),
            "claude_code" => run_claude(),
            _ => unreachable!(),
        };
        match &result {
            Ok(sample) => crate::logging::info(&format!(
                "active speed: {provider} completed ttft={:.0}ms model={}",
                sample.ttft_ms,
                sample.model.as_deref().unwrap_or("unknown")
            )),
            Err(error) => crate::logging::error(&format!("active speed: {provider} failed: {error}")),
        }
        result
    }).await.map_err(|_| "The speed check worker stopped unexpectedly.".to_string())?
}

fn run_codex() -> Result<ActiveSpeedTest, String> {
    let executable = resolve_executable("codex.exe", "codex")
        .ok_or_else(|| "Codex CLI was not found on PATH.".to_string())?;
    let mut command = Command::new(executable);
    command
        .args(["app-server", "--listen", "stdio://"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn()
        .map_err(|_| "Could not start Codex app-server.".to_string())?;

    let result = run_codex_protocol(&mut child);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn run_codex_protocol(child: &mut Child) -> Result<ActiveSpeedTest, String> {
    let stdin = child.stdin.take().ok_or_else(|| "Codex app-server stdin is unavailable.".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "Codex app-server stdout is unavailable.".to_string())?;
    let (tx, rx) = mpsc::channel::<String>();
    thread::Builder::new().name("codex-speed-reader".into()).spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => if tx.send(line).is_err() { break },
                Err(_) => break,
            }
        }
    }).map_err(|_| "Could not start Codex app-server reader.".to_string())?;
    let mut input = stdin;

    send_rpc(&mut input, json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "TokenMe", "title": "TokenMe speed check", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true }
        }
    }))?;
    let init = wait_response(&rx, child, 1, Duration::from_secs(15))?;
    if init.get("error").is_some() { return Err("Codex app-server initialization failed.".into()); }
    send_rpc(&mut input, json!({ "method": "initialized", "params": {} }))?;

    send_rpc(&mut input, json!({
        "id": 2,
        "method": "thread/start",
        "params": {
            "cwd": std::env::temp_dir().to_string_lossy(),
            "ephemeral": true,
            "sandbox": "read-only",
            "approvalPolicy": "never",
            "baseInstructions": "This is a short speed check. Do not use tools. Reply only with the requested word."
        }
    }))?;
    let thread_response = wait_response(&rx, child, 2, Duration::from_secs(20))?;
    if thread_response.get("error").is_some() { return Err("Codex could not create the temporary speed-check thread.".into()); }
    let thread_result = thread_response.get("result").ok_or_else(|| "Codex returned no thread result.".to_string())?;
    let thread_id = thread_result.get("thread").and_then(|v| v.get("id")).and_then(Value::as_str)
        .ok_or_else(|| "Codex returned no thread id.".to_string())?.to_string();
    let model = thread_result.get("model").and_then(Value::as_str).map(str::to_string);

    let started = Instant::now();
    send_rpc(&mut input, json!({
        "id": 3,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{ "type": "text", "text": PROMPT }],
            "approvalPolicy": "never",
            "sandboxPolicy": { "type": "readOnly", "networkAccess": false },
            "effort": "low"
        }
    }))?;
    let mut first_delta: Option<Duration> = None;
    let mut turn_id: Option<String> = None;
    let mut output_tokens = None;
    let mut turn_duration_ms = None;
    let mut turn_completed = false;
    let mut usage_deadline: Option<Instant> = None;
    loop {
        let message = if let Some(deadline) = usage_deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { break; }
            match rx.recv_timeout(remaining) {
                Ok(line) => serde_json::from_str(&line).map_err(|_| "Codex app-server sent an invalid protocol message.".to_string())?,
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            receive_message(&rx, child, started)?
        };
        if message.get("id").and_then(Value::as_i64) == Some(3) {
            if message.get("error").is_some() { return Err("Codex rejected the speed-check request.".into()); }
            turn_id = message.pointer("/result/turn/id").and_then(Value::as_str).map(str::to_string);
            continue;
        }
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = message.get("params").unwrap_or(&Value::Null);
        match method {
            "item/agentMessage/delta" => {
                if params.get("delta").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
                    && first_delta.is_none()
                { first_delta = Some(started.elapsed()); }
            }
            "item/started" | "item/updated" => {
                let item_type = params.get("item").and_then(|v| v.get("type")).and_then(Value::as_str).unwrap_or_default();
                // "userMessage" opens every turn — it is the input's own echo,
                // not a tool call, and aborting on it stopped every healthy
                // run before the first token could arrive.
                if !item_type.is_empty() && !matches!(item_type, "agentMessage" | "reasoning" | "userMessage") {
                    return Err(format!("Codex started a {item_type} item during the read-only speed check; the run was stopped."));
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(tokens) = token_usage_output(params, thread_id.as_str(), turn_id.as_deref()) {
                    output_tokens = Some(tokens);
                    if turn_completed && output_tokens.is_some() { break; }
                }
            }
            "turn/completed" => {
                let turn = params.get("turn").unwrap_or(&Value::Null);
                turn_completed = true;
                turn_duration_ms = turn.get("durationMs").and_then(|value| value.as_f64().or_else(|| value.as_i64().map(|n| n as f64)))
                    .filter(|value| value.is_finite() && *value >= 0.0);
                // Older app-server versions exposed usage on the completed turn;
                // keep that fallback while preferring the current notification.
                if output_tokens.is_none() {
                    output_tokens = numeric(turn.pointer("/usage/outputTokens").or_else(|| turn.pointer("/usage/output_tokens")));
                }
                if output_tokens.is_some() { break; }
                // The usage notification is independent from turn/completed and
                // may arrive just after it. Give it a short bounded grace period
                // so a missing notification never turns a finished test into a
                // 90-second wait.
                usage_deadline = Some(Instant::now() + USAGE_GRACE);
            }
            "turn/failed" => return Err("The Codex speed-check turn failed.".into()),
            _ => {}
        }
    };
    if !turn_completed { return Err("Codex completed without a finished speed-check turn.".into()); }
    let ttft = first_delta.ok_or_else(|| "Codex completed without a streamed text delta; no TTFT sample was recorded.".to_string())?;
    let ttft_ms = ttft.as_secs_f64() * 1000.0;
    let duration_ms = turn_duration_ms.unwrap_or_else(|| started.elapsed().as_secs_f64() * 1000.0);
    let decode_ms = (duration_ms - ttft_ms).max(0.0);
    let tps = output_tokens.filter(|tokens| *tokens > 0 && decode_ms > 0.0)
        .map(|tokens| tokens as f64 / (decode_ms / 1000.0));
    Ok(ActiveSpeedTest {
        provider: "codex".into(), model, ttft_ms,
        tps, output_tokens, duration_ms: Some(duration_ms),
        sampled_at_ms: now_ms(), measurement: "codex_stream".into(),
    })
}

fn wait_response(rx: &mpsc::Receiver<String>, child: &mut Child, id: i64, timeout: Duration) -> Result<Value, String> {
    let started = Instant::now();
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() { return Err("Codex app-server timed out during startup.".into()); }
        let line = rx.recv_timeout(remaining).map_err(|_| {
            if child.try_wait().ok().flatten().is_some() { "Codex app-server exited during startup.".to_string() }
            else { "Codex app-server timed out during startup.".to_string() }
        })?;
        let message: Value = serde_json::from_str(&line).map_err(|_| "Codex app-server sent an invalid protocol message.".to_string())?;
        if message.get("id").and_then(Value::as_i64) == Some(id) { return Ok(message); }
    }
}

fn receive_message(rx: &mpsc::Receiver<String>, child: &mut Child, started: Instant) -> Result<Value, String> {
    let remaining = REQUEST_TIMEOUT.saturating_sub(started.elapsed());
    if remaining.is_zero() { return Err("The Codex speed check timed out.".into()); }
    let line = rx.recv_timeout(remaining).map_err(|_| {
        if child.try_wait().ok().flatten().is_some() { "Codex app-server exited before the turn completed.".to_string() }
        else { "The Codex speed check timed out.".to_string() }
    })?;
    serde_json::from_str(&line).map_err(|_| "Codex app-server sent an invalid protocol message.".into())
}

fn send_rpc(input: &mut impl Write, message: Value) -> Result<(), String> {
    serde_json::to_writer(&mut *input, &message).map_err(|_| "Could not encode the Codex request.".to_string())?;
    input.write_all(b"\n").and_then(|()| input.flush()).map_err(|_| "Could not write to Codex app-server.".to_string())
}

fn run_claude() -> Result<ActiveSpeedTest, String> {
    let executable = resolve_claude_executable().ok_or_else(|| "Claude Code CLI was not found on PATH.".to_string())?;
    let (tx, rx) = mpsc::channel::<Vec<claude_otel_ttft::TtftSample>>();
    let server = claude_otel_ttft::start_server(0, move |samples| { let _ = tx.send(samples); })
        .map_err(|_| "Could not start the temporary Claude telemetry receiver.".to_string())?;
    let endpoint = format!("http://{}/v1/traces", server.local_addr());
    // The process environment cannot carry this redirect: without
    // --restricted the CLI applies ~/.claude/settings.json's env block OVER
    // the inherited environment, and custom-API setups keep an OTEL endpoint
    // of their own in there — the samples never reach this ephemeral
    // receiver. A --settings file outranks the user's settings, so the whole
    // telemetry block rides in there instead.
    let settings_path = std::env::temp_dir()
        .join(format!("tokenme-claude-speed-{}.json", std::process::id()));
    std::fs::write(&settings_path, json!({ "env": {
        "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
        "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA": "1",
        "OTEL_TRACES_EXPORTER": "otlp",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL": "http/json",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT": endpoint,
        "OTEL_TRACES_EXPORT_INTERVAL": "1000",
    }}).to_string())
        .map_err(|_| "Could not stage the Claude telemetry settings.".to_string())?;
    let mut command = Command::new(executable);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    // `--tools` is variadic: written as two args it swallows the empty value
    // AND the prompt after it, and the CLI exits "Input must be provided".
    // The joined form closes the list; `--` pins the prompt as positional.
    // No `--restricted`: it skips ~/.claude/settings.json's env block, which
    // is exactly where custom-API setups live (ANTHROPIC_BASE_URL /
    // ANTHROPIC_AUTH_TOKEN) — restricted runs fell back to OAuth and died on
    // an identity this machine may not even use. Tools are already off via
    // --tools=, and the turn is capped at one.
    command
        .args(["-p", "--output-format", "json", "--max-turns", "1", "--no-session-persistence", "--tools="])
        .arg("--settings").arg(&settings_path)
        .arg("--").arg(PROMPT)
        .current_dir(std::env::temp_dir())
        .env("CLAUDE_CODE_ENABLE_TELEMETRY", "1")
        .env("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1")
        .env("OTEL_TRACES_EXPORTER", "otlp")
        .env("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json")
        .env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", endpoint)
        .env("OTEL_TRACES_EXPORT_INTERVAL", "1000")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = command.spawn();
    let result = match child {
        Ok(mut child) => run_claude_child(&mut child, &rx),
        Err(_) => Err("Could not start Claude Code CLI.".into()),
    };
    server.shutdown();
    let _ = std::fs::remove_file(&settings_path);
    result
}

fn cli_speed_sample(output: &[u8]) -> Option<ActiveSpeedTest> {
    let value = serde_json::from_slice::<Value>(output).ok()?;
    let ttft_ms = value.get("ttft_stream_ms").and_then(Value::as_f64).filter(|value| value.is_finite() && *value > 0.0)?;
    let duration_ms = value.get("duration_ms").and_then(Value::as_f64).filter(|value| value.is_finite() && *value >= ttft_ms);
    let output_tokens = value.pointer("/usage/output_tokens").and_then(|value| numeric(Some(value)));
    let tps = match (output_tokens, duration_ms) {
        (Some(tokens), Some(duration)) if duration > ttft_ms => Some(tokens as f64 * 1000.0 / (duration - ttft_ms)),
        _ => None,
    };
    let model = value.get("model").and_then(Value::as_str).map(str::to_string)
        .or_else(|| value.get("modelUsage").and_then(Value::as_object).and_then(|models| models.keys().next().cloned()));
    Some(ActiveSpeedTest {
        provider: "claude_code".into(), model, ttft_ms, tps, output_tokens, duration_ms,
        sampled_at_ms: now_ms(), measurement: "claude_cli".into(),
    })
}

fn run_claude_child(child: &mut Child, rx: &mpsc::Receiver<Vec<claude_otel_ttft::TtftSample>>) -> Result<ActiveSpeedTest, String> {
    let stdout = child.stdout.take();
    let output_reader = thread::spawn(move || {
        let mut output = Vec::new();
        if let Some(mut stdout) = stdout { let _ = stdout.read_to_end(&mut output); }
        output
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => { let _ = child.kill(); let _ = child.wait(); let _ = output_reader.join(); return Err("Could not read Claude Code process status.".into()); }
        }
        if started.elapsed() >= REQUEST_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = output_reader.join();
            return Err("The Claude Code speed check timed out.".into());
        }
        thread::sleep(Duration::from_millis(40));
    };
    let output = output_reader.join().unwrap_or_default();
    if !status.success() { return Err("Claude Code exited before exporting a speed sample.".into()); }

    let export_deadline = Instant::now() + EXPORT_GRACE;
    let mut sample = None;
    while Instant::now() < export_deadline {
        let remaining = export_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(samples) => {
                sample = samples.into_iter().find(|s| s.field == MetricField::TtftMs).or(sample);
                if sample.is_some() { break; }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(sample) = sample {
        return Ok(ActiveSpeedTest {
            provider: "claude_code".into(), model: sample.model, ttft_ms: sample.millis,
            tps: sample.tps, output_tokens: sample.output_tokens, duration_ms: sample.duration_ms,
            sampled_at_ms: sample.sampled_at_ms.min(i64::MAX as u64) as i64,
            measurement: "claude_otel".into(),
        });
    }
    cli_speed_sample(&output).ok_or_else(|| "Claude Code did not export request TTFT. Check that this CLI version supports enhanced telemetry.".to_string())
}

fn numeric(value: Option<&Value>) -> Option<u64> {
    value.and_then(|v| v.as_u64().or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok())))
}

fn token_usage_output(params: &Value, thread_id: &str, turn_id: Option<&str>) -> Option<u64> {
    if params.get("threadId").and_then(Value::as_str) != Some(thread_id) { return None; }
    if let Some(expected) = turn_id {
        if params.get("turnId").and_then(Value::as_str) != Some(expected) { return None; }
    }
    numeric(params.pointer("/tokenUsage/last/outputTokens")
        .or_else(|| params.pointer("/tokenUsage/total/outputTokens")))
}

fn resolve_executable(executable: &str, fallback: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(executable);
        if candidate.is_file() { return Some(candidate); }
        #[cfg(not(target_os = "windows"))]
        {
            let candidate = directory.join(fallback);
            if candidate.is_file() { return Some(candidate); }
        }
    }
    Some(std::path::PathBuf::from(fallback))
}

fn resolve_claude_executable() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        for name in ["claude", "claude.exe"] {
            let candidate = directory.join(name);
            if candidate.is_file() { return Some(candidate); }
            let candidate = directory.join("node_modules").join("@anthropic-ai").join("claude-code").join("bin").join(name);
            if candidate.is_file() { return Some(candidate); }
        }
    }
    Some(std::path::PathBuf::from(if cfg!(windows) { "claude.exe" } else { "claude" }))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_rate_uses_generation_window_after_first_text() {
        let output_tokens = Some(120);
        let ttft = Duration::from_millis(500);
        let duration = Duration::from_millis(2500);
        let tps = output_tokens.map(|tokens| tokens as f64 / duration.saturating_sub(ttft).as_secs_f64());
        assert_eq!(tps, Some(60.0));
    }

    #[test]
    fn token_usage_notification_provides_output_tokens_for_the_active_turn() {
        let params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "tokenUsage": {
                "last": { "outputTokens": 7 },
                "total": { "outputTokens": 7 }
            }
        });
        assert_eq!(token_usage_output(&params, "thread-1", Some("turn-1")), Some(7));
        assert_eq!(token_usage_output(&params, "thread-1", Some("other-turn")), None);
        assert_eq!(token_usage_output(&params, "other-thread", None), None);
    }

    #[test]
    fn only_supported_providers_are_admitted() {
        assert!(matches!("codex", "codex" | "claude_code"));
        assert!(matches!("claude_code", "codex" | "claude_code"));
        assert!(!matches!("dsh", "codex" | "claude_code"));
    }

    // Live checks against the real CLIs: one short API turn each, and they
    // need the tools logged in on this machine, so they stay behind #[ignore]
    // — `cargo test --lib live_ -- --ignored` runs them by hand.
    #[test]
    #[ignore]
    fn live_codex_speed_check_returns_a_sample() {
        let sample = run_codex().expect("codex live speed check");
        assert!(sample.ttft_ms > 0.0, "ttft must be measured, got {:?}", sample.ttft_ms);
        assert_eq!(sample.measurement, "codex_stream");
    }

    #[test]
    #[ignore]
    fn live_claude_speed_check_returns_a_sample() {
        let sample = run_claude().expect("claude live speed check");
        assert!(sample.ttft_ms > 0.0, "ttft must be measured, got {:?}", sample.ttft_ms);
        assert_eq!(sample.measurement, "claude_otel");
    }

    #[test]
    fn claude_fallback_matches_the_host_executable_name() {
        let path = resolve_claude_executable().unwrap();
        if std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| {
            ["claude", "claude.exe"].iter().any(|name| dir.join(name).is_file())
        })) {
            assert!(path.file_name().is_some_and(|name| name == "claude" || name == "claude.exe"));
        } else {
            assert_eq!(path, std::path::PathBuf::from(if cfg!(windows) { "claude.exe" } else { "claude" }));
        }
    }

    #[test]
    fn cli_result_fallback_uses_stream_ttft() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.json");
        std::fs::write(&path, br#"{"ttft_ms":23024,"ttft_stream_ms":1835,"duration_ms":23112,"usage":{"output_tokens":851},"modelUsage":{"claude-opus-5":{}}}"#).unwrap();
        let sample = cli_speed_sample(&std::fs::read(&path).unwrap()).expect("CLI speed result");
        assert_eq!(sample.measurement, "claude_cli");
        assert_eq!(sample.ttft_ms, 1835.0);
        assert_eq!(sample.model.as_deref(), Some("claude-opus-5"));
        assert!(sample.tps.is_some_and(|tps| tps > 39.0 && tps < 40.0));
    }
}
