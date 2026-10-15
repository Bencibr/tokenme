//! Subcommand implementations. Every number rendered here comes from
//! `usage_core::report::summarize`; this file only slices events and formats.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::thread;

use chrono::{Datelike, Local, TimeZone};
use base64::Engine as _;
use serde::Deserialize;
use usage_core::{summarize, CallKind, DateFilter, Item, Meter, Summary, UsageEvent, Window};
use usage_device_sync::{
    hash_bytes, identity_bytes, DeviceIdentity, DiagnosticsGrant, DiagnosticsMcpRequest, DiagnosticsRequestAuth,
    DiagnosticsService, HttpsServerConfig, HttpsTransportConfig, NetworkCall, NetworkCounts,
    NetworkPackage, NetworkRow, PairingInvite, PairingRequest, SignedUpload,
    SyncRuntime, SyncRuntimeConfig, SyncStore, Tls13HttpsTransport,
};
use usage_index::{retention_cutoff, IngestReport};

use crate::args::{Group, Win};
use crate::context::{print_json, Ctx};
use crate::render::{self, cost_cell, Table};

const MS_DAY: i64 = 86_400_000;
pub const MAX_PERIODS: i64 = 90;

/// Serve the diagnostics MCP protocol from the current sync database.
///
/// The protocol owns authentication and grants; the CLI host only opens the
/// local SyncStore and hands its stdin/stdout to the bounded stdio bridge.
/// Keeping this command outside `context::build` is intentional: it must not
/// ingest logs, print progress, or put anything except JSON-RPC responses on
/// stdout. The debugger identity is carried by each signed MCP request; the
/// service authority is intentionally process-scoped, so a restarted command
/// cannot accept grants issued by the previous process.
pub fn diagnostics_stdio(global: &crate::args::Global) -> Result<(), String> {
    let db_path = global
        .db
        .clone()
        .or_else(usage_index::Index::default_path)
        .ok_or_else(|| "diagnostics stdio needs --db <path> on this platform".to_string())?;
    if !db_path.exists() {
        return Err(format!("diagnostics stdio requires an existing sync database: {}", db_path.display()));
    }
    let status = SyncStore::read_status(&db_path).map_err(|error| format!("cannot read sync identity from {}: {error}", db_path.display()))?;
    let primary_id = if let Some(primary_id) = status.primary_id {
        primary_id
    } else {
        SyncStore::read_secondary_config(&db_path)
            .map_err(|error| format!("cannot read secondary identity from {}: {error}", db_path.display()))?
            .map(|config| config.primary_id)
            .ok_or_else(|| format!("no device-sync identity is stored in {}; pair or start sync first", db_path.display()))?
    };
    if let Some(config_path) = &global.diagnostics_config {
        return diagnostics_bridge_stdio(&db_path, &primary_id, config_path);
    }
    let store = SyncStore::open(&db_path, primary_id).map_err(|error| format!("cannot open sync store {}: {error}", db_path.display()))?;
    let service = DiagnosticsService::new(store.primary_id()).map_err(|error| format!("cannot start diagnostics service: {error}"))?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    service.serve_stdio(&store, &mut reader, &mut writer).map_err(|error| format!("diagnostics stdio failed: {error}"))
}

#[derive(Debug, Deserialize)]
struct DiagnosticsBridgeConfig {
    schema: String,
    endpoint: String,
    #[serde(default, alias = "serverName")]
    server_name: Option<String>,
    certificate_pin: String,
    certificate_der_b64: String,
    grant: DiagnosticsGrant,
}

fn diagnostics_bridge_stdio(db_path: &std::path::Path, primary_id: &str, config_path: &std::path::Path) -> Result<(), String> {
    let config = load_diagnostics_bridge_config(config_path)?;
    if config.schema != "tokenme-diagnostics-grant/v1" {
        return Err("diagnostics config schema is unsupported".into());
    }
    if config.grant.target_device_id != primary_id {
        return Err("diagnostics grant target does not match the --db primary identity".into());
    }
    let secondary = SyncStore::read_secondary_config(db_path)
        .map_err(|error| format!("cannot read debugger identity from {}: {error}", db_path.display()))?
        .ok_or_else(|| "diagnostics bridge requires a paired secondary identity in --db".to_string())?;
    if secondary.device_id != config.grant.debugger_device_id || secondary.origin != config.grant.debugger_origin {
        return Err("diagnostics grant debugger identity does not match the paired --db identity".into());
    }
    if !secondary.identity_path.exists() {
        return Err(format!("diagnostics debugger identity is missing: {}", secondary.identity_path.display()));
    }
    let identity = DeviceIdentity::load_or_create(&secondary.identity_path, secondary.origin.clone(), "tokenme diagnostics bridge")
        .map_err(|error| format!("cannot load diagnostics debugger identity: {error}"))?;
    if identity.device_id != config.grant.debugger_device_id || identity.origin != config.grant.debugger_origin {
        return Err("diagnostics identity file does not match the grant".into());
    }
    let server_name = config.server_name.clone().filter(|name| !name.trim().is_empty()).unwrap_or(endpoint_server_name(&config.endpoint)?);
    let mut transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(config.endpoint.clone(), server_name, config.certificate_der_b64_bytes()?))
        .map_err(|error| format!("cannot configure diagnostics TLS transport: {error}"))?;
    let pin = parse_sha256(&config.certificate_pin)?;
    if transport.config().pinned_certificate_sha256 != pin {
        return Err("diagnostics certificate pin does not match certificate_der_b64".into());
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut session_id = None;
    let mut sequence = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).map_err(|error| format!("diagnostics stdio read failed: {error}"))?;
        if read == 0 { return Ok(()); }
        if line.len() > usage_device_sync::MAX_DIAGNOSTICS_BODY_BYTES {
            let id = serde_json::from_str::<serde_json::Value>(line.trim_end()).ok().and_then(|value| json_rpc_id(&value));
            eprintln!("tokenme diagnostics: request exceeds the configured size limit");
            write_diagnostics_bridge_error(&mut writer, id, -32600, "limit_exceeded", "diagnostics request exceeds the configured size limit")?;
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line.trim_end()) {
            Ok(value) => value,
            Err(_) => {
                eprintln!("tokenme diagnostics: invalid JSON-RPC request");
                write_diagnostics_bridge_error(&mut writer, None, -32700, "parse_error", "invalid JSON-RPC request")?;
                continue;
            }
        };
        let id = json_rpc_id(&value);
        sequence = sequence.saturating_add(1);
        let request = match build_diagnostics_bridge_request(&config.grant, &identity, session_id.clone(), value, sequence) {
            Ok(request) => request,
            Err(error) => {
                let message = bounded_diagnostics_error(&error);
                eprintln!("tokenme diagnostics: {message}");
                write_diagnostics_bridge_error(&mut writer, id, -32600, "invalid_request", &message)?;
                continue;
            }
        };
        let method = request.method.clone();
        let request_id = request.id.clone();
        match transport.diagnostics_mcp(&request) {
            Ok(Some(response)) => {
                if method == "initialize" {
                    session_id = response.result.as_ref().and_then(|result| result.get("sessionId")).and_then(serde_json::Value::as_str).map(str::to_owned);
                }
                serde_json::to_writer(&mut writer, &response).map_err(|error| format!("diagnostics stdio write failed: {error}"))?;
                writer.write_all(b"\n").map_err(|error| format!("diagnostics stdio write failed: {error}"))?;
                writer.flush().map_err(|error| format!("diagnostics stdio flush failed: {error}"))?;
            }
            Ok(None) => {
                if request_id.is_some() {
                    eprintln!("tokenme diagnostics: remote diagnostics returned no response for a request");
                    write_diagnostics_bridge_error(&mut writer, request_id, -32099, "transport_error", "remote diagnostics returned no response")?;
                }
            }
            Err(error) => {
                let detail = bounded_diagnostics_error(&error.to_string());
                eprintln!("tokenme diagnostics: {detail}");
                if request_id.is_some() {
                    write_diagnostics_bridge_error(&mut writer, request_id, -32099, "transport_error", "remote diagnostics transport failed")?;
                }
            }
        }
    }
}

impl DiagnosticsBridgeConfig {
    fn certificate_der_b64_bytes(&self) -> Result<Vec<u8>, String> {
        base64::engine::general_purpose::STANDARD.decode(&self.certificate_der_b64).map_err(|_| "diagnostics certificate_der_b64 is invalid".to_string())
    }
}

fn load_diagnostics_bridge_config(path: &std::path::Path) -> Result<DiagnosticsBridgeConfig, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("cannot read diagnostics config {}: {error}", path.display()))?;
    if bytes.len() > usage_device_sync::MAX_DIAGNOSTICS_BODY_BYTES { return Err("diagnostics config exceeds the configured size limit".into()); }
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid diagnostics config: {error}"))
}

fn endpoint_server_name(endpoint: &str) -> Result<String, String> {
    let authority = endpoint.strip_prefix("https://").ok_or_else(|| "diagnostics endpoint must use https://".to_string())?;
    let authority = authority.split('/').next().unwrap_or_default();
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        authority.rsplit_once(':').map(|(host, _)| host).unwrap_or(authority)
    };
    if host.is_empty() { return Err("diagnostics endpoint has no server name".into()); }
    Ok(host.to_string())
}

fn build_diagnostics_bridge_request(
    grant: &DiagnosticsGrant,
    identity: &DeviceIdentity,
    session_id: Option<String>,
    value: serde_json::Value,
    sequence: u64,
) -> Result<DiagnosticsMcpRequest, String> {
    let object = value.as_object().ok_or_else(|| "JSON-RPC request must be an object".to_string())?;
    let jsonrpc = object.get("jsonrpc").and_then(serde_json::Value::as_str).ok_or_else(|| "JSON-RPC version is missing".to_string())?;
    if jsonrpc != "2.0" { return Err("JSON-RPC version must be 2.0".into()); }
    let method = object.get("method").and_then(serde_json::Value::as_str).ok_or_else(|| "JSON-RPC method is missing".to_string())?;
    let id = object.get("id").cloned();
    if id.as_ref().is_some_and(|value| !matches!(value, serde_json::Value::Null | serde_json::Value::String(_) | serde_json::Value::Number(_))) {
        return Err("JSON-RPC id must be a string, number, or null".into());
    }
    if method != "initialize" && session_id.is_none() {
        return Err("MCP session is not initialized; send initialize first".into());
    }
    let params = object.get("params").cloned().unwrap_or_else(|| serde_json::json!({}));
    let request_id = id.as_ref().and_then(|value| match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }).filter(|value| !value.is_empty()).unwrap_or_else(|| format!("tokenme-stdio-{sequence}"));
    let now_ms = usage_core::report::now_ms();
    let expires_at_ms = now_ms.saturating_add(usage_device_sync::MAX_DIAGNOSTICS_REQUEST_TTL_MS.min(5_000));
    let auth = DiagnosticsRequestAuth::new_signed(grant, identity, session_id, method, &params, request_id, now_ms, expires_at_ms)
        .map_err(|error| format!("cannot sign diagnostics request: {error}"))?;
    Ok(DiagnosticsMcpRequest { jsonrpc: jsonrpc.into(), id, method: method.into(), params, auth })
}

fn json_rpc_id(value: &serde_json::Value) -> Option<serde_json::Value> {
    let id = value.as_object()?.get("id")?;
    if matches!(id, serde_json::Value::Null | serde_json::Value::String(_) | serde_json::Value::Number(_)) { Some(id.clone()) } else { None }
}

fn write_diagnostics_bridge_error<W: Write>(writer: &mut W, id: Option<serde_json::Value>, code: i32, error_code: &str, message: &str) -> Result<(), String> {
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(serde_json::Value::Null),
        "error": {
            "code": code,
            "message": bounded_diagnostics_error(message),
            "data": { "errorCode": error_code },
        },
    });
    serde_json::to_writer(&mut *writer, &response).map_err(|error| format!("diagnostics stdio write failed: {error}"))?;
    writer.write_all(b"\n").map_err(|error| format!("diagnostics stdio write failed: {error}"))?;
    writer.flush().map_err(|error| format!("diagnostics stdio flush failed: {error}"))
}

fn bounded_diagnostics_error(error: &str) -> String {
    let mut value = error.replace(['\r', '\n'], " ");
    if value.len() > 512 { value.truncate(512); value.push('…'); }
    value
}

