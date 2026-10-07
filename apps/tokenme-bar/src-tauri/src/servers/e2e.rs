//! Live end-to-end test against a real Linux sshd (this repo uses a colima
//! docker container: debian + openssh-server, password auth, an aarch64/amd64
//! host so the bundled static collector executes natively).
//!
//! `#[ignore]`d on purpose: it needs a running sshd target plus a one-time
//! password, so it can never be part of the default gate. Run it explicitly:
//!
//! ```text
//! docker build -t tokenme-e2e-ssh /tmp/tokenme-e2e   # /tmp/tokenme-e2e/Dockerfile
//! docker run -d --name tokenme-e2e -p 127.0.0.1:2222:22 tokenme-e2e-ssh
//! E2E_SSH_PASSWORD=<the password baked into the image> \
//!   cargo test --lib servers::e2e -- --ignored --nocapture
//! ```
//!
//! Everything runs under a scratch `HOME` (`E2E_HOME`, default
//! `/tmp/tokenme-e2e-home`): servers.json, the dedicated key pair,
//! `~/tokenme-sync` and the index are created there, never in the real
//! profile. The scratch dir is kept after the run for inspection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::Manager;

use super::fetch;
use super::{keys, remote, ssh, wizard, ServerRecord};
use crate::engine;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
    tauri::async_runtime::block_on(fut)
}

