//! Black-box tests for the `tokenme` binary. They avoid the real adapters: the
//! cases here either exit before probing or use `--no-ingest`/`--status`, so a
//! half-finished adapter cannot break the CLI's own contract.

use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::process::{Child, Command, Output, Stdio};
use std::io::Write;
use std::thread;
use std::time::Duration;

use usage_device_sync::{
    ensure_server_certificate, hash_bytes, DeviceIdentity, HttpsServerConfig, PairingRequest,
    SecondarySyncConfig, SyncStore, HttpsSyncServer,
};

fn tokenme() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tokenme"))
}

/// A database of this process's own. Two `cargo test` runs on one fixed path (or a
/// leftover from an aborted one) make SQLite answer "disk I/O error", which reads
/// like a product failure but is a harness collision.
fn temp_db(name: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "tokenme-cli-test-{name}-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    remove_db(&path);
    path
}

/// A database and its WAL sidecars, which SQLite leaves behind on an abrupt exit.
fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn unique_dir(_name: &str) -> PathBuf {
    // macOS limits Unix-domain socket paths to SUN_LEN; keep the fixture root
    // short because the product deliberately nests its socket under db/device-sync.
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!("tms-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn codex_line(ts: &str, input: u64, output: u64) -> String {
    format!("{}\n", serde_json::json!({
        "timestamp": ts,
        "type": "event_msg",
        "payload": {"type": "token_count", "info": {"last_token_usage": {
            "input_tokens": input, "cached_input_tokens": 0,
            "cache_write_input_tokens": 0, "output_tokens": output,
            "reasoning_output_tokens": 0, "total_tokens": input + output,
        }}}
    }))
}

fn read_event_count(path: &Path) -> u64 {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row("SELECT count(*) FROM event", [], |row| row.get::<_, i64>(0)).unwrap().max(0) as u64
}

fn read_receipt_evidence(path: &Path, device_id: &str, stream_id: &str, generation: u64) -> Option<(u64, usize)> {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row(
        "SELECT rows, length(receipt_hash) FROM device_receipt WHERE device_id = ?1 AND stream_id = ?2 AND generation = ?3",
        rusqlite::params![device_id, stream_id, generation as i64],
        |row| Ok((row.get::<_, i64>(0)?.max(0) as u64, row.get::<_, i64>(1)?.max(0) as usize)),
    ).ok()
}

fn sync_runtime_failure_diagnostics(
    mut child: Child,
    db: &Path,
    primary_db: &Path,
    device: &DeviceIdentity,
    stream_id: &str,
    generation: u64,
    rollout: &Path,
    stage: &str,
) -> String {
    let child_status = child.try_wait().unwrap();
    if child_status.is_none() { let _ = child.kill(); }
    let output = child.wait_with_output().unwrap();
    let count = |conn: &rusqlite::Connection, sql: &str| -> String {
        conn.query_row(sql, [], |row| row.get::<_, i64>(0))
            .map(|value| value.to_string())
            .unwrap_or_else(|error| format!("error:{error}"))
    };
    let secondary = match SyncStore::open(db, "primary-cli") {
        Ok(store) => {
            let conn = store.index().conn();
            let events = count(conn, "SELECT count(*) FROM event");
            let dirty = count(conn, "SELECT count(*) FROM sync_dirty");
            let journal = count(conn, "SELECT count(*) FROM device_sync_journal");
            let outbox = store.outbox_summary().map(|summary| format!("pending={},in_flight={},bytes={}", summary.pending, summary.in_flight, summary.bytes)).unwrap_or_else(|error| format!("error:{error}"));
            let config = store.secondary_config().map(|value| format!("{value:?}")).unwrap_or_else(|error| format!("error:{error}"));
            format!("secondary event={events} dirty={dirty} journal={journal} outbox={outbox} config={config}")
        }
        Err(error) => format!("secondary open error:{error}"),
    };
    let primary = match SyncStore::open(primary_db, "primary-cli") {
        Ok(store) => {
            let events = store.index().event_count().map(|value| value.to_string()).unwrap_or_else(|error| format!("error:{error}"));
            let status = SyncStore::read_status(primary_db).map(|value| format!("{value:?}")).unwrap_or_else(|error| format!("error:{error}"));
            let receipt = store.receipt(&device.device_id, stream_id, generation).map(|value| format!("{value:?}")).unwrap_or_else(|error| format!("error:{error}"));
            format!("primary event={events} status={status} receipt={receipt}")
        }
        Err(error) => format!("primary open error:{error}"),
    };
    let fixture = std::fs::metadata(rollout)
        .map(|metadata| format!("exists=true bytes={} modified={:?}", metadata.len(), metadata.modified().ok()))
        .unwrap_or_else(|error| format!("exists=false error:{error}"));
    format!(
        "stage={stage}; child_status={child_status:?}; child_stdout={}; child_stderr={}; fixture={} cutoff_ms={}; {}; {}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr), fixture,
        usage_index::retention_cutoff(usage_core::report::now_ms()), secondary, primary,
    )
}

fn run(args: &[&str]) -> Output {
    tokenme()
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tokenme builds and runs")
        .wait_with_output()
        .expect("tokenme exited")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn help_is_printable() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for cmd in ["detect", "daily", "weekly", "monthly", "report", "sessions", "quota", "index", "budget", "pricing"] {
        assert!(text.contains(cmd), "help omits {cmd}");
    }
    assert!(text.contains("--pricing-override"), "help omits global flags");
}

#[test]
fn no_arguments_prints_help() {
    let out = run(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Usage:"), "expected help, got {:?}", stdout(&out));
}

#[test]
fn an_unopenable_index_is_exit_1() {
    // `/proc` is a useful Unix fixture, but Windows treats that spelling as a
    // relative filename and SQLite can create it.  A directory is not a valid
    // SQLite database on either platform and gives the same open failure.
    let blocked = std::env::temp_dir().join(format!(
        "tokenme-cli-unopenable-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir(&blocked);
    std::fs::create_dir(&blocked).expect("create an unopenable database directory");
    let blocked = blocked.to_string_lossy().into_owned();
    let out = run(&["--db", &blocked, "index", "--status"]);
    let _ = std::fs::remove_dir(&blocked);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("cannot open index"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "nothing may reach stdout on failure");
}

#[test]
fn an_unknown_tool_is_rejected() {
    // A `--db` even though the bad tool is rejected before the index opens:
    // the default path is the operator's live index, and no test may touch it.
    let db = temp_db("reject-tool");
    let out = run(&["--db", &db.to_string_lossy(), "--tool", "not-a-tool", "detect"]);
    remove_db(&db);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown --tool"), "{}", stderr(&out));
}

#[test]
fn a_malformed_price_override_is_rejected() {
    // Same rule: the override parses before the index opens, but the default
    // path is the live index and the parse order is an implementation detail.
    let db = temp_db("reject-price");
    let out = run(&["--db", &db.to_string_lossy(), "--pricing-override", "glm-5.2", "report", "--no-ingest"]);
    remove_db(&db);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("--pricing-override"), "{}", stderr(&out));
}

#[test]
fn a_malformed_date_is_rejected() {
    // The date parses AFTER the index opens (context::build), so without a
    // --db this probe opened the operator's live index on every run — and a
    // fresh one-shot repair there would fire from a test bench.
    let db = temp_db("reject-date");
    let out = run(&["--db", &db.to_string_lossy(), "--since", "last-tuesday", "daily", "--no-ingest"]);
    remove_db(&db);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("YYYY-MM-DD"), "{}", stderr(&out));
}

#[test]
fn index_status_on_a_fresh_db_is_valid_json() {
    let db = temp_db("status");
    let out = run(&["--db", &db.to_string_lossy(), "--offline", "index", "--status", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("--json parses");
    assert_eq!(value["total_events"], 0);
    assert_eq!(value["files_tracked"], 0);
    assert_eq!(value["ingest"], serde_json::Value::Null);
    assert_eq!(value["errors"].as_array().map(Vec::len), Some(0));
    remove_db(&db);
}

#[test]
fn periods_on_an_empty_index_render_zero_rows() {
    let db = temp_db("periods");
    let out = run(&[
        "--db",
        &db.to_string_lossy(),
        "--offline",
        "--json",
        "--no-ingest",
        "daily",
        "--days",
        "3",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json parses");
    assert_eq!(value["grain"], "day");
    assert_eq!(value["rows"].as_array().map(Vec::len), Some(3));
    assert_eq!(value["rows"][0]["requests"], 0);
    assert_eq!(value["totals"]["total_tokens"], 0.0);
    remove_db(&db);

    // The same state in a terminal renders a table with an explicit total row.
    let text = run(&["--db", &db.to_string_lossy(), "--offline", "--no-ingest", "daily", "--days", "3"]);
    let rendered = stdout(&text);
    assert!(rendered.contains("Day"), "{rendered}");
    assert!(rendered.contains("total"), "{rendered}");
    assert!(rendered.contains("prices:"), "the price source must be stated: {rendered}");
}

#[test]
fn report_json_is_the_whole_usage_core_report() {
    let db = temp_db("report");
    let out = run(&[
        "--db",
        &db.to_string_lossy(),
        "--offline",
        "--json",
        "--no-ingest",
        "report",
        "--window",
        "week",
        "--group",
        "model",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("report parses");
    // The panel and the CLI agree because this is `usage_core::Report`, field for field.
    for key in ["day", "week", "month", "heatmap", "quotas", "sources", "pricing", "recent_sessions", "all_time"] {
        assert!(value.get(key).is_some(), "Report.{key} missing");
    }
    assert_eq!(value["heatmap"].as_array().map(Vec::len), Some(742));
    assert_eq!(value["day"]["summary"]["requests"], 0);
    remove_db(&db);
}

#[cfg(unix)]
#[test]
fn sync_run_ingests_and_uploads_a_codex_append_using_only_the_db_sidecar() {
    let root = unique_dir("resident-codex");
    let db = root.join("secondary.db");
    let primary_db = root.join("primary.db");
    let codex_home = root.join("codex");
    let rollout = codex_home.join("sessions/2026-10-14/rollout-2026-10-14T01-00-00-00000000-0000-4000-8000-0000000000c1.jsonl");
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let initial_rollout = concat!(
        "{\"timestamp\":\"2026-10-14T01:00:00.000Z\",\"type\":\"session_meta\",\"payload\":{\"session_id\":\"resident-session\",\"cwd\":\"/tmp\"}}\n",
        "{\"timestamp\":\"2026-10-14T01:00:01.000Z\",\"type\":\"turn_context\",\"payload\":{\"turn_id\":\"turn-1\",\"cwd\":\"/tmp\",\"model\":\"gpt-5.6-luna\"}}\n{}"
    ).replace("{}", &codex_line("2026-10-14T01:00:02.000Z", 10, 2));
    std::fs::write(&rollout, initial_rollout
    ).unwrap();

    let cert_root = root.join("certificate");
    std::fs::create_dir_all(&cert_root).unwrap();
    std::fs::set_permissions(&cert_root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let (cert_der, key_der) = ensure_server_certificate(
        &cert_root.join("primary-cert.pem"), &cert_root.join("primary-key.pem"), "localhost",
    ).unwrap();
    let pin = hash_bytes(&cert_der);
    let pin_hex: String = pin.iter().map(|byte| format!("{byte:02x}")).collect();

    let device = DeviceIdentity::create("cli-resident-origin", "CLI resident").unwrap();
    let mut primary = SyncStore::open(&primary_db, "primary-cli").unwrap();
    // The invite is only used to exercise the signed pairing state here; the
    // actual runtime endpoint is replaced with the bound loopback port below.
    let invite = primary.create_pairing_invite("https://127.0.0.1:47821", &pin_hex, i64::MAX).unwrap();
    let request = PairingRequest::new(&invite, &device, "linux").unwrap();
    let pending = primary.receive_pairing_request(&request, 1).unwrap();
    primary.approve_pairing(&pending.request_id, 2).unwrap();
    let stream = primary.open_stream_for_registered(&device.device_id, 3).unwrap();

    let server = HttpsSyncServer::bind(HttpsServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        certificate_der: cert_der.clone(), private_key_der: key_der,
        allow_lan: false, allowed_peer_ips: Vec::new(),
        max_body_bytes: usage_device_sync::MAX_COMPRESSED_BYTES, max_concurrent: 1,
        staging_dir: root.join("staging"), idle_timeout: Duration::from_secs(30),
        total_timeout: Duration::from_secs(600), staging_quota_bytes: 512 * 1024 * 1024,
    }, primary).unwrap();
    let addr = server.local_addr().unwrap();
    let server_stop = Arc::new(AtomicBool::new(false));
    let server_stop_thread = Arc::clone(&server_stop);
    let server_thread = thread::spawn(move || server.run_until(&server_stop_thread));

    let identity_path = root.join("device-sync/secondary-identity.json");
    device.save(&identity_path).unwrap();
    let mut secondary = SyncStore::open(&db, "primary-cli").unwrap();
    secondary.save_secondary_config(&SecondarySyncConfig {
        primary_id: "primary-cli".into(), device_id: device.device_id.clone(), origin: device.origin.clone(),
        endpoint: format!("https://127.0.0.1:{}", addr.port()), server_name: "localhost".into(),
        certificate_der: cert_der.clone(), certificate_pin: pin, identity_path: identity_path.clone(),
        stream_id: Some(stream.stream_id.clone()), data_epoch: Some(stream.data_epoch),
        expected_generation: Some(stream.expected_generation), enabled: true, capture_enabled: true,
        last_error: None, last_manifest_hash: None, last_gzip_hash: None,
        last_connected_at_ms: None, last_ack_generation: None, last_ack_receipt_hash: None,
    }, 4).unwrap();

    let child = tokenme()
        .args(["--db", db.to_str().unwrap(), "--tool", "codex", "--json", "sync", "run"])
        .env("CODEX_HOME", &codex_home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let socket = root.join("device-sync/secondary-runtime.sock");
    let start_deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !socket.exists() && std::time::Instant::now() < start_deadline { thread::sleep(Duration::from_millis(50)); }
    if !socket.exists() {
        let diagnostic = sync_runtime_failure_diagnostics(child, &db, &primary_db, &device, &stream.stream_id, stream.expected_generation, &rollout, "socket");
        server_stop.store(true, Ordering::Release);
        let server_result = server_thread.join().map(|result| format!("{result:?}")).unwrap_or_else(|_| "server thread panicked".into());
        panic!("CLI did not create the db-scoped control socket at {}: {diagnostic}; server={server_result}", socket.display());
    }

    let first_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < first_deadline {
        if read_event_count(&primary_db) >= 1 && read_receipt_evidence(&primary_db, &device.device_id, &stream.stream_id, stream.expected_generation).is_some() { break; }
        thread::sleep(Duration::from_millis(100));
    }
    let initial_receipt = read_receipt_evidence(&primary_db, &device.device_id, &stream.stream_id, stream.expected_generation);
    if read_event_count(&primary_db) < 1 || initial_receipt.is_none() {
        let diagnostic = sync_runtime_failure_diagnostics(child, &db, &primary_db, &device, &stream.stream_id, stream.expected_generation, &rollout, "initial");
        server_stop.store(true, Ordering::Release);
        let server_result = server_thread.join().map(|result| format!("{result:?}")).unwrap_or_else(|_| "server thread panicked".into());
        panic!("initial Codex row/receipt was not uploaded: event_count={} receipt={initial_receipt:?}; {diagnostic}; server={server_result}", read_event_count(&primary_db));
    }
    assert_eq!(initial_receipt.map(|(_, hash_len)| hash_len), Some(32), "initial receipt hash must be durable");

    {
        let mut file = std::fs::OpenOptions::new().append(true).open(&rollout).unwrap();
        file.write_all(codex_line("2026-10-14T01:00:03.000Z", 20, 3).as_bytes()).unwrap();
        file.flush().unwrap();
    }
    let second_deadline = std::time::Instant::now() + Duration::from_secs(40);
    while std::time::Instant::now() < second_deadline {
        if read_event_count(&primary_db) >= 2 && read_receipt_evidence(&primary_db, &device.device_id, &stream.stream_id, stream.expected_generation + 1).is_some() { break; }
        thread::sleep(Duration::from_millis(100));
    }
    let append_receipt = read_receipt_evidence(&primary_db, &device.device_id, &stream.stream_id, stream.expected_generation + 1);
    if read_event_count(&primary_db) < 2 || append_receipt.is_none() {
        let diagnostic = sync_runtime_failure_diagnostics(child, &db, &primary_db, &device, &stream.stream_id, stream.expected_generation + 1, &rollout, "append");
        server_stop.store(true, Ordering::Release);
        let server_result = server_thread.join().map(|result| format!("{result:?}")).unwrap_or_else(|_| "server thread panicked".into());
        panic!("Codex append/receipt was not uploaded: event_count={} receipt={append_receipt:?}; {diagnostic}; server={server_result}", read_event_count(&primary_db));
    }
    assert_eq!(append_receipt.map(|(_, hash_len)| hash_len), Some(32), "append receipt hash must be durable");

    let mut control = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    control.write_all(b"stop\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "sync run failed: {}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("sync run stop must emit JSON report");
    assert!(report["acknowledged"].as_u64().unwrap_or(0) >= 2, "resident runtime must ACK both generations: {report}");
    server_stop.store(true, Ordering::Release);
    assert!(server_thread.join().unwrap().is_ok());
    let after_stop = SyncStore::open(&db, "primary-cli").unwrap();
    let after_config = after_stop.secondary_config().unwrap().unwrap();
    assert_eq!(after_stop.outbox_summary().unwrap().pending, 0, "stop must leave no acknowledged package pending");
    assert_eq!(after_stop.outbox_summary().unwrap().in_flight, 0, "stop must leave no package in flight");
    assert_eq!(after_config.last_ack_generation, Some(stream.expected_generation + 1));
    assert_eq!(after_config.last_ack_receipt_hash.map(|hash| hash.len()), Some(32));
    assert!(identity_path.starts_with(root.join("device-sync")));
    assert!(!root.join("device-sync").join("primary-cert.pem").exists(), "secondary --db sidecar must not borrow a primary identity");
    let _ = std::fs::remove_dir_all(root);
}

// ------------------------------------------------------------------- pricing

/// A price is a pick between listings, so the pick has to be inspectable — that
/// is what `pricing explain` is for, and it must work with no index at all.
#[test]
fn a_forced_price_names_itself_as_the_source() {
    let out = run(&[
        "--offline",
        "--json",
        "--pricing-override",
        "glm-9.9=1/2/0.5/0.1",
        "pricing",
        "explain",
        "glm-9.9",
        "not-a-model-anywhere",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json parses");
    assert_eq!(v[0]["chosen"]["provider"], "override", "a forced price is not a models.dev listing");
    assert_eq!(v[0]["chosen"]["input"], 1.0);
    assert_eq!(v[0]["beaten"].as_array().map(Vec::len), Some(0), "nothing was contested");
    assert_eq!(v[1]["chosen"], serde_json::Value::Null, "an unknown id has no price, not a $0 one");
    assert!(v[0]["pricing"]["key_count"].as_u64().unwrap_or(0) > 0, "the table it came from is stated");
}

#[test]
fn explain_needs_a_model_to_explain() {
    let out = run(&["--offline", "pricing", "explain"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("at least one model"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "nothing may reach stdout on failure");
}

/// The error bar on the money column, on an index with nothing in it.
#[test]
fn contested_pricing_of_an_empty_index_is_an_empty_answer() {
    let db = temp_db("contested");
    let out = run(&[
        "--db",
        &db.to_string_lossy(),
        "--offline",
        "--json",
        "--no-ingest",
        "pricing",
        "contested",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json parses");
    assert_eq!(v["rows"].as_array().map(Vec::len), Some(0));
    assert_eq!(v["cost_in_rows"], 0.0);
    assert_eq!(v["max_worst_delta"], 0.0);

    let text = run(&["--db", &db.to_string_lossy(), "--offline", "--no-ingest", "pricing", "contested"]);
    assert!(stdout(&text).contains("nothing contested"), "{}", stdout(&text));
    remove_db(&db);
}

/// Icons are decoration, so the contract is only that the answer is a map of
/// data URLs — and that a machine or platform with no bundles yields `{}` rather
/// than an error, since the panel then draws monograms everywhere.
#[test]
fn icon_urls_are_a_map_of_data_urls_or_nothing() {
    let out = run(&["--offline", "--json", "icons"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json parses");
    let map = v.as_object().expect("an object keyed by tool");
    for (tool, url) in map {
        let url = url.as_str().unwrap_or_default();
        assert!(url.starts_with("data:image/png;base64,"), "{tool}: {url}");
    }
}