#[cfg(test)]
mod diagnostics_bridge_tests {
    use super::*;
    use usage_device_sync::DiagnosticsScope;

    #[test]
    fn diagnostics_config_deserializes_and_request_auth_is_signed() {
        let identity = DeviceIdentity::create("debugger-origin", "Debugger").unwrap();
        let grant = DiagnosticsGrant {
            grant_id: "dgrant-test".into(),
            target_device_id: "primary-test".into(),
            debugger_device_id: identity.device_id.clone(),
            debugger_origin: identity.origin.clone(),
            scopes: vec![DiagnosticsScope::Read],
            issued_at_ms: 1_000,
            expires_at_ms: 20_000,
            signature: [0; 64],
        };
        let encoded_signature = base64::engine::general_purpose::STANDARD.encode([0u8; 64]);
        let config: DiagnosticsBridgeConfig = serde_json::from_value(serde_json::json!({
            "schema": "tokenme-diagnostics-grant/v1",
            "endpoint": "https://localhost:47821",
            "server_name": "diagnostics.local",
            "certificate_pin": "00".repeat(32),
            "certificate_der_b64": base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]),
            "grant": {
                "grant_id": grant.grant_id,
                "target_device_id": grant.target_device_id,
                "debugger_device_id": grant.debugger_device_id,
                "debugger_origin": grant.debugger_origin,
                "scopes": ["diagnostics.read"],
                "issued_at_ms": grant.issued_at_ms,
                "expires_at_ms": grant.expires_at_ms,
                "signature": encoded_signature,
            }
        })).unwrap();
        assert_eq!(config.schema, "tokenme-diagnostics-grant/v1");
        assert_eq!(config.server_name.as_deref(), Some("diagnostics.local"));
        assert_eq!(config.certificate_der_b64_bytes().unwrap(), vec![1, 2, 3]);
        let camel_case_config: DiagnosticsBridgeConfig = serde_json::from_value(serde_json::json!({
            "schema": "tokenme-diagnostics-grant/v1",
            "endpoint": "https://localhost:47821",
            "serverName": "diagnostics.local",
            "certificate_pin": "00".repeat(32),
            "certificate_der_b64": base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]),
            "grant": serde_json::to_value(&config.grant).unwrap(),
        })).unwrap();
        assert_eq!(camel_case_config.server_name.as_deref(), Some("diagnostics.local"));
        let request = build_diagnostics_bridge_request(&config.grant, &identity, None, serde_json::json!({
            "jsonrpc": "2.0", "id": "probe", "method": "initialize", "params": {}
        }), 1).unwrap();
        assert_eq!(request.auth.debugger_device_id, identity.device_id);
        assert_eq!(request.auth.origin, identity.origin);
        assert_eq!(request.auth.request_id, "probe");
        assert_ne!(request.auth.signature, [0; 64]);

        let error = build_diagnostics_bridge_request(&config.grant, &identity, None, serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        }), 2).unwrap_err();
        assert!(error.contains("session is not initialized"));
        let list = build_diagnostics_bridge_request(&config.grant, &identity, Some("session-1".into()), serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}
        }), 3).unwrap();
        assert_eq!(list.auth.session_id.as_deref(), Some("session-1"));
        assert_eq!(list.method, "tools/list");
        let call = build_diagnostics_bridge_request(&config.grant, &identity, Some("session-1".into()), serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "logs.tail", "arguments": {}}
        }), 4).unwrap();
        assert_eq!(call.auth.session_id.as_deref(), Some("session-1"));
        assert_eq!(call.method, "tools/call");
    }
}

// ---------------------------------------------------------------- budget