/// Polls until the engine's published report carries `want_rows` **and**
/// `want_sha` for `file` — a stale record for the same file name from an
/// earlier pull must not satisfy the wait.
fn wait_for_sync<R: tauri::Runtime>(
    handle: &tauri::AppHandle<R>,
    file: &str,
    want_rows: u64,
    want_sha: &str,
    timeout: Duration,
) -> Option<usage_core::SyncRecord> {
    let started = Instant::now();
    loop {
        if let Some(report) = handle.state::<engine::Shared>().report() {
            if let Some(rec) = report.syncs.iter().find(|s| s.file == file) {
                if rec.rows == want_rows && rec.sha256.as_deref() == Some(want_sha) {
                    return Some(rec.clone());
                }
            }
        }
        if started.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn event_count() -> Option<u64> {
    let path = usage_index::Index::default_path()?;
    usage_index::Index::open(path).ok()?.event_count().ok()
}

/// The one row appended mid-test, built with the exact key set of the planted
/// fixture (see /tmp/tokenme-e2e/fixture.py).
fn appended_row() -> String {
    let row = serde_json::json!({
        "type": "assistant",
        "timestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string(),
        "message": {
            "id": "msg_e2e_3",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "e2e appended"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "stop_details": null,
            "usage": {
                "input_tokens": 1200,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 640,
                "output_tokens": 345,
                "output_tokens_details": {"thinking_tokens": 0},
                "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                "service_tier": "standard",
                "cache_creation": {"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 0},
                "inference_geo": "",
                "iterations": [],
                "speed": "standard"
            }
        },
        "uuid": "e2e0000-0000-4000-8000-000000000103",
        "sessionId": "e2e00000-0000-4000-8000-000000000001",
        "cwd": "/home/e2e/proj",
        "parentUuid": null,
        "userType": "external",
        "version": "2.1.283",
        "gitBranch": "",
        "isSidechain": false,
        "entrypoint": "cli",
        "apiBlockIndex": 0,
        "effort": null,
        "perTurnEffort": null,
        "wireToolInputs": []
    });
    serde_json::to_string(&row).unwrap()
}

#[test]
#[ignore = "needs a live sshd target (see module docs); run with --ignored"]
fn live_install_pull_resync_cleanup() {
    let host = env_or("E2E_SSH_HOST", "127.0.0.1");
    let port: u16 = env_or("E2E_SSH_PORT", "2222").parse().expect("E2E_SSH_PORT");
    let user = env_or("E2E_SSH_USER", "e2e");
    let password =
        std::env::var("E2E_SSH_PASSWORD").expect("set E2E_SSH_PASSWORD (the sshd user's password)");
    let name = env_or("E2E_SERVER_NAME", "e2e-box");
    let home = std::path::PathBuf::from(env_or("E2E_HOME", "/tmp/tokenme-e2e-home"));
    let started = Instant::now();

    // Capture the real home before the override below: several adapters prefer
    // a tool-root env var over `~` (the Qoder adapter reads `$QODER_CONFIG_DIR`
    // first), and harness shells export those pointing at the real profile —
    // the first green-looking pass ingested 14,838 real Qoder events into the
    // scratch index that way. Every variable whose whole value is a path under
    // the real home gets dropped right after `HOME` is redirected.
    let real_home = dirs::home_dir().expect("a real home to scrub").to_string_lossy().into_owned();

    // Fresh scratch profile; HOME must be redirected before anything reads dirs.
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("create scratch home");
    std::env::set_var("HOME", &home);
    assert_eq!(
        dirs::home_dir().as_deref(),
        Some(home.as_path()),
        "HOME override did not take effect"
    );
    let doomed: Vec<std::ffi::OsString> = std::env::vars_os()
        .filter(|(k, v)| {
            k != "HOME"
                && k != "PATH"
                && v.to_str().is_some_and(|v| v.starts_with(real_home.as_str()))
        })
        .map(|(k, _)| k)
        .collect();
    for key in &doomed {
        std::env::remove_var(key);
    }
    println!("e2e: scrubbed {} inherited env vars rooted in {real_home}: {doomed:?}", doomed.len());
    let panel = super::panel_dir().expect("panel dir under the scratch home");
    assert!(panel.starts_with(&home), "panel dir escaped the scratch home: {}", panel.display());

    // The engine, wired exactly as lib.rs wires it — its own thread included.
    let (tx, rx) = std::sync::mpsc::channel::<engine::Msg>();
    let app = tauri::test::mock_builder()
        .manage(engine::EngineChannel(tx.clone()))
        .manage(engine::Shared::new(crate::Settings::load()))
        .manage(super::state())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("mock app");
    let handle = app.handle().clone();
    engine::start(handle.clone(), rx, tx);
    let state = handle.state::<Arc<super::ServersState>>().inner().clone();

    // 1. Probe: TOFU fingerprint + arch over password auth.
    let probe_out = block_on(wizard::probe(
        &state,
        wizard::ProbeReq {
            name: name.clone(),
            host: host.clone(),
            port: Some(port),
            user: user.clone(),
            auth: wizard::AuthReq::Password { password: password.clone() },
        },
    ));
    assert!(probe_out.ok, "probe failed: {:?}", probe_out.error);
    assert!(!probe_out.mismatch);
    assert_eq!(probe_out.arch.as_deref(), Some("aarch64"), "arch: {:?}", probe_out.arch);
    assert!(probe_out.hostname.is_some(), "probe should capture the hostname");
    let session = probe_out.session.expect("probe session id");
    let fingerprint = probe_out.fingerprint.clone().expect("probe fingerprint");

    // 2. Install: keygen → pubkey → dedicated reconnect → clear_pw → arch →
    //    collector → export → detect → merge (waits for the engine's publish).
    let out = block_on(wizard::install(
        &handle,
        &state,
        wizard::InstallReq { session, every_secs: 900, days: 30, fingerprint: fingerprint.clone() },
    ));
    assert!(out.ok, "install failed: {:?}", out.error);
    assert_eq!(out.rows, 2, "want exactly the two planted fixture rows");
    assert!(!out.merge_pending, "the engine did not publish the merge within the wizard's wait");
    let server = out.server.expect("server view");
    assert_eq!(server.status, "ok");
    assert_eq!(server.fingerprint, fingerprint);
    assert!(
        server.tools.iter().any(|t| t.contains("Claude")),
        "detect should see the fixture's tool, got {:?}",
        server.tools
    );

    // 3. The bundle + manifest landed in the scratch ~/tokenme-sync.
    let sync_dir = usage_index::default_sync_dir().expect("sync dir");
    assert!(sync_dir.starts_with(&home), "sync dir escaped the scratch home");
    let gz_name = format!("tokenme-{name}.jsonl.gz");
    let gz = sync_dir.join(&gz_name);
    let manifest_path = sync_dir.join(format!("tokenme-{name}.manifest.json"));
    assert!(gz.is_file(), "missing {}", gz.display());
    assert!(manifest_path.is_file());
    let manifest: usage_index::SyncManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.format, usage_index::SYNC_FORMAT);
    assert_eq!(manifest.origin, name);
    assert_eq!(manifest.rows, out.rows);
    let sha = usage_index::sha256_file(&gz).unwrap();
    assert!(sha.eq_ignore_ascii_case(&manifest.sha256));
    assert!(manifest.window_lo_ms < manifest.window_hi_ms);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [&gz, &manifest_path] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} must be 0600", p.display());
        }
    }

    // 4. The registration persisted — with the pin, without the password.
    let raw = std::fs::read_to_string(super::store_path().expect("servers.json path")).unwrap();
    assert!(raw.contains(&name) && raw.contains(&fingerprint), "servers.json is missing the record");
    assert!(!raw.contains(&password), "the password reached servers.json");
    for path in files_under(&panel) {
        let bytes = std::fs::read(&path).unwrap_or_default();
        assert!(
            !contains_bytes(&bytes, password.as_bytes()),
            "the password reached {}",
            path.display()
        );
    }
    let key_path = keys::dedicated_key_path().expect("dedicated key path");
    assert!(key_path.is_file(), "dedicated key missing at {}", key_path.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "dedicated key must be 0600");
    }

    // 5. The engine's report carries the merge: sync record + machine entry.
    let report = handle.state::<engine::Shared>().report().expect("engine published a report");
    let rec = report
        .syncs
        .iter()
        .find(|s| s.file == gz_name)
        .unwrap_or_else(|| panic!("no sync record for {gz_name}: {:?}", report.syncs));
    assert_eq!(rec.origin, name);
    assert_eq!(rec.rows, out.rows);
    assert_eq!(rec.sha256.as_deref(), Some(sha.as_str()));
    assert!(
        report.machines.iter().any(|m| m.origin == name),
        "machine {name} missing from {:?}",
        report.machines
    );
    let events_after_install = event_count().expect("index event count");
    assert_eq!(events_after_install, out.rows, "the index should hold exactly the merged rows");

    // 6. An unchanged server is not re-delivered (the scheduled path's
    //    every-tick behavior): pull_once directly, minus the hub's timer.
    let record: ServerRecord = state.store.lock().unwrap().servers[0].clone();
    let pulled = block_on(fetch::pull_once(&handle, &record)).expect("second pull");
    assert!(!pulled.delivered, "an identical bundle must not be re-delivered");
    assert!(!pulled.uploaded, "the collector was already current on the server");
    assert_eq!(pulled.rows, out.rows);

    // 7. Live change: append one row inside the container over the dedicated
    //    key, pull again — the new bundle is delivered and merged.
    let appended = appended_row();
    assert!(!appended.contains('\''), "the append command quotes with single quotes");
    let mut conn = block_on(fetch::connect_dedicated(&record)).expect("dedicated connect");
    let append_cmd = format!("printf '%s\\n' '{appended}' >> ~/.claude/projects/e2e/session.jsonl");
    let ar = block_on(ssh::exec(&conn.handle, &append_cmd, ssh::EXEC_TIMEOUT)).expect("append exec");
    assert!(ar.ok(), "remote append failed: {}", ar.summary());
    block_on(ssh::sftp_close(&mut conn));

    let pulled2 = block_on(fetch::pull_once(&handle, &record)).expect("third pull");
    assert!(pulled2.delivered, "a changed remote bundle must be delivered");
    assert_eq!(pulled2.rows, out.rows + 1);
    let sha2 = usage_index::sha256_file(&gz).unwrap();
    assert_ne!(sha2, sha, "the bundle must have been rewritten");
    let merged = wait_for_sync(&handle, &gz_name, out.rows + 1, &sha2, Duration::from_secs(30))
        .unwrap_or_else(|| panic!("the engine never published the {}-row resync", out.rows + 1));
    assert_eq!(merged.origin, name);
    let events_after_resync = event_count().expect("index event count");
    assert_eq!(events_after_resync, out.rows + 1, "the appended row must reach the index");

    // 8. The removal path's remote half: collector gone, our key line gone,
    //    the door shut for the dedicated key.
    let key = keys::ensure_keypair().expect("dedicated key");
    let mut conn = block_on(fetch::connect_dedicated(&record)).expect("cleanup connect");
    let rm = block_on(ssh::exec(&conn.handle, remote::CMD_CLEANUP_BIN, ssh::EXEC_TIMEOUT)).unwrap();
    assert!(rm.ok(), "collector cleanup failed: {}", rm.summary());
    let remove_line = remote::cmd_pubkey_remove(&key.blob).unwrap();
    let pr = block_on(ssh::exec(&conn.handle, &remove_line, ssh::EXEC_TIMEOUT)).unwrap();
    assert!(pr.ok(), "authorized_keys cleanup failed: {}", pr.summary());
    let check = block_on(ssh::exec(
        &conn.handle,
        &format!(
            "if [ -e ~/.tokenme/bin/tokenme ]; then echo BIN_STILL_THERE; else echo BIN_GONE; fi; \
             grep -c -F '{blob}' ~/.ssh/authorized_keys || true",
            blob = key.blob
        ),
        ssh::EXEC_TIMEOUT,
    ))
    .unwrap();
    block_on(ssh::sftp_close(&mut conn));
    let check_out = check.stdout.clone();
    assert!(check_out.contains("BIN_GONE"), "collector still on the server: {check_out}");
    assert_eq!(check_out.split_whitespace().last(), Some("0"), "key line still present: {check_out}");

    let mut conn = block_on(ssh::connect(&host, port, Some(&fingerprint))).expect("closed-door connect");
    let denied = block_on(ssh::authenticate(&mut conn, &user, &ssh::Auth::Dedicated));
    block_on(ssh::sftp_close(&mut conn));
    assert!(denied.is_err(), "the dedicated key still authenticates after cleanup");

    println!(
        "e2e ok: {} rows merged +1 resync, cleanup verified; scratch profile kept at {} ({} ms elapsed)",
        out.rows,
        home.display(),
        started.elapsed().as_millis()
    );
}

fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}