/// Read, set and clear the caps in `tokenme/settings.json`. No index needed: this
/// only touches the settings file, and the report picks the caps up from there.
pub fn budget(action: &crate::args::BudgetCmd, json: bool) -> Result<(), String> {
    use usage_core::budget::{load_budgets, store_budget, Budget};

    match action {
        crate::args::BudgetCmd::List => {
            let budgets = load_budgets();
            if json {
                crate::context::print_json(&budgets);
                return Ok(());
            }
            if budgets.is_empty() {
                println!("no budgets set — tokenme can only show vendor-reported windows until you add one");
                println!("  tokenme budget set <tool> --daily 5 --monthly 50");
                return Ok(());
            }
            let mut t = Table::new(&["Tool", "Daily", "Monthly"]).right(&[1, 2]);
            for (tool, b) in &budgets {
                t.push(vec![
                    tool.clone(),
                    if b.daily_usd > 0.0 { format!("${:.2}", b.daily_usd) } else { "-".into() },
                    if b.monthly_usd > 0.0 { format!("${:.2}", b.monthly_usd) } else { "-".into() },
                ]);
            }
            print!("{}", t.render(false));
            Ok(())
        }
        crate::args::BudgetCmd::Set { tool, daily, monthly } => {
            if daily.is_none() && monthly.is_none() {
                return Err("give at least one of --daily / --monthly".into());
            }
            let previous = load_budgets().remove(tool.as_str()).unwrap_or_default();
            let budget = Budget {
                daily_usd: daily.unwrap_or(if tool_is_new(tool) { 0.0 } else { previous.daily_usd }),
                monthly_usd: monthly.unwrap_or(previous.monthly_usd),
            };
            let path = store_budget(tool, budget).map_err(|e| e.to_string())?;
            println!(
                "{}: {} → {}",
                tool,
                if budget.is_empty() { "no cap".into() } else {
                    [
                        budget.daily_usd.gt(&0.0).then(|| format!("${:.0}/天", budget.daily_usd)),
                        budget.monthly_usd.gt(&0.0).then(|| format!("${:.0}/月", budget.monthly_usd)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ")
                },
                path.display()
            );
            Ok(())
        }
        crate::args::BudgetCmd::Rm { tool } => {
            store_budget(tool, Budget::default()).map_err(|e| e.to_string())?;
            println!("{tool}: cap removed");
            Ok(())
        }
    }
}

/// `set x --monthly 50` on a tool with no stored cap must not inherit a stale
/// daily figure from a previous run of a different tool.
fn tool_is_new(tool: &str) -> bool {
    !usage_core::budget::load_budgets().contains_key(tool)
}

// ---------------------------------------------------------------- detect

pub fn detect(ctx: &Ctx) -> Result<(), String> {
    let (detected, problems) = ctx.detect();
    if !ctx.quiet() {
        for p in &problems {
            eprintln!("tokenme: {p}");
        }
    }
    let statuses = ctx.index.source_statuses(&detected).map_err(|e| e.to_string())?;

    if ctx.g.json {
        print_json(&statuses);
    } else {
        let mut t = Table::new(&["Tool", "Found", "Events", "Roots", "Hint"]).right(&[2]);
        let mut seen: Vec<&str> = Vec::new();
        for s in &statuses {
            seen.push(s.id.as_str());
            let found = if s.detected {
                render::paint("yes", "32", ctx.color)
            } else {
                render::paint("no", "2", ctx.color)
            };
            let problem = problems.iter().find(|p| p.starts_with(&format!("{}:", s.id)));
            let hint = match (problem, s.hint.as_deref()) {
                (Some(p), _) => render::truncate(p, 52),
                (None, Some(h)) => render::truncate(h, 52),
                (None, None) if s.detected => "ready".into(),
                (None, None) => "no readable logs here yet".into(),
            };
            t.push(vec![
                s.display.clone(),
                found,
                render::count(s.events_ingested),
                render::truncate(
                    &s.roots
                        .iter()
                        .map(|p| render::shorten_path(&p.to_string_lossy()))
                        .collect::<Vec<_>>()
                        .join("  "),
                    46,
                ),
                hint,
            ]);
        }
        // A built-in source that is neither detected nor indexed still belongs in
        // the table: "why is my Codex history missing" is the whole point.
        for adapter in &ctx.adapters {
            if seen.contains(&adapter.id()) {
                continue;
            }
            let problem = problems.iter().find(|p| p.starts_with(&format!("{}:", adapter.id())));
            t.push(vec![
                adapter.display_name().to_string(),
                render::paint("no", "2", ctx.color),
                "0".into(),
                String::new(),
                render::truncate(
                    problem.map(String::as_str).unwrap_or("not installed or no logs yet"),
                    52,
                ),
            ]);
        }
        print!("{}", t.render(ctx.color));
        println!(
            "{}",
            render::dim(&index_line(ctx, None), ctx.color)
        );
    }

    if detected.is_empty() {
        return Err(format!(
            "no supported AI tool logs found (looked for {})",
            usage_adapter_all::TOOL_IDS.join(", ")
        ));
    }
    Ok(())
}

fn index_line(ctx: &Ctx, report: Option<&IngestReport>) -> String {
    let total = ctx.index.event_count().unwrap_or(0);
    let files = ctx.index.files_tracked().unwrap_or(0);
    let age = ctx
        .index
        .meta_value("last_ingest_ms")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .map(|ms| render::ago(ctx.now_ms - ms))
        .unwrap_or_else(|| "never".into());
    match report {
        Some(r) => {
            // The pass's own voice: read failures and the adapters' notes about
            // silent ones. Empty on a healthy pass, so nothing changes there.
            let mut extra = String::new();
            for e in &r.errors {
                extra.push_str(&format!("\n  error: {e}"));
            }
            for n in &r.notes {
                extra.push_str(&format!("\n  note: {n}"));
            }
            format!(
                "index {} · {} events from {files} files · {} new, {} deduped, {} purged in {}ms{extra}",
                render::shorten_path(&ctx.db_path.display().to_string()),
                render::count(total),
                r.new_events,
                r.deduped,
                r.purged,
                r.took_ms
            )
        }
        None => format!(
            "index {} · {} events from {files} files · last ingest {age}",
            render::shorten_path(&ctx.db_path.display().to_string()),
            render::count(total)
        ),
    }
}

/// ------------------------------------------------------------ workbuddy-login

pub fn workbuddy_login() -> Result<(), String> {
    let summary = usage_quota::workbuddy_login()?;
    println!("{summary}");
    println!("run `tokenme quota` to see the WorkBuddy credit bars");
    Ok(())
}

// -------------------------------------------------- daily / weekly / month

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grain {
    Day,
    Week,
    Month,
}

impl Grain {
    pub fn label(self) -> &'static str {
        match self {
            Grain::Day => "day",
            Grain::Week => "week",
            Grain::Month => "month",
        }
    }
}

#[derive(Debug)]
struct Period {
    key: String,
    label: String,
    start_ms: i64,
    end_ms: i64,
}

fn local_date(ms: i64) -> Option<chrono::NaiveDate> {
    Local.timestamp_millis_opt(ms).single().map(|d| d.date_naive())
}

fn start_ms_of(d: chrono::NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .and_then(|t| Local.from_local_datetime(&t).single())
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

fn monday_of(d: chrono::NaiveDate) -> chrono::NaiveDate {
    d - chrono::Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

fn period_of(ms: i64, grain: Grain) -> Period {
    let today = local_date(ms).unwrap_or_else(|| Local::now().date_naive());
    match grain {
        Grain::Day => {
            let start = start_ms_of(today);
            Period { key: today.format("%Y-%m-%d").to_string(), label: today.format("%Y-%m-%d").to_string(), start_ms: start, end_ms: start + MS_DAY - 1 }
        }
        Grain::Week => {
            let monday = monday_of(today);
            let start = start_ms_of(monday);
            Period {
                key: format!("{}-W{:02}", monday.iso_week().year(), monday.iso_week().week()),
                label: format!("{}..{}", monday.format("%m-%d"), (monday + chrono::Duration::days(6)).format("%m-%d")),
                start_ms: start,
                end_ms: start + 7 * MS_DAY - 1,
            }
        }
        Grain::Month => {
            let first = today.with_day(1).unwrap_or(today);
            let start = start_ms_of(first);
            // End of the month = start of the next one minus a millisecond.
            let next = if first.month() == 12 {
                chrono::NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
            } else {
                chrono::NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
            };
            Period {
                key: first.format("%Y-%m").to_string(),
                label: first.format("%Y-%m").to_string(),
                start_ms: start,
                end_ms: next.map(start_ms_of).unwrap_or(start + 31 * MS_DAY) - 1,
            }
        }
    }
}

/// The last `n` periods, oldest first, ending with the one containing `now_ms`.
fn trailing_periods(now_ms: i64, grain: Grain, n: i64) -> Vec<Period> {
    let mut out = Vec::with_capacity(n as usize);
    let mut cursor = now_ms;
    for _ in 0..n.max(1) {
        let p = period_of(cursor, grain);
        cursor = p.start_ms - 1;
        out.push(p);
    }
    out.reverse();
    out
}

#[derive(serde::Serialize)]
struct PeriodRow {
    key: String,
    label: String,
    start_ms: i64,
    end_ms: i64,
    sessions: u64,
    requests: u64,
    input: f64,
    cache_creation: f64,
    cache_read: f64,
    output: f64,
    total_tokens: f64,
    /// `None` means "no price known", never zero spend.
    cost: Option<f64>,
    cached_pct: f64,
    unpriced_models: Vec<String>,
}

pub fn periods(ctx: &Ctx, grain: Grain, requested: i64) -> Result<(), String> {
    let n = requested.clamp(1, MAX_PERIODS);
    let capped = requested != n;
    let mut spans = trailing_periods(ctx.now_ms, grain, n);
    if let Some(since) = ctx.since_ms {
        spans.retain(|p| p.end_ms >= since);
    }
    if spans.is_empty() {
        return Err(format!(
            "--since {} is after the last {} period",
            ctx.g.since.as_deref().unwrap_or("?"),
            grain.label()
        ));
    }

    let range_start = spans[0].start_ms;
    let events = ctx.events_since(range_start)?;
    let mut buckets: BTreeMap<String, Vec<UsageEvent>> = BTreeMap::new();
    for ev in &events {
        buckets.entry(period_of(ev.ts_ms, grain).key).or_default().push(ev.clone());
    }

    let total_span =
        Period { key: "total".into(), label: "total".into(), start_ms: range_start, end_ms: ctx.now_ms };
    let totals = ctx.summary_of(&events);
    let mut rows: Vec<PeriodRow> = Vec::new();
    for span in &spans {
        let empty = Vec::new();
        let slice = buckets.get(&span.key).unwrap_or(&empty);
        rows.push(period_row(ctx, span, &ctx.summary_of(slice)));
    }
    let total_row = period_row(ctx, &total_span, &totals);

    if ctx.g.json {
        #[derive(serde::Serialize)]
        struct Out {
            grain: &'static str,
            rows: Vec<PeriodRow>,
            totals: PeriodRow,
            pricing: usage_core::PricingMeta,
        }
        print_json(&Out {
            grain: grain.label(),
            rows,
            totals: total_row,
            pricing: ctx.pricing_meta().clone(),
        });
        return Ok(());
    }

    let head = render::capitalised(grain.label());
    let mut t = Table::new(&[head.as_str(),
        "sessions",
        "requests",
        "input",
        "cache read",
        "output",
        "total",
        "cost",
        "cached",
    ])
    .right(&[1, 2, 3, 4, 5, 6, 7, 8]);
    for r in &rows {
        t.push(vec![
            r.label.clone(),
            render::count(r.sessions),
            render::count(r.requests),
            render::tokens(r.input),
            render::tokens(r.cache_read),
            render::tokens(r.output),
            render::tokens(r.total_tokens),
            cost_cell(r.cost),
            render::pct(r.cached_pct),
        ]);
    }
    t.footer(vec![
        "total".into(),
        render::count(totals.sessions),
        render::count(totals.requests),
        render::tokens(totals.counts.input),
        render::tokens(totals.counts.cache_read),
        render::tokens(totals.counts.output),
        render::tokens(totals.total_tokens),
        cost_cell(row_cost(ctx, &totals)),
        render::pct(totals.cached_pct),
    ]);
    print!("{}", t.render(ctx.color));
    footnotes(ctx, &totals, &head_note(grain, n, requested, capped, &spans));
    Ok(())
}

fn head_note(
    grain: Grain,
    n: i64,
    requested: i64,
    capped: bool,
    spans: &[Period],
) -> String {
    let cap =
        if capped { format!(", requested {requested}, capped at {MAX_PERIODS}") } else { String::new() };
    let clipped = if spans.len() as i64 != n {
        format!(" ({} in --since/--until)", spans.len())
    } else {
        String::new()
    };
    format!(
        "last {} {}{} · {} → {}{}{}",
        n,
        grain.label(),
        if n == 1 { "" } else { "s" },
        spans.first().map(|p| p.label.clone()).unwrap_or_default(),
        spans.last().map(|p| p.label.clone()).unwrap_or_default(),
        cap,
        clipped,
    )
}

/// `None` whenever the price table cannot answer, so a JSON consumer sees the
/// difference between free and unknown.
fn row_cost(ctx: &Ctx, s: &Summary) -> Option<f64> {
    if !ctx.pricing_available() {
        return None;
    }
    Some(s.cost)
}

fn period_row(ctx: &Ctx, span: &Period, s: &Summary) -> PeriodRow {
    PeriodRow {
        key: span.key.clone(),
        label: span.label.clone(),
        start_ms: span.start_ms,
        end_ms: span.end_ms,
        sessions: s.sessions,
        requests: s.requests,
        input: s.counts.input,
        cache_creation: s.counts.cache_creation,
        cache_read: s.counts.cache_read,
        output: s.counts.output,
        total_tokens: s.total_tokens,
        cost: row_cost(ctx, s),
        cached_pct: s.cached_pct,
        unpriced_models: s.unpriced.iter().map(|u| u.model.clone()).collect(),
    }
}

fn footnotes(ctx: &Ctx, totals: &Summary, head: &str) {
    let mut notes = vec![head.to_string(), ctx.pricing_footer()];
    if !totals.unpriced.is_empty() {
        let top: Vec<String> = totals
            .unpriced
            .iter()
            .take(3)
            .map(|u| format!("{} {} tok", u.model, render::tokens(u.total_tokens)))
            .collect();
        notes.push(format!("no price for {} — tokens counted, cost excluded", top.join(", ")));
    }
    if totals.credits > 0.0 {
        notes.push(if totals.credit_cost > 0.0 {
            format!(
                "{} credits metered -> ~{} of the cost at the published plan price",
                render::count(totals.credits as u64),
                render::money(totals.credit_cost)
            )
        } else {
            format!("{} credits metered (no published price, not in cost)", render::count(totals.credits as u64))
        });
    }
    for note in notes {
        println!("{}", render::dim(&note, ctx.color));
    }
}

// ---------------------------------------------------------------- report

fn window_slice(r: &usage_core::Report, w: Win) -> &Window {
    match w {
        Win::Day => &r.day,
        Win::Week => &r.week,
        Win::Month => &r.month,
    }
}

fn group_slice(w: &Window, g: Group) -> (&str, &Vec<Item>) {
    match g {
        Group::Tool => ("Tool", &w.breakdown.tools),
        Group::Model => ("Model", &w.breakdown.models),
        Group::Project => ("Project", &w.breakdown.projects),
        Group::Mcp => ("MCP", &w.breakdown.mcps),
        Group::Skill => ("Skill", &w.breakdown.skills),
    }
}

pub fn report(ctx: &Ctx, win: Win, group: Group) -> Result<(), String> {
    // The full `Report` carries whole-history totals, so this command reads the
    // whole index rather than a window slice.
    let events = ctx.all_events()?;
    let opts = ctx.report_options(ctx.source_statuses(), 20);
    let report = summarize(&events, &opts);

    if ctx.g.json {
        print_json(&report);
        return Ok(());
    }

    let w = window_slice(&report, win);
    let (col, items) = group_slice(w, group);
    println!(
        "{} {}",
        render::bold(&format!("{} · {}", win_label(win), w.label), ctx.color),
        render::dim(&format!("({} elapsed)", human_span(w.end_ms - w.start_ms)), ctx.color)
    );

    let mut t = Table::new(&[col, "requests", "sessions", "tokens", "cost", "cached", "share"])
        .right(&[1, 2, 3, 4, 5, 6]);
    for item in items {
        let share = if w.summary.total_tokens > 0.0 {
            render::pct(item.total_tokens / w.summary.total_tokens * 100.0)
        } else {
            "—".into()
        };
        t.push(vec![
            render::truncate(&item.label, 34),
            render::count(item.requests),
            render::count(item.sessions),
            render::tokens(item.total_tokens),
            if item.priced { render::money(item.cost) } else { "no price".into() },
            render::pct(item.counts.cached_pct()),
            share,
        ]);
    }
    if t.is_empty() {
        println!("{}", render::dim("nothing indexed in this window", ctx.color));
    } else {
        t.footer(vec![
            "window total".into(),
            render::count(w.summary.requests),
            render::count(w.summary.sessions),
            render::tokens(w.summary.total_tokens),
            cost_cell(if ctx.pricing_available() { Some(w.summary.cost) } else { None }),
            render::pct(w.summary.cached_pct),
            String::new(),
        ]);
        print!("{}", t.render(ctx.color));
    }

    // `Window::prev` is the equally long slice right before the window, so the
    // comparison length is the window's own elapsed span.
    let prev_len = w.end_ms - w.start_ms;
    println!(
        "{}",
        render::dim(
            &format!(
                "vs the equal-elapsed {} before it: cost {} → {} ({}) · tokens {} → {} ({})",
                human_span(prev_len),
                if ctx.pricing_available() { render::money(w.prev.cost) } else { "no price".into() },
                if ctx.pricing_available() { render::money(w.summary.cost) } else { "no price".into() },
                render::signed_pct(w.delta_cost_pct, ctx.color),
                render::tokens(w.prev.total_tokens),
                render::tokens(w.summary.total_tokens),
                render::signed_pct(w.delta_tokens_pct, ctx.color),
            ),
            ctx.color
        )
    );
    footnotes(ctx, &w.summary, &format!("{} events in window", render::count(w.summary.requests)));
    if let Some(all) = report.heatmap.iter().rev().find(|c| c.requests > 0) {
        println!(
            "{}",
            render::dim(
                &format!("last active day: {} · {}", all.date, render::tokens(all.total_tokens)),
                ctx.color
            )
        );
    }
    Ok(())
}

fn win_label(w: Win) -> &'static str {
    match w {
        Win::Day => "Today",
        Win::Week => "This week",
        Win::Month => "This month",
    }
}

fn human_span(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

// -------------------------------------------------------------- sessions

pub fn sessions(ctx: &Ctx, limit: usize) -> Result<(), String> {
    let lookback = ctx.now_ms - 90 * MS_DAY;
    let events = ctx.events_since(lookback)?;
    let opts = ctx.report_options(Vec::new(), limit.clamp(1, 200));
    let report = summarize(&events, &opts);
    if ctx.g.json {
        print_json(&report.recent_sessions);
        return Ok(());
    }
    let mut t = Table::new(&["last active", "device", "tool", "session", "project", "model", "reqs", "tokens", "cost"])
        .right(&[6, 7, 8]);
    for s in &report.recent_sessions {
        t.push(vec![
            render::ago(ctx.now_ms - s.last_ms),
            if s.origin.is_empty() { "local".into() } else { s.origin.clone() },
            s.tool.clone(),
            s.session.chars().take(12).collect(),
            render::truncate(&s.project.as_deref().map(render::shorten_path).unwrap_or_default(), 28),
            render::truncate(&s.model, 24),
            render::count(s.requests),
            render::tokens(s.total_tokens),
            cost_cell(if ctx.pricing_available() { Some(s.cost) } else { None }),
        ]);
    }
    if t.is_empty() {
        println!("{}", render::dim("no sessions indexed in the last 90 days", ctx.color));
        return Ok(());
    }
    print!("{}", t.render(ctx.color));
    footnotes(ctx, &report.all_time, &format!("{} sessions shown", report.recent_sessions.len()));
    Ok(())
}

// ----------------------------------------------------------------- quota

pub fn quota(ctx: &Ctx) -> Result<(), String> {
    let lookback = ctx.now_ms - 30 * MS_DAY;
    let events = ctx.events_since(lookback)?;
    let report = summarize(&events, &ctx.options());
    if ctx.g.json {
        print_json(&report.quotas);
        return Ok(());
    }
    if report.quotas.is_empty() {
        println!(
            "{}",
            render::dim(
                "no quota: Codex reports it in its own logs (kept for an hour only — an older sample is not the current answer), Claude/OpenCode/Copilot only answer when probed (a signed-out or proxy-managed tool has no window)",
                ctx.color
            )
        );
        return Ok(());
    }
    let mut t = Table::new(&["tool", "used", "", "window", "resets", "sample", "via"]).right(&[2, 3, 4, 5]);
    // A window that already elapsed is not remaining quota; the panel hides those
    // too, so the two surfaces have to agree.
    let live: Vec<_> = report
        .quotas
        .iter()
        .filter(|q| q.resets_at_ms == 0 || q.resets_at_ms > ctx.now_ms)
        .collect();
    if live.is_empty() {
        println!(
            "{}",
            render::dim("no unexpired quota window (the last one has already reset)", ctx.color)
        );
        return Ok(());
    }
    for q in &live {
        let window = match &q.label {
            Some(label) if !label.is_empty() => label.clone(),
            _ => render::minutes_window(q.window_minutes),
        };
        let resets = if q.resets_at_ms > 0 { render::until(q.resets_at_ms - ctx.now_ms) } else { "-".into() };
        t.push(vec![
            q.tool.clone(),
            render::pct(q.used_percent),
            render::bar(q.used_percent, 12),
            window,
            resets,
            render::ago(ctx.now_ms - q.sampled_at_ms),
            match q.origin {
                usage_core::QuotaOrigin::Probe => "probe",
                usage_core::QuotaOrigin::Log => "log",
                usage_core::QuotaOrigin::Budget => "budget",
            }
            .to_string(),
        ]);
    }
    print!("{}", t.render(ctx.color));
    Ok(())
}

// --------------------------------------------------------------- pricing

/// One price listing, flattened so `--json` carries the four stages by name.
#[derive(Debug, Clone, serde::Serialize)]
struct Listing {
    provider: String,
    input: f64,
    output: f64,
    cache_creation: f64,
    cache_read: f64,
}

fn listing(provider: &str, p: usage_core::Price) -> Listing {
    Listing { provider: provider.to_string(), input: p.input, output: p.output, cache_creation: p.cache_creation, cache_read: p.cache_read }
}

/// `$/M` with only the digits that mean something: most contested listings sit
/// two decimals apart, and a few are fractions of a cent.
fn usd(v: f64) -> String {
    let digits = if v.abs() >= 1.0 { 2 } else if v.abs() >= 0.01 { 3 } else { 5 };
    let s = format!("{v:.digits$}");
    let (whole, frac) = s.split_once('.').unwrap_or((s.as_str(), ""));
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("${whole}")
    } else {
        format!("${whole}.{frac}")
    }
}

/// A stage price, where a zero means "this stage is not billed" rather than
/// "free model", so it reads as a dash.
fn per_million(v: f64) -> String {
    if v == 0.0 { "—".into() } else { usd(v) }
}

fn price_line(p: usage_core::Price) -> String {
    format!(
        "in {} · out {} · cache write {} · cache read {}",
        per_million(p.input),
        per_million(p.output),
        per_million(p.cache_creation),
        per_million(p.cache_read),
    )
}

/// Which listing priced a model — the answer `explain` gives, shaped for JSON.
#[derive(Debug, Clone, serde::Serialize)]
struct PriceTrace {
    model: String,
    /// The listing the precedence rule chose; `None` means no vendor sells it.
    chosen: Option<Listing>,
    /// Everything it beat that charges differently, still in precedence order.
    /// Listings at an identical price are not in here — there is nothing to
    /// trace about a tie.
    beaten: Vec<Listing>,
    /// Where the pick sits among the others.
    range: Option<Range>,
    pricing: usage_core::PricingMeta,
}

/// The ends of the range among the listings the pick did **not** take, plus how
/// the pick itself sits in it. Computed over the alternatives only: including the
/// winner would let "cheapest" name the price already in use.
#[derive(Debug, Clone, serde::Serialize)]
struct Range {
    cheaper: usize,
    dearer: usize,
    cheapest: Option<Listing>,
    dearest: Option<Listing>,
}

fn range_of(winner: &Listing, beaten: &[Listing]) -> Range {
    let key = |l: &Listing| l.input + l.output + l.cache_creation + l.cache_read;
    // All four stages count: most of `grok-4.6`'s 23 listings agree on $2/$6 and
    // differ *only* in whether they record a cache-read price, so comparing input
    // and output alone would call those ties and hide the real spread.
    let w = key(winner);
    Range {
        cheaper: beaten.iter().filter(|l| key(l) < w).count(),
        dearer: beaten.iter().filter(|l| key(l) > w).count(),
        cheapest: beaten.iter().min_by(|a, b| key(a).total_cmp(&key(b))).cloned(),
        dearest: beaten.iter().max_by(|a, b| key(a).total_cmp(&key(b))).cloned(),
    }
}

fn listing_of(l: &Listing) -> String {
    format!(
        "{} · {}",
        l.provider,
        price_line(usage_core::Price { input: l.input, output: l.output, cache_creation: l.cache_creation, cache_read: l.cache_read })
    )
}

pub fn pricing_explain(
    map: &usage_core::PricingMap,
    models: &[String],
    json: bool,
    color: bool,
) -> Result<(), String> {
    let traces: Vec<PriceTrace> = models
        .iter()
        .map(|m| {
            let e = map.explain(m);
            let chosen = e.map(|x| listing(x.provider, x.price));
            let beaten: Vec<Listing> = e
                .map(|x| x.alternatives.iter().map(|(p, price)| listing(p, *price)).collect())
                .unwrap_or_default();
            let range = chosen.as_ref().filter(|_| !beaten.is_empty()).map(|c| range_of(c, &beaten));
            PriceTrace {
                model: m.clone(),
                chosen,
                beaten,
                range,
                pricing: map.meta().clone(),
            }
        })
        .collect();

    if json {
        print_json(&traces);
        return Ok(());
    }

    for t in &traces {
        println!("{}", render::bold(&t.model, color));
        let Some(chosen) = &t.chosen else {
            println!(
                "  {}",
                render::dim("no provider in the table sells this id — tokens counted, cost excluded", color)
            );
            continue;
        };
        let price = usage_core::Price {
            input: chosen.input,
            output: chosen.output,
            cache_creation: chosen.cache_creation,
            cache_read: chosen.cache_read,
        };
        println!("  priced by {:<18}{}", chosen.provider, price_line(price));
        let Some(range) = &t.range else {
            println!(
                "  {}",
                render::dim("listed by one provider only — this price was not a choice", color)
            );
            continue;
        };
        println!(
            "  {}",
            render::dim(
                &format!(
                    "{} other listing(s) charging differently: {} cheaper, {} dearer",
                    t.beaten.len(),
                    range.cheaper,
                    range.dearer
                ),
                color
            )
        );
        // Both ends need all four stages: most of a contested model's listings
        // agree on input/output and differ only in what they bill a cache read.
        if let Some(lo) = &range.cheapest {
            println!("  {}", render::dim(&format!("cheapest     {}", listing_of(lo)), color));
        }
        if let Some(hi) = &range.dearest {
            println!("  {}", render::dim(&format!("dearest      {}", listing_of(hi)), color));
        }
        println!(
            "  {}",
            render::dim(&format!("next in the precedence order: {}", listing_of(&t.beaten[0])), color)
        );
    }
    if let Some(first) = traces.first() {
        println!("{}", render::dim(&table_note(&first.pricing), color));
    }
    Ok(())
}

fn table_note(meta: &usage_core::PricingMeta) -> String {
    let src = match meta.source {
        usage_core::PricingSource::ModelsDev => "live from models.dev",
        usage_core::PricingSource::Cache => "cached models.dev snapshot",
        usage_core::PricingSource::Bundled => "bundled snapshot",
        usage_core::PricingSource::Unavailable => "UNAVAILABLE",
    };
    let age = if meta.fetched_at_ms == 0 {
        "never".to_string()
    } else {
        render::ago(usage_core::report::now_ms() - meta.fetched_at_ms)
    };
    format!(
        "price table: {src} · {} keys · fetched {age}{}",
        render::count(meta.key_count as u64),
        if meta.stale { " · >24h old" } else { "" }
    )
}

#[derive(Debug, Default)]
struct ModelAgg {
    counts: usage_core::TokenCounts,
    requests: u64,
    tools: BTreeMap<String, ()>,
}

/// A model whose money depends on which listing won.
#[derive(Debug, Clone, serde::Serialize)]
struct ContestedRow {
    model: String,
    tools: Vec<String>,
    requests: u64,
    total_tokens: f64,
    /// What the report charges for it.
    cost: f64,
    priced_by: String,
    /// Distinct prices on offer for this id **including** the pick. Not the
    /// number of listings: `grok-4.6` is sold by 23 providers on the wire but only
    /// four of them quote a different price, and the other nineteen cannot change
    /// this number.
    distinct_prices: usize,
    /// Difference if the next listing that charges differently had won. Listings
    /// at an identical price are not in this list at all, so their delta is 0.
    runner_up_delta: f64,
    /// Widest gap against any listing on offer.
    worst_delta: f64,
}

#[derive(serde::Serialize)]
struct Contested {
    rows: Vec<ContestedRow>,
    /// Rows hidden behind `--limit`; the totals below always cover all of them.
    omitted: usize,
    cost_in_rows: f64,
    max_runner_up_delta: f64,
    max_worst_delta: f64,
    contested_tokens_pct: f64,
    pricing: usage_core::PricingMeta,
}

/// Every model in the index that more than one provider sells, ranked by how
/// much the precedence rule could move the money. This is the error bar on
/// "cost": tokens are exact, a price is a pick between listings.
pub fn pricing_contested(ctx: &Ctx, limit: usize) -> Result<(), String> {
    let events = ctx.all_events()?;
    let map = ctx.pricing();

    let mut acc: BTreeMap<String, ModelAgg> = BTreeMap::new();
    let mut all_tokens = 0.0f64;
    for ev in &events {
        all_tokens += ev.counts.total();
        // Credit-metered sources are priced by a plan rate, not by this table,
        // so a listing spread says nothing about their money.
        if ev.meter == usage_core::Meter::Credits {
            continue;
        }
        let Some(model) = ev.model.as_deref() else { continue };
        let a = acc.entry(model.to_string()).or_default();
        a.counts += &ev.counts;
        a.requests += 1;
        a.tools.insert(ev.tool.clone(), ());
    }

    let mut rows: Vec<ContestedRow> = Vec::new();
    for (model, a) in &acc {
        let Some(e) = map.explain(model) else { continue };
        if e.alternatives.is_empty() {
            continue;
        }
        let winner = e.price.cost_of(&a.counts);
        let deltas: Vec<f64> = e.alternatives.iter().map(|(_, p)| p.cost_of(&a.counts) - winner).collect();
        let worst = deltas.iter().copied().fold(0.0f64, |m, d| if d.abs() > m.abs() { d } else { m });
        rows.push(ContestedRow {
            model: model.clone(),
            tools: a.tools.keys().cloned().collect(),
            requests: a.requests,
            total_tokens: a.counts.total(),
            cost: winner,
            priced_by: e.provider.to_string(),
            distinct_prices: deltas.len() + 1,
            runner_up_delta: deltas[0],
            worst_delta: worst,
        });
    }
    rows.sort_by(|x, y| {
        y.worst_delta
            .abs()
            .total_cmp(&x.worst_delta.abs())
            .then_with(|| y.cost.total_cmp(&x.cost))
    });

    let cost_in_rows: f64 = rows.iter().map(|r| r.cost).sum();
    let max_runner_up_delta: f64 = rows.iter().map(|r| r.runner_up_delta.abs()).sum();
    let max_worst_delta: f64 = rows.iter().map(|r| r.worst_delta.abs()).sum();
    let contested_tokens = rows.iter().map(|r| r.total_tokens).sum::<f64>();
    let contested_tokens_pct =
        if all_tokens > 0.0 { contested_tokens / all_tokens * 100.0 } else { 0.0 };

    if ctx.g.json {
        let omitted = rows.len().saturating_sub(limit);
        print_json(&Contested {
            rows,
            omitted,
            cost_in_rows,
            max_runner_up_delta,
            max_worst_delta,
            contested_tokens_pct,
            pricing: map.meta().clone(),
        });
        return Ok(());
    }

    let mut t = Table::new(&[
        "model",
        "tools",
        "requests",
        "tokens",
        "cost",
        "priced by",
        "prices",
        "Δ next",
        "Δ worst",
    ])
    .right(&[2, 3, 4, 6, 7, 8]);
    for r in rows.iter().take(limit.max(1)) {
        t.push(vec![
            render::truncate(&r.model, 30),
            r.tools.join(","),
            render::count(r.requests),
            render::tokens(r.total_tokens),
            render::money(r.cost),
            r.priced_by.clone(),
            render::count(r.distinct_prices as u64),
            signed_money(r.runner_up_delta),
            signed_money(r.worst_delta),
        ]);
    }
    if t.is_empty() {
        println!(
            "{}",
            render::dim(
                "nothing contested: every priced model in the index has exactly one listing",
                ctx.color
            )
        );
        return Ok(());
    }
    let omitted = rows.len().saturating_sub(limit.max(1));
    t.footer(vec![
        format!("{} contested model(s)", rows.len()),
        String::new(),
        String::new(),
        render::pct(contested_tokens_pct),
        render::money(cost_in_rows),
        String::new(),
        String::new(),
        format!("±{}", render::money(max_runner_up_delta)),
        format!("±{}", render::money(max_worst_delta)),
    ]);
    print!("{}", t.render(ctx.color));
    if omitted > 0 {
        println!("{}", render::dim(&format!("{omitted} more row(s) behind --limit; the totals cover all of them"), ctx.color));
    }
    // No probes here: this command is about prices, and `ctx.options()` would
    // spend up to 30 seconds polling quota nobody asked for.
    let total = summarize(&events, &usage_core::ReportOptions::new(map).with_now(ctx.now_ms)).all_time;
    println!(
        "{}",
        render::dim(
            &format!(
                "{} of the {} tokens in the index has more than one price on offer. If every pick went to the next-ranked listing the report would move by ±{}; across all listings the widest possible spread is ±{} (total cost {}, of which {} sits in the rows above), so a cost is exact only where Δ is 0.",
                render::pct(contested_tokens_pct),
                render::tokens(all_tokens),
                render::money(max_runner_up_delta),
                render::money(max_worst_delta),
                render::money(total.cost),
                render::money(cost_in_rows),
            ),
            ctx.color
        )
    );
    println!("{}", render::dim(&table_note(map.meta()), ctx.color));
    Ok(())
}

fn signed_money(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    // `usd`, not `money`: a row whose whole spread is four tenths of a cent is
    // still information, and `$0.00` would read as an exact tie.
    format!("{}{}", if v > 0.0 { "+" } else { "-" }, usd(v.abs()))
}

/// Which tool rows can show a real application icon. Presentation only — no
/// number depends on it — but the panel and a browser review both read this, so
/// the answer is stated rather than guessed at either end.
pub fn icons(json: bool) -> Result<(), String> {
    let map = usage_core::icons::icon_data_urls();
    if json {
        print_json(&map);
        return Ok(());
    }
    let mut t = Table::new(&["Tool", "Bundle tried", "Icon"]).right(&[2]);
    for (tool, names) in usage_core::icons::APP_BUNDLES {
        let (bundle, icon) = match (map.get(*tool), names.is_empty()) {
            (Some(url), true) => ("内置图标".into(), format!("{:.0} KB", decoded_kb(url))),
            (Some(url), false) => (names.join(", "), format!("{:.0} KB", decoded_kb(url))),
            (None, true) => ("—".into(), "命令行工具，没有 app 图标".into()),
            (None, false) => (names.join(", "), "本机未安装，或图标读不出 PNG".into()),
        };
        t.push(vec![tool.to_string(), bundle, icon]);
    }
    print!("{}", t.render(false));
    println!(
        "{}",
        render::dim("图标只影响外观：读不到就用彩色首字母，任何数字都不来自这里", false)
    );
    Ok(())
}

/// The icon's real byte count out of its `data:` URL: base64 is 4/3 of the
/// payload less the `=` padding, and rounding that down prints a 900 B badge as
/// "0 KB" next to a 20 KB one.
fn decoded_kb(data_url: &str) -> f64 {
    let b64 = data_url.split_once(',').map(|(_, rest)| rest).unwrap_or_default();
    let pad = b64.chars().rev().take_while(|c| *c == '=').count();
    let bytes = b64.len() / 4 * 3;
    (bytes - pad) as f64 / 1024.0
}

// ----------------------------------------------------------------- index

pub fn index(
    ctx: &mut Ctx,
    rebuild: bool,
    prune: bool,
    status: bool,
    force: bool,
) -> Result<(), String> {
    if force {
        ctx.force = true;
    }
    let mut last: Option<IngestReport> = None;
    if rebuild {
        let report = ctx
            .index
            .rebuild_with(&ctx.adapters, force)
            .map_err(|e| format!("rebuild failed: {e}"))?;
        for err in ctx.index.errors() {
            if !ctx.quiet() {
                eprintln!("tokenme: {err}");
            }
        }
        last = Some(report);
    } else if !status {
        last = ctx.ingest();
    }
    let mut pruned = 0u64;
    if prune {
        let cutoff = retention_cutoff(usage_core::report::now_ms());
        pruned = ctx.index.prune(cutoff).map_err(|e| format!("prune failed: {e}"))?;
        let _ = ctx.index.vacuum();
    }
    let per_tool = ctx.index.per_tool_counts().map_err(|e| e.to_string())?;
    let total = ctx.index.event_count().unwrap_or(0);
    let db_size = db_size(&ctx.db_path);

    if ctx.g.json {
        #[derive(serde::Serialize)]
        struct Status {
            db: PathBuf,
            bytes: u64,
            total_events: u64,
            files_tracked: usize,
            per_tool: BTreeMap<String, u64>,
            schema_version: Option<String>,
            last_ingest_ms: Option<i64>,
            pruned: u64,
            ingest: Option<IngestReport>,
            errors: Vec<String>,
        }
        print_json(&Status {
            db: ctx.db_path.clone(),
            bytes: db_size,
            total_events: total,
            files_tracked: ctx.index.files_tracked().unwrap_or(0),
            per_tool,
            schema_version: ctx.index.meta_value("schema_version").ok().flatten(),
            last_ingest_ms: ctx.index.meta_value("last_ingest_ms").ok().flatten().and_then(|s| s.parse().ok()),
            pruned,
            ingest: last.clone(),
            errors: ctx.index.errors().to_vec(),
        });
        return Ok(());
    }

    if let Some(r) = &last {
        print!(
            "{}",
            Table::new(&["files scanned", "files changed", "new events", "deduped", "purged", "total", "took"])
                .right(&[0, 1, 2, 3, 4, 5, 6])
                .row(vec![
                    render::count(r.files_scanned as u64),
                    render::count(r.files_changed as u64),
                    render::count(r.new_events),
                    render::count(r.deduped),
                    render::count(r.purged),
                    render::count(r.total_events),
                    format!("{}ms", r.took_ms),
                ])
                .render(ctx.color)
        );
        for (tool, n) in &r.per_tool {
            println!("  {tool}: {}", render::count(*n));
        }
    } else {
        print!(
            "{}",
            Table::new(&["total events", "files tracked", "index size"])
                .right(&[0, 1, 2])
                .row(vec![
                    render::count(total),
                    render::count(ctx.index.files_tracked().unwrap_or(0) as u64),
                    render::bytes(db_size),
                ])
                .render(ctx.color)
        );
        for (tool, n) in &per_tool {
            println!("  {tool}: {}", render::count(*n));
        }
    }
    if pruned > 0 {
        println!("pruned {pruned} event(s) older than {} days", usage_index::RETENTION_DAYS);
    }
    println!(
        "{}",
        render::dim(
            &format!(
                "db {} · schema {} · last ingest {}",
                render::shorten_path(&ctx.db_path.display().to_string()),
                ctx.index.meta_value("schema_version").ok().flatten().unwrap_or_default(),
                ctx.index
                    .meta_value("last_ingest_ms")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|ms| render::ago(ctx.now_ms - ms))
                    .unwrap_or_else(|| "never".into())
            ),
            ctx.color
        )
    );
    for err in ctx.index.errors() {
        eprintln!("tokenme: {err}");
    }
    Ok(())
}

fn db_size(path: &std::path::Path) -> u64 {
    [path.to_path_buf(), path.with_extension("db-wal"), path.with_extension("db-shm")]
        .into_iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

// ---------------------------------------------------------------- sync

fn fmt_day(ms: i64) -> String {
    local_date(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "?".into())
}

/// The manifest window is `[lo, hi)`; showing the inclusive day reads better.
fn window_label(lo_ms: i64, hi_ms: i64) -> String {
    format!("{} → {}", fmt_day(lo_ms), fmt_day(hi_ms.saturating_sub(1)))
}

/// The write half of machine-to-machine sync: dump a window of this machine's
/// events as a JSONL.gz bundle next to its manifest. The default directory is
/// the one the menu-bar engine watches, so on a Linux collector the whole
/// writer is `tokenme export --days 30` from a systemd timer.
pub fn export_sync(
    ctx: &mut Ctx,
    days: i64,
    out: Option<PathBuf>,
    origin: Option<String>,
) -> Result<(), String> {
    // Unlike a report, an export must not refuse an empty window: a collector
    // between bursts (or fresh out of the box) still ships a valid bundle.
    // Ingest failures degrade like everywhere else — export the index as-is
    // and let the next timer tick catch up.
    let _ = ctx.ingest();
    let out_dir = match out {
        Some(dir) => dir,
        None => usage_index::default_sync_dir()
            .ok_or_else(|| "no home directory; pass --out <dir>".to_string())?,
    };
    let origin = origin
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .unwrap_or_else(usage_index::hostname);
    let rep = ctx
        .index
        .export_sync(&usage_index::ExportOptions { days, out_dir, origin })
        .map_err(|e| e.to_string())?;

    if ctx.g.json {
        print_json(&rep);
        return Ok(());
    }
    println!(
        "{}",
        Table::new(&["origin", "rows", "window", "took"])
            .right(&[1, 3])
            .row(vec![
                rep.origin.clone(),
                render::count(rep.rows),
                window_label(rep.window_lo_ms, rep.window_hi_ms),
                format!("{}ms", rep.took_ms),
            ])
            .render(ctx.color)
    );
    println!(
        "{}",
        render::dim(
            &format!(
                "bundle   {}\nmanifest {}\nsha256   {}",
                render::shorten_path(&rep.gz_path.display().to_string()),
                render::shorten_path(&rep.manifest_path.display().to_string()),
                rep.sha256
            ),
            ctx.color
        )
    );
    Ok(())
}

/// The read half: merge one bundle exported by another machine. Idempotent —
/// re-importing the same file changes nothing — and atomic: any failure (bad
/// hash, format mismatch, a batch that does not reconcile) rolls the whole
/// merge back, so the index is never half-merged.
pub fn import_sync(ctx: &mut Ctx, file: &std::path::Path, dry_run: bool) -> Result<(), String> {
    let rep = ctx
        .index
        .import_sync(&usage_index::ImportOptions { file: file.to_path_buf(), dry_run })
        .map_err(|e| e.to_string())?;
    if rep.self_import && !ctx.quiet() {
        eprintln!(
            "tokenme: bundle origin {:?} equals this machine's hostname — merging a copy of this \
             machine's own export; rows stay keyed as foreign",
            rep.origin
        );
    }
    if ctx.g.json {
        print_json(&rep);
        return Ok(());
    }
    println!(
        "{}",
        Table::new(&["origin", "rows", "new", "updated", "deduped", "calls+", "stale", "took"])
            .right(&[1, 2, 3, 4, 5, 6, 7])
            .row(vec![
                rep.origin.clone(),
                render::count(rep.rows),
                render::count(rep.inserted),
                render::count(rep.updated),
                render::count(rep.deduped),
                render::count(rep.calls_added),
                render::count(rep.stale_rows),
                format!("{}ms", rep.took_ms),
            ])
            .render(ctx.color)
    );
    println!(
        "{}",
        render::dim(
            &format!(
                "bundle   {}\nwindow   {}\nsha256   {}",
                render::shorten_path(&rep.file),
                window_label(rep.window_lo_ms, rep.window_hi_ms),
                rep.sha256
            ),
            ctx.color
        )
    );
    if rep.dry_run {
        println!("{}", render::dim("dry run — everything parsed, merged and reconciled; nothing was committed", ctx.color));
    }
    Ok(())
}

/// Strict network sync command. This intentionally does not use the legacy
/// JSONL/SSH importer and does not remove a local package after an ACK: the
/// outbox/journal lifecycle is a later slice, so the command sends one bounded
/// snapshot and leaves the source index untouched.
pub fn sync_upload(
    ctx: &mut Ctx,
    endpoint: String,
    server_name: String,
    trust_cert: PathBuf,
    pin_sha256: String,
    identity_path: PathBuf,
    origin: String,
    primary_id: String,
    stream_id: String,
    data_epoch: u64,
    generation: u64,
    days: i64,
) -> Result<(), String> {
    if days <= 0 {
        return Err("--days must be positive".into());
    }
    if data_epoch == 0 || generation == 0 {
        return Err("--data-epoch and --generation start at 1".into());
    }
    let identity = DeviceIdentity::load_or_create(&identity_path, origin, "tokenme-cli")
        .map_err(|e| format!("cannot load sync identity {}: {e}", identity_path.display()))?;
    let hi_ms = ctx.now_ms.saturating_add(1);
    let lo_ms = ctx.now_ms.saturating_sub(days.saturating_mul(MS_DAY));
    let all_events = ctx.index.all_events().map_err(|e| e.to_string())?;
    let keyless_ids = stable_keyless_occurrences(&all_events);
    let mut keyless_seen: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut rows = Vec::new();
    for event in all_events.into_iter().filter(|event| event.ts_ms >= lo_ms) {
        if event.ts_ms < lo_ms || event.ts_ms >= hi_ms || event.source.starts_with("linux:") {
            continue;
        }
        let (key, occurrence_id) = match event.dedupe_key.clone() {
            Some(key) => (Some(key), None),
            None => {
                let fingerprint = keyless_fingerprint(&event);
                let ordinal = keyless_seen.entry(fingerprint.clone()).or_insert(0);
                let occurrence = keyless_ids
                    .get(&fingerprint)
                    .and_then(|ids| ids.get(*ordinal))
                    .cloned()
                    .ok_or_else(|| "strict sync could not assign a stable keyless occurrence".to_string())?;
                *ordinal += 1;
                (None, Some(occurrence))
            }
        };
        let calls = event.calls.iter().map(|call| NetworkCall {
            kind: match call.kind { CallKind::Mcp => "mcp", CallKind::Skill => "skill" }.into(),
            name: call.name.clone(),
        }).collect::<Vec<_>>();
        rows.push(NetworkRow {
            key,
            occurrence_id,
            tool: event.tool,
            ts_ms: event.ts_ms,
            session: event.session,
            project: event.project,
            model: event.model,
            meter: match event.meter { Meter::Tokens => "tokens", Meter::Credits => "credits" }.into(),
            source: if event.source.is_empty() { "local".into() } else { event.source },
            counts: NetworkCounts {
                in_tok: Some(event.counts.input),
                cc_tok: Some(event.counts.cache_creation),
                cr_tok: Some(event.counts.cache_read),
                out_tok: Some(event.counts.output),
                reason_tok: Some(event.counts.reasoning),
                credits: Some(event.counts.credits),
            },
            calls,
        });
    }
    let package = NetworkPackage::from_rows(
        identity.origin.clone(),
        usage_index::INDEX_SCHEMA_VERSION,
        lo_ms,
        hi_ms,
        &rows,
        ctx.now_ms,
    ).map_err(|e| format!("cannot build strict sync package: {e}"))?;
    let envelope = usage_device_sync::UploadEnvelope {
        protocol_id: usage_device_sync::PROTOCOL_ID.into(),
        primary_id,
        device_id: identity.device_id.clone(),
        origin: identity.origin.clone(),
        stream_id,
        data_epoch,
        generation,
        manifest_bytes: package.manifest_bytes.clone(),
        gzip_sha256: usage_device_sync::hash_bytes(&package.gzip_bytes),
    };
    let upload = SignedUpload::sign(envelope, &identity.signing_key, package.gzip_bytes)
        .map_err(|e| format!("cannot sign strict sync package: {e}"))?;
    let cert_der = usage_device_sync::load_pem_certificate(&trust_cert)
        .map_err(|e| format!("cannot load pinned trust certificate {}: {e}", trust_cert.display()))?;
    let pin = parse_sha256(&pin_sha256)?;
    let mut transport_config = HttpsTransportConfig::pinned(endpoint, server_name, cert_der);
    transport_config.pinned_certificate_sha256 = pin;
    let mut transport = Tls13HttpsTransport::new(transport_config).map_err(|e| e.to_string())?;
    let receipt = transport.upload(&upload).map_err(|e| format!("strict sync upload failed: {e}"))?;
    if ctx.g.json {
        print_json(&receipt);
    } else {
        println!("uploaded {} row(s): inserted {}, updated {}, deduped {}", receipt.rows, receipt.inserted, receipt.updated, receipt.deduped);
        println!("receipt {} / generation {}", hex_bytes(&receipt.receipt_hash), receipt.generation);
        println!("local index and pending source data were retained; this command has no outbox cleanup");
    }
    Ok(())
}

/// A keyless adapter row cannot use SQLite rowid: rebuild/prune may recreate it.
/// Derive its identity from stable source content and keep a deterministic
/// multiplicity for exact duplicates. The complete snapshot makes a window
/// export a projection of one numbering, not a fresh numbering of a dirty subset.
fn stable_keyless_occurrences(events: &[UsageEvent]) -> BTreeMap<Vec<u8>, Vec<String>> {
    let mut fingerprints: Vec<Vec<u8>> = events
        .iter()
        .filter(|event| event.dedupe_key.is_none())
        .map(keyless_fingerprint)
        .collect();
    fingerprints.sort();
    let mut result = BTreeMap::new();
    let mut counts: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for fingerprint in fingerprints {
        let ordinal = counts.entry(fingerprint.clone()).or_insert(0);
        result.entry(fingerprint.clone()).or_insert_with(Vec::new).push(format!(
            "codex-occ-v1:{}:{}",
            hex_bytes(&hash_bytes(&identity_bytes(b"tokenme/device-sync/keyless-occurrence/v1", &[&fingerprint]))),
            *ordinal,
        ));
        *ordinal += 1;
    }
    result
}

fn keyless_fingerprint(event: &UsageEvent) -> Vec<u8> {
    let meter = match event.meter { Meter::Tokens => "tokens", Meter::Credits => "credits" };
    let mut call_names = event.calls.iter().map(|call| {
        let kind = match call.kind { CallKind::Mcp => "mcp", CallKind::Skill => "skill" };
        format!("{kind}\u{1f}{name}", name = call.name)
    }).collect::<Vec<_>>();
    call_names.sort();
    let calls = call_names.join("\u{1e}");
    let count_bits = [
        event.counts.input.to_bits(), event.counts.cache_creation.to_bits(),
        event.counts.cache_read.to_bits(), event.counts.output.to_bits(),
        event.counts.reasoning.to_bits(), event.counts.credits.to_bits(),
    ];
    let ts = event.ts_ms.to_be_bytes();
    let count_bytes = count_bits.iter().flat_map(|bits| bits.to_be_bytes()).collect::<Vec<_>>();
    let fields = [
        event.tool.as_bytes(), &ts, event.session.as_bytes(), event.project.as_deref().unwrap_or("").as_bytes(),
        event.model.as_deref().unwrap_or("").as_bytes(), meter.as_bytes(), event.source.as_bytes(),
        count_bytes.as_slice(), calls.as_bytes(),
    ];
    identity_bytes(b"tokenme/device-sync/keyless-fingerprint/v1", &fields)
}

pub fn sync_status(ctx: &Ctx) -> Result<(), String> {
    let status = SyncStore::read_status(&ctx.db_path).map_err(|e| e.to_string())?;
    let secondary = SyncStore::read_secondary_config(&ctx.db_path).map_err(|e| e.to_string())?;
    let running = secondary_runtime_status(&ctx.db_path);
    if ctx.g.json {
        let outbox = secondary.as_ref().and_then(|config| SyncStore::open(&ctx.db_path, config.primary_id.clone()).ok()?.outbox_summary().ok());
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "registry": status,
            "secondary": secondary,
            "runtime_running": running,
            "outbox": outbox,
        })).map_err(|e| e.to_string())?);
    } else {
        println!("primary: {}", status.primary_id.as_deref().unwrap_or_else(|| secondary.as_ref().map(|config| config.primary_id.as_str()).unwrap_or("not initialized")));
        if let Some(config) = secondary {
            let outbox = SyncStore::open(&ctx.db_path, config.primary_id.clone()).ok().and_then(|store| store.outbox_summary().ok());
            println!("secondary: {}", if running { "running" } else if config.enabled { "stopped" } else { "disabled" });
            println!("endpoint: {}", config.endpoint);
            println!("certificate_pin: {}", hex_bytes(&config.certificate_pin));
            println!("stream: {}", config.stream_id.as_deref().unwrap_or("not opened"));
            println!("data_epoch: {}", config.data_epoch.map(|value| value.to_string()).unwrap_or_else(|| "-".into()));
            println!("last_connected_at_ms: {}", config.last_connected_at_ms.map(|value| value.to_string()).unwrap_or_else(|| "-".into()));
            println!("pending: {}", outbox.as_ref().map(|summary| summary.pending).unwrap_or(0));
            if let Some(error) = config.last_error { println!("last_error: {error}"); }
        }
        for device in status.devices {
            println!("{}\t{}\t{}\t{}\t{}", device.device_id, device.origin, device.os, format!("{:?}", device.status).to_lowercase(), device.last_generation.map(|n| n.to_string()).unwrap_or_else(|| "-".into()));
        }
        for request in status.pairing_requests {
            println!("pairing {}\t{}\t{}", request.request_id, request.origin, request.status);
        }
    }
    Ok(())
}

pub fn sync_run(ctx: &mut Ctx) -> Result<(), String> {
    let config = SyncStore::read_secondary_config(&ctx.db_path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no secondary sync configuration; pair this device first".to_string())?;
    let mut config_store = SyncStore::open(&ctx.db_path, config.primary_id.clone()).map_err(|error| error.to_string())?;
    if !config.enabled { config_store.set_secondary_enabled(true, ctx.now_ms).map_err(|error| error.to_string())?; }
    drop(config_store);
    let identity = DeviceIdentity::load_or_create(
        config.identity_path.clone(), config.origin.clone(), "tokenme-cli secondary",
    ).map_err(|error| format!("cannot load secondary identity: {error}"))?;
    if identity.device_id != config.device_id || identity.origin != config.origin {
        return Err("secondary identity does not match the paired device configuration".into());
    }
    let mut transport_config = HttpsTransportConfig::pinned(
        config.endpoint.clone(), config.server_name.clone(), config.certificate_der.clone(),
    );
    transport_config.pinned_certificate_sha256 = config.certificate_pin;
    let transport = Tls13HttpsTransport::new(transport_config).map_err(|error| error.to_string())?;
    let store = SyncStore::open(&ctx.db_path, config.primary_id.clone()).map_err(|error| error.to_string())?;
    let owner = format!("cli-secondary-{}", identity.device_id);
    let runtime_config = SyncRuntimeConfig::new(owner).map_err(|error| error.to_string())?;
    let cancellation = transport.cancellation();
    let runtime = SyncRuntime::new_secondary(store, transport, runtime_config, identity);
    let adapters = std::mem::take(&mut ctx.adapters);
    run_secondary_service(runtime, cancellation, config.primary_id, ctx.db_path.clone(), ctx.g.json, adapters)
}

pub fn sync_stop(ctx: &Ctx) -> Result<(), String> {
    #[cfg(unix)]
    {
        let path = secondary_control_path(&ctx.db_path)?;
        let mut socket = std::os::unix::net::UnixStream::connect(&path)
            .map_err(|error| format!("secondary runtime is not running ({}): {error}", path.display()))?;
        socket.write_all(b"stop\n").map_err(|error| format!("cannot request secondary stop: {error}"))?;
        let mut response = [0u8; 64];
        let count = socket.read(&mut response).map_err(|error| format!("cannot read secondary stop response: {error}"))?;
        println!("{}", String::from_utf8_lossy(&response[..count]).trim());
        if let Some(config) = SyncStore::read_secondary_config(&ctx.db_path).map_err(|error| error.to_string())? {
            let mut store = SyncStore::open(&ctx.db_path, config.primary_id).map_err(|error| error.to_string())?;
            store.set_secondary_enabled(false, ctx.now_ms).map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        Err("sync stop uses a private Unix socket; Windows named-pipe ACL implementation is UNVERIFIED on this platform".into())
    }
}

fn secondary_runtime_status(db_path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(path) = secondary_control_path(db_path) else { return false; };
        let Ok(mut socket) = std::os::unix::net::UnixStream::connect(path) else { return false; };
        if socket.write_all(b"status\n").is_err() { return false; }
        let mut response = [0u8; 64];
        socket.read(&mut response).is_ok()
    }
    #[cfg(not(unix))]
    { false }
}

#[cfg(unix)]
fn secondary_control_path(db_path: &std::path::Path) -> Result<PathBuf, String> {
    let root = cli_sync_dir_for_db(db_path)?;
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => return Err(format!("private sync directory is not a real directory: {}", root.display())),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(&root).map_err(|error| format!("cannot create private sync directory: {error}"))?,
        Err(error) => return Err(format!("cannot inspect private sync directory: {error}")),
    }
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).map_err(|error| format!("cannot protect private sync directory: {error}"))?;
    Ok(root.join("secondary-runtime.sock"))
}

#[cfg(unix)]
fn bind_secondary_control(db_path: &std::path::Path) -> Result<(std::os::unix::net::UnixListener, PathBuf), String> {
    use std::os::unix::fs::FileTypeExt;
    let path = secondary_control_path(db_path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
            return Err(format!("secondary control path is not a private Unix socket: {}", path.display()));
        }
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return Err("secondary runtime is already running".into());
        }
        std::fs::remove_file(&path).map_err(|error| format!("cannot remove stale secondary socket: {error}"))?;
    }
    let listener = std::os::unix::net::UnixListener::bind(&path).map_err(|error| format!("cannot bind private secondary socket: {error}"))?;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).map_err(|error| format!("cannot protect secondary socket: {error}"))?;
    listener.set_nonblocking(true).map_err(|error| format!("cannot configure secondary socket: {error}"))?;
    Ok((listener, path))
}

#[cfg(unix)]
fn run_secondary_service(
    runtime: SyncRuntime, cancellation: usage_device_sync::ClientTransportCancellation,
    primary_id: String, db_path: PathBuf, json: bool,
    adapters: Vec<Box<dyn usage_core::SourceAdapter>>,
) -> Result<(), String> {
    let (listener, path) = bind_secondary_control(&db_path)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = thread::spawn(move || {
        let mut runtime = runtime;
        runtime.run_until_with_ingest(&thread_stop, usage_core::report::now_ms, |store| {
            let filter = DateFilter::new(Some(retention_cutoff(usage_core::report::now_ms())), None);
            let options = usage_index::IngestOptions {
                force: false,
                claim_wait: std::time::Duration::from_millis(500),
            };
            if let Err(error) = store.index_mut().ingest_with(&adapters, &filter, &options) {
                eprintln!("tokenme: resident sync ingest failed ({error}); pending local data was retained");
            }
            Ok(())
        })
    });
    eprintln!("tokenme: secondary sync runtime started; stop with `tokenme sync stop`");
    let mut stop_requested = false;
    let mut control_error = None;
    'service: loop {
        if join.is_finished() { break; }
        match listener.accept() {
            Ok((mut socket, _)) => {
                let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                let _ = socket.set_write_timeout(Some(std::time::Duration::from_secs(1)));
                let mut command = [0u8; 64];
                let count = match socket.read(&mut command) {
                    Ok(0) => continue,
                    Ok(count) => count,
                    Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                    Err(_) => continue,
                };
                match std::str::from_utf8(&command[..count]).unwrap_or("").trim() {
                    "stop" => {
                        stop_requested = true;
                        stop.store(true, Ordering::Release);
                        cancellation.cancel();
                        let _ = socket.write_all(b"stopping\n");
                    }
                    "status" => {
                        let _ = socket.write_all(b"running\n");
                    }
                    _ => {
                        let _ = socket.write_all(b"unknown command\n");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(std::time::Duration::from_millis(100)),
            Err(error) => {
                control_error = Some(format!("secondary control listener failed: {error}"));
                stop.store(true, Ordering::Release);
                break 'service;
            }
        }
        if stop_requested { break; }
    }
    stop.store(true, Ordering::Release);
    cancellation.cancel();
    let joined = join.join().map_err(|_| "secondary runtime thread panicked".to_string());
    let cleanup = std::fs::remove_file(&path).map_err(|error| format!("cannot remove secondary control socket: {error}"));
    if let Some(error) = control_error { return Err(error); }
    cleanup?;
    let result = joined?.map_err(|error| error.to_string());
    if stop_requested {
        let mut store = SyncStore::open(&db_path, primary_id).map_err(|error| error.to_string())?;
        store.set_secondary_enabled(false, usage_core::report::now_ms()).map_err(|error| error.to_string())?;
    }
    let report = result?;
    if json { print_json(&report); }
    else { println!("secondary runtime stopped: {} pending package(s), {} acknowledged", report.pending, report.acknowledged); }
    Ok(())
}

#[cfg(not(unix))]
fn run_secondary_service(
    _runtime: SyncRuntime, _cancellation: usage_device_sync::ClientTransportCancellation,
    _primary_id: String, _db_path: PathBuf, _json: bool,
    _adapters: Vec<Box<dyn usage_core::SourceAdapter>>,
) -> Result<(), String> {
    Err("sync run uses a private Unix socket; Windows named-pipe ACL implementation is UNVERIFIED on this platform".into())
}

pub fn sync_invite(
    ctx: &Ctx, endpoint: String, pin_sha256: Option<String>, certificate: Option<PathBuf>,
    server_name: String, expires_at_ms: i64, primary_id: String,
) -> Result<(), String> {
    let certificate_der = if let Some(path) = certificate {
        usage_device_sync::load_pem_certificate(&path).map_err(|error| format!("cannot load primary certificate {}: {error}", path.display()))?
    } else {
        let root = cli_sync_dir_for_db(&ctx.db_path)?;
        let cert_path = root.join("primary-cert.pem");
        let key_path = root.join("primary-key.pem");
        usage_device_sync::ensure_server_certificate(&cert_path, &key_path, &server_name)
            .map_err(|error| format!("cannot create primary service certificate: {error}"))?.0
    };
    let derived_pin = hex_bytes(&hash_bytes(&certificate_der));
    let certificate_pin = pin_sha256.unwrap_or_else(|| derived_pin.clone());
    let parsed_pin = parse_sha256(&certificate_pin)?;
    if parsed_pin != hash_bytes(&certificate_der) {
        return Err("--pin-sha256 does not match the primary certificate; refusing to create an invite".into());
    }
    let mut store = SyncStore::open(&ctx.db_path, primary_id).map_err(|e| e.to_string())?;
    let primary_id = store.primary_id().to_string();
    let invite = store.create_pairing_invite(endpoint, certificate_pin, expires_at_ms).map_err(|e| e.to_string())?;
    let secret_hex = hex_bytes(&invite.secret);
    let url = invite.url_with_certificate(&primary_id, &server_name, &certificate_der).map_err(|e| e.to_string())?;
    if ctx.g.json {
        println!("{}", serde_json::json!({"invite_id": invite.invite_id, "endpoint": invite.endpoint, "certificate_pin": invite.certificate_pin, "expires_at_ms": invite.expires_at_ms, "secret_hex": secret_hex, "url": url}));
    } else {
        println!("invite_id {}", invite.invite_id);
        println!("secret_hex {}", secret_hex);
        println!("{}", url);
    }
    Ok(())
}

pub fn sync_approve(ctx: &Ctx, request_id: String, primary_id: String) -> Result<(), String> {
    let mut store = SyncStore::open(&ctx.db_path, primary_id).map_err(|e| e.to_string())?;
    let device = store.approve_pairing(&request_id, ctx.now_ms).map_err(|e| e.to_string())?;
    if ctx.g.json { println!("{}", serde_json::to_string(&device).map_err(|e| e.to_string())?); }
    else { println!("approved {} ({})", device.device_id, device.origin); }
    Ok(())
}

pub fn sync_pair(
    ctx: &Ctx,
    invite_url: Option<String>,
    endpoint: Option<String>,
    server_name: Option<String>,
    trust_cert: Option<PathBuf>,
    pin_sha256: Option<String>,
    invite_id: Option<String>,
    invite_secret_hex: Option<String>,
    identity_path: Option<PathBuf>,
    origin: Option<String>,
    os: Option<String>,
) -> Result<(), String> {
    if let Some(url) = invite_url {
        let details = PairingInvite::parse_url(&url).map_err(|error| format!("invalid pairing invite: {error}"))?;
        let primary_id = details.primary_id.clone().ok_or_else(|| "invite does not contain primary id; create a fresh Panel/CLI invite".to_string())?;
        let certificate_der = details.certificate_der.clone().ok_or_else(|| "invite does not contain the public certificate; create a fresh invite".to_string())?;
        let pin = parse_sha256(&details.invite.certificate_pin)?;
        if usage_device_sync::hash_bytes(&certificate_der) != pin {
            return Err("pairing invite certificate does not match its advertised pin".into());
        }
        let identity_path = identity_path.unwrap_or(cli_sync_dir_for_db(&ctx.db_path)?.join("secondary-identity.json"));
        let identity = DeviceIdentity::load_or_create(&identity_path, origin.unwrap_or_else(|| "secondary".into()), "tokenme-cli secondary")
            .map_err(|error| format!("cannot load sync identity: {error}"))?;
        let server_name = details.server_name.unwrap_or_else(|| "localhost".into());
        let request = PairingRequest::new(&details.invite, &identity, os.unwrap_or_else(|| std::env::consts::OS.into())).map_err(|error| error.to_string())?;
        let mut transport_config = HttpsTransportConfig::pinned(details.invite.endpoint.clone(), server_name.clone(), certificate_der.clone());
        transport_config.pinned_certificate_sha256 = pin;
        let mut transport = Tls13HttpsTransport::new(transport_config).map_err(|error| error.to_string())?;
        let state = transport.send_pairing_request(&request).map_err(|error| format!("pairing request failed: {error}"))?;
        let mut store = SyncStore::open(&ctx.db_path, primary_id.clone()).map_err(|error| error.to_string())?;
        store.save_secondary_config(&usage_device_sync::SecondarySyncConfig {
            primary_id, device_id: identity.device_id.clone(), origin: identity.origin.clone(), endpoint: details.invite.endpoint,
            server_name, certificate_der, certificate_pin: pin, identity_path, stream_id: None, data_epoch: None,
            expected_generation: None, enabled: true, capture_enabled: true, last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, ctx.now_ms).map_err(|error| error.to_string())?;
        if ctx.g.json { print_json(&state); } else { println!("pairing request {} ({}) — secondary runtime can now use `tokenme sync run`", state.request_id, state.status); }
        return Ok(());
    }

    let endpoint = endpoint.ok_or_else(|| "--endpoint is required unless --invite is used".to_string())?;
    let server_name = server_name.ok_or_else(|| "--server-name is required unless --invite is used".to_string())?;
    let trust_cert = trust_cert.ok_or_else(|| "--trust-cert is required unless --invite is used".to_string())?;
    let pin_sha256 = pin_sha256.ok_or_else(|| "--pin-sha256 is required unless --invite is used".to_string())?;
    let invite_id = invite_id.ok_or_else(|| "--invite-id is required unless --invite is used".to_string())?;
    let invite_secret_hex = invite_secret_hex.ok_or_else(|| "--invite-secret-hex is required unless --invite is used".to_string())?;
    let identity_path = identity_path.ok_or_else(|| "--identity is required unless --invite is used".to_string())?;
    let origin = origin.ok_or_else(|| "--origin is required unless --invite is used".to_string())?;
    let os = os.unwrap_or_else(|| std::env::consts::OS.into());
    let identity = DeviceIdentity::load_or_create(&identity_path, origin, "tokenme-cli").map_err(|error| error.to_string())?;
    let invite_secret_hex = if invite_secret_hex == "-" {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).map_err(|error| format!("cannot read invite secret from stdin: {error}"))?;
        input.trim().to_string()
    } else { invite_secret_hex };
    let secret = parse_hex(&invite_secret_hex, "--invite-secret-hex")?;
    if secret.len() != 32 { return Err("--invite-secret-hex must be 32 bytes".into()); }
    let invite = PairingInvite { invite_id, endpoint, certificate_pin: pin_sha256.clone(), expires_at_ms: i64::MAX, secret };
    let request = PairingRequest::new(&invite, &identity, os).map_err(|error| error.to_string())?;
    let cert_der = usage_device_sync::load_pem_certificate(&trust_cert).map_err(|error| error.to_string())?;
    let pin = parse_sha256(&pin_sha256)?;
    if usage_device_sync::hash_bytes(&cert_der) != pin {
        return Err("--trust-cert does not hash to --pin-sha256; refusing to send the invite secret".into());
    }
    let mut config = HttpsTransportConfig::pinned(invite.endpoint.clone(), server_name, cert_der);
    config.pinned_certificate_sha256 = pin;
    let mut transport = Tls13HttpsTransport::new(config).map_err(|error| error.to_string())?;
    let state = transport.send_pairing_request(&request).map_err(|error| error.to_string())?;
    println!("pairing request {} ({})", state.request_id, state.status);
    Ok(())
}

fn cli_sync_dir_for_db(db_path: &std::path::Path) -> Result<PathBuf, String> {
    let absolute = if db_path.is_absolute() {
        db_path.to_path_buf()
    } else {
        std::env::current_dir().map_err(|error| format!("cannot resolve --db path: {error}"))?.join(db_path)
    };
    let parent = absolute.parent().ok_or_else(|| format!("--db path has no parent directory: {}", absolute.display()))?;
    let root = parent.join("device-sync");
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => return Err(format!("private sync directory is not a real directory: {}", root.display())),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(&root).map_err(|error| format!("cannot create private sync directory: {error}"))?,
        Err(error) => return Err(format!("cannot inspect private sync directory: {error}")),
    }
    #[cfg(unix)]
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).map_err(|error| format!("cannot protect private sync directory: {error}"))?;
    Ok(root)
}

pub fn sync_open_stream(ctx: &Ctx, device_id: String, primary_id: String) -> Result<(), String> {
    let mut store = SyncStore::open(&ctx.db_path, primary_id).map_err(|e| e.to_string())?;
    let receipt = store.open_stream_for_registered(&device_id, ctx.now_ms).map_err(|e| e.to_string())?;
    if ctx.g.json { println!("{}", serde_json::json!({"primary_id": receipt.primary_id, "device_id": receipt.device_id, "stream_id": receipt.stream_id, "data_epoch": receipt.data_epoch, "expected_generation": receipt.expected_generation})); }
    else { println!("stream_id {} data_epoch {} generation {}", receipt.stream_id, receipt.data_epoch, receipt.expected_generation); }
    Ok(())
}

pub fn sync_serve(ctx: &Ctx, bind: String, cert: Option<PathBuf>, key: Option<PathBuf>, primary_id: String, allow_lan: bool) -> Result<(), String> {
    let bind_addr: SocketAddr = bind.parse().map_err(|e| format!("--bind is invalid: {e}"))?;
    let root = cli_sync_dir_for_db(&ctx.db_path)?;
    let cert_path = cert.unwrap_or_else(|| root.join("primary-cert.pem"));
    let key_path = key.unwrap_or_else(|| root.join("primary-key.pem"));
    let (certificate_der, private_key_der) = usage_device_sync::ensure_server_certificate(&cert_path, &key_path, "localhost")
        .map_err(|error| format!("cannot prepare private primary certificate: {error}"))?;
    let store = SyncStore::open(&ctx.db_path, primary_id).map_err(|e| e.to_string())?;
    let server = usage_device_sync::HttpsSyncServer::bind(HttpsServerConfig {
        bind_addr, certificate_der, private_key_der, allow_lan, allowed_peer_ips: Vec::new(),
        max_body_bytes: usage_device_sync::MAX_COMPRESSED_BYTES, max_concurrent: 1,
        staging_dir: ctx.db_path.with_file_name("device-sync-staging"),
        idle_timeout: std::time::Duration::from_secs(30), total_timeout: std::time::Duration::from_secs(600),
        staging_quota_bytes: 512 * 1024 * 1024,
    }, store).map_err(|e| e.to_string())?;
    eprintln!("tokenme: sync primary listening on {}", server.local_addr().map_err(|e| e.to_string())?);
    server.run().map_err(|e| e.to_string())
}

fn parse_sha256(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("--pin-sha256 must be exactly 64 hexadecimal characters".into());
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(|_| "invalid --pin-sha256".to_string())?;
    }
    Ok(out)
}

fn parse_hex(value: &str, label: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{label} must contain an even number of hexadecimal characters"));
    }
    (0..value.len() / 2).map(|i| u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(|_| format!("invalid {label}"))).collect()
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// DSH accuracy audit: the projection ledger the desktop app keeps, printed
/// next to what the index actually holds, per session — plus the structural
/// facts that explain honest drift (inherited pre-v4 events, subagent spend
/// outside `tokenUsage`, a cumulative event that day-buckets onto its last
/// prompt day). A row that agrees reads `ok`; anything else names the gap.
pub fn dsh_doctor(ctx: &Ctx) -> Result<(), String> {
    let ledger = usage_adapter_dsh::ledger();
    let mut indexed: BTreeMap<String, (f64, f64, f64, f64, Option<String>, i64)> = BTreeMap::new();
    for e in ctx.index.all_events().map_err(|e| e.to_string())?.into_iter().filter(|e| e.tool == "dsh") {
        let slot = indexed.entry(e.session.clone()).or_insert((0.0, 0.0, 0.0, 0.0, None, 0));
        slot.0 += e.counts.input;
        slot.1 += e.counts.cache_creation;
        slot.2 += e.counts.cache_read;
        slot.3 += e.counts.output;
        if slot.4.is_none() {
            slot.4 = e.model.clone();
        }
        slot.5 += 1;
    }
    let mut table = Table::new(&[
        "session",
        "fmt",
        "ledger in/out/cr/cc",
        "index in/out/cr/cc",
        "events",
        "status",
    ]);
    let mut mismatches = 0usize;
    for row in &ledger {
        let slot = indexed.get(&row.session);
        let ledger_str = format!("{:.0}/{:.0}/{:.0}/{:.0}", row.input, row.output, row.cache_read, row.cache_write);
        let (index_str, events, status) = match slot {
            Some((i, cc, cr, o, _, n)) if row.format >= 4 => {
                let agree = (*i - row.input).abs() < 0.5
                    && (*o - row.output).abs() < 0.5
                    && (*cr - row.cache_read).abs() < 0.5
                    && (*cc - row.cache_write).abs() < 0.5;
                if agree {
                    (format!("{:.0}/{:.0}/{:.0}/{:.0}", i, o, cr, cc), *n, "ok".to_string())
                } else {
                    mismatches += 1;
                    (format!("{:.0}/{:.0}/{:.0}/{:.0}", i, o, cr, cc), *n, "MISMATCH".to_string())
                }
            }
            Some((i, cc, cr, o, _, n)) => {
                // A v3 stream carries per-call events; the ledger row is
                // identity-only, so agreement is "events exist".
                (format!("{:.0}/{:.0}/{:.0}/{:.0}", i, o, cr, cc), *n, "v3 stream".to_string())
            }
            None if row.input == 0.0 && row.output == 0.0 && row.cache_read == 0.0 && row.cache_write == 0.0 => {
                ("—".to_string(), 0, "ok · empty".to_string())
            }
            None => {
                mismatches += 1;
                ("—".to_string(), 0, "not indexed".to_string())
            }
        };
        let mut notes = Vec::new();
        if row.inherited_events > 0 {
            notes.push(format!("inherited {} pre-v4 events", row.inherited_events));
        }
        if row.subagents > 0 {
            notes.push(format!("{} subagents (spend outside tokenUsage)", row.subagents));
        }
        table.push(vec![
            row.session.chars().take(12).collect(),
            row.format.to_string(),
            ledger_str,
            index_str,
            events.to_string(),
            if notes.is_empty() { status } else { format!("{status} · {}", notes.join("; ")) },
        ]);
    }
    for (session, (i, cc, cr, o, _, n)) in &indexed {
        if !ledger.iter().any(|r| r.session == *session) {
            mismatches += 1;
            table.push(vec![
                session.chars().take(12).collect(),
                "?".into(),
                "—".into(),
                format!("{i:.0}/{o:.0}/{cr:.0}/{cc:.0}"),
                n.to_string(),
                "indexed but no projection".into(),
            ]);
        }
    }
    println!("DSH ledger: {} sessions · index: {} sessions · {} mismatched", ledger.len(), indexed.len(), mismatches);
    print!("{}", table.render(false));
    println!("notes: tokenUsage is the main thread's ledger — subagent and pre-v4 spend sit outside it; a");
    println!("cumulative session day-buckets onto its last prompt day, so day views move as sessions age.");
    Ok(())
}

/// Codex accuracy audit: every rollout replayed from scratch (cursor 0, no
/// manifest, no dedupe) and aggregated per session, printed next to what the
/// index holds. Codex has no vendor-side ledger — the rollouts are the only
/// record — so replay IS the independent truth: a divergence is a cursor or
/// dedupe failure in the indexer, and the row names which side moved.
pub fn codex_doctor(ctx: &Ctx) -> Result<(), String> {
    let replay = usage_adapter_codex::replay(&DateFilter::default());
    let mut indexed: BTreeMap<String, (f64, f64, f64, f64, usize)> = BTreeMap::new();
    for e in ctx.index.all_events().map_err(|e| e.to_string())?.into_iter().filter(|e| e.tool == "codex") {
        let slot = indexed.entry(e.session.clone()).or_insert((0.0, 0.0, 0.0, 0.0, 0));
        slot.0 += e.counts.input;
        slot.1 += e.counts.cache_creation;
        slot.2 += e.counts.cache_read;
        slot.3 += e.counts.output;
        slot.4 += 1;
    }
    let mut table = Table::new(&[
        "session",
        "replay in/out/cached/calls",
        "index in/out/cached/calls",
        "files",
        "status",
    ]);
    let mut mismatches = 0usize;
    let mut checked = 0usize;
    for row in &replay {
        let replay_str = format!(
            "{:.0}/{:.0}/{:.0}/{}",
            row.input, row.output, row.cached, row.calls
        );
        let (index_str, status) = match indexed.get(&row.session) {
            Some((i, cc, cr, o, n)) => {
                checked += 1;
                let agree = (*i - row.input).abs() < 0.5
                    && (*o - row.output).abs() < 0.5
                    && ((*cr + *cc) - row.cached).abs() < 0.5
                    && *n == row.calls;
                let index_str = format!("{i:.0}/{o:.0}/{:.0}/{n}", cr + cc);
                if agree { (index_str, "ok".to_string()) } else { mismatches += 1; (index_str, "MISMATCH".to_string()) }
            }
            None => {
                ("—".to_string(), "not indexed (retention or ingest gap)".to_string())
            }
        };
        table.push(vec![
            row.session.chars().take(14).collect(),
            replay_str,
            index_str,
            row.files.to_string(),
            status,
        ]);
    }
    for (session, (i, cc, cr, o, n)) in &indexed {
        if !replay.iter().any(|r| r.session == *session) {
            mismatches += 1;
            table.push(vec![
                session.chars().take(14).collect(),
                "—".into(),
                format!("{i:.0}/{o:.0}/{:.0}/{n}", cr + cc),
                "?".into(),
                "indexed but replay finds nothing".into(),
            ]);
        }
    }
    println!(
        "Codex replay: {} sessions · index: {} sessions · {} checked · {} mismatched",
        replay.len(),
        indexed.len(),
        checked,
        mismatches
    );
    print!("{}", table.render(false));
    println!("notes: the replay re-parses every rollout from scratch — the index side is what the panel");
    println!("shows; a MISMATCH means the indexer counted some call twice or dropped it.");
    Ok(())
}
