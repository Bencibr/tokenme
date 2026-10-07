//! The pull pipeline: prepare the server, run its export, bring the bundle
//! into `~/tokenme-sync` through the same gates a manual copy would face.
//!
//! Shared by the scheduled hub and the install wizard — the wizard runs
//! exactly this code for its last two steps, so "it worked during install"
//! and "it works on the schedule" cannot drift apart.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use russh_sftp::client::SftpSession;
use tauri::{AppHandle, Manager, Runtime};

use usage_index::SyncManifest;

use super::ssh::{self, Conn, ExecOut, SshError};
use super::{collector, remote, ServerRecord};

/// The outcome of one pull.
#[derive(Debug, Clone)]
pub struct Pulled {
    pub rows: u64,
    pub took_ms: i64,
    pub uploaded: bool,
    /// `true` when a bundle was written (and the engine woken); `false` when
    /// the server's bundle was already here, byte for byte.
    pub delivered: bool,
}

/// Connect + authenticate with the dedicated key. Shared entry for hub pulls
/// and post-install checks.
pub async fn connect_dedicated(rec: &ServerRecord) -> Result<Conn, SshError> {
    let mut conn = ssh::connect(&rec.host, rec.port, Some(&rec.fingerprint)).await?;
    ssh::authenticate(&mut conn, &rec.user, &super::Auth::Dedicated).await?;
    Ok(conn)
}

/// One full pull: provision if needed → export → fetch (or discover it is
/// unchanged) → wake the engine. The caller records success/failure.
pub async fn pull_once<R: Runtime>(app: &AppHandle<R>, rec: &ServerRecord) -> Result<Pulled, SshError> {
    let started = Instant::now();
    let mut conn = connect_dedicated(rec).await?;
    let sn = ssh::sftp(&conn.handle).await?;

    let uploaded = ensure_collector(app, &conn, &sn).await?;
    run_export(&conn, &rec.name, rec.days).await?;
    let fetched = fetch_bundle(&sn, &rec.name).await;
    ssh::sftp_close(&mut conn).await;
    let fetched = fetched?;

    if fetched.written {
        wake_engine(app);
    }
    Ok(Pulled {
        rows: fetched.rows,
        took_ms: started.elapsed().as_millis() as i64,
        uploaded,
        delivered: fetched.written,
    })
}

/// Makes sure `~/.tokenme/bin/tokenme` exists and is *this* build, uploading
/// the bundled static binary when the sha256 differs. Returns whether an
/// upload happened.
pub async fn ensure_collector<R: Runtime>(app: &AppHandle<R>, conn: &Conn, sn: &SftpSession) -> Result<bool, SshError> {
    let mk = ssh::exec(&conn.handle, remote::CMD_MKDIR, ssh::EXEC_TIMEOUT).await?;
    if !mk.ok() {
        return Err(SshError::new("remote_cmd", format!("cannot prepare ~/.tokenme: {}", mk.summary())));
    }

    let uname = ssh::exec(&conn.handle, remote::CMD_UNAME, ssh::PROBE_EXEC_TIMEOUT).await?;
    let arch_raw = uname.stdout.trim();
    let arch = collector::arch_target(arch_raw).ok_or_else(|| {
        SshError::new(
            "unsupported_arch",
            format!(
                "architecture {arch_raw:?} has no bundled collector — use scripts/install-linux.sh --push on this machine"
            ),
        )
    })?;
    let (_fname, bytes, sha) = collector::load(app, arch)?;

    let current = ssh::exec(&conn.handle, remote::CMD_SHA_BIN, ssh::EXEC_TIMEOUT).await?;
    if current.stdout.trim().eq_ignore_ascii_case(&sha) {
        return Ok(false);
    }

    ssh::sftp_write(sn, ".tokenme/bin/tokenme.tmp", &bytes).await?;
    let uploaded_sha = ssh::exec(&conn.handle, remote::CMD_SHA_TMP, ssh::EXEC_TIMEOUT).await?;
    let seen = uploaded_sha.stdout.trim();
    if !seen.eq_ignore_ascii_case(&sha) {
        let saw = if seen.is_empty() { "no digest" } else { seen };
        return Err(SshError::new("sftp", format!("upload verification failed: local {sha}, server saw {saw}")));
    }
    let installed = ssh::exec(&conn.handle, remote::CMD_INSTALL_BIN, ssh::EXEC_TIMEOUT).await?;
    if !installed.ok() {
        return Err(SshError::new(
            "remote_cmd",
            format!("installing the collector failed: {}", installed.summary()),
        ));
    }
    Ok(true)
}

/// The collection itself: one `tokenme export` over the configured window.
pub async fn run_export(conn: &Conn, name: &str, days: i64) -> Result<ExecOut, SshError> {
    let cmd = remote::cmd_export(name, days)?;
    let out = ssh::exec(&conn.handle, &cmd, ssh::EXPORT_TIMEOUT).await?;
    if !out.ok() {
        return Err(SshError::new("remote_cmd", format!("tokenme export failed: {}", out.summary())));
    }
    Ok(out)
}

pub struct Fetched {
    pub rows: u64,
    /// `false` when the identical bundle was already here.
    pub written: bool,
}

/// Reads the manifest, and unless the same sha is already on disk, downloads
/// the bundle, verifies the sha the manifest declares, and lands `.jsonl.gz`
/// + manifest into `~/tokenme-sync` via tmp + rename (mode 0600).
pub async fn fetch_bundle(sn: &SftpSession, name: &str) -> Result<Fetched, SshError> {
    let manifest_rel = format!(".tokenme/sync/tokenme-{name}.manifest.json");
    let manifest_bytes = ssh::sftp_read(sn, &manifest_rel, 1 << 20).await?;
    let manifest: SyncManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| SshError::new("proto", format!("the server's manifest is not readable: {e}")))?;
    if manifest.format != usage_index::SYNC_FORMAT {
        return Err(SshError::new(
            "proto",
            format!("bundle format {:?} is not supported (want {})", manifest.format, usage_index::SYNC_FORMAT),
        ));
    }
    if manifest.window_lo_ms >= manifest.window_hi_ms {
        return Err(SshError::new("proto", format!("bundle window is empty ({}..{})", manifest.window_lo_ms, manifest.window_hi_ms)));
    }

    let dir = usage_index::default_sync_dir()
        .ok_or_else(|| SshError::new("local_io", "no home directory for ~/tokenme-sync"))?;
    std::fs::create_dir_all(&dir).map_err(|e| SshError::new("local_io", format!("cannot create {}: {e}", dir.display())))?;

    let gz_name = format!("tokenme-{name}.jsonl.gz");
    let gz_path = dir.join(&gz_name);
    if gz_path.exists() {
        if let Ok(local_sha) = usage_index::sha256_file(&gz_path) {
            if local_sha.eq_ignore_ascii_case(&manifest.sha256) {
                return Ok(Fetched { rows: manifest.rows, written: false });
            }
        }
    }

    let gz_rel = format!(".tokenme/sync/{gz_name}");
    let bytes = ssh::sftp_read(sn, &gz_rel, ssh::BUNDLE_CAP).await?;
    let got = collector::sha_of(&bytes);
    if !got.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(SshError::new(
            "proto",
            format!("bundle sha256 mismatch: manifest says {}, downloaded {got}", manifest.sha256),
        ));
    }

    // Bundle first, manifest second: the engine only looks at `.jsonl.gz`, and
    // it re-checks the manifest's presence itself, so a crash between the two
    // writes leaves a retryable state, never a half-truth.
    write_private(&dir, &gz_name, &bytes)?;
    let manifest_name = format!("tokenme-{name}.manifest.json");
    write_private(&dir, &manifest_name, &manifest_bytes)?;

    Ok(Fetched { rows: manifest.rows, written: true })
}

/// Detected tool display names from the server's own `tokenme detect`.
pub async fn detect_tools(conn: &Conn) -> Result<Vec<String>, SshError> {
    let out = ssh::exec(&conn.handle, remote::CMD_DETECT, ssh::EXEC_TIMEOUT).await?;
    let parsed: Vec<serde_json::Value> = serde_json::from_str(out.stdout.trim())
        .map_err(|e| SshError::new("proto", format!("detect output is not JSON: {e}")))?;
    let mut tools = Vec::new();
    for item in parsed {
        if item.get("detected").and_then(|v| v.as_bool()).unwrap_or(false) {
            let label = item
                .get("display")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("id").and_then(|v| v.as_str()))
                .unwrap_or("?")
                .to_string();
            tools.push(label);
        }
    }
    Ok(tools)
}

/// Nudges the engine: the fresh bundle is in `~/tokenme-sync`, merge it now
/// rather than on the next cadence. The merge itself is the frozen import
/// gate — a bad bundle is rejected there, not here.
pub fn wake_engine<R: Runtime>(app: &AppHandle<R>) {
    let Some(dir) = usage_index::default_sync_dir() else { return };
    let _ = app.state::<crate::engine::EngineChannel>().0.send(crate::engine::Msg::Wake(dir));
}

/// Waits (up to `timeout`) for the engine to publish the merge of `gz_name`.
/// `None` means "still written to ~/tokenme-sync, merge pending" — the honest
/// fallback the wizard UI shows.
pub async fn await_merge<R: Runtime>(app: &AppHandle<R>, gz_name: &str, timeout: Duration) -> Option<u64> {
    let started = Instant::now();
    loop {
        if let Some(report) = app.state::<crate::engine::Shared>().report() {
            if let Some(rec) = report.syncs.iter().find(|s| s.file == gz_name) {
                return Some(rec.rows);
            }
        }
        if started.elapsed() >= timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// tmp + rename with mode 0600 — the same discipline `tokenme export` uses,
/// because these bundles name sessions, projects and log paths.
fn write_private(dir: &Path, name: &str, data: &[u8]) -> Result<(), SshError> {
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| SshError::new("local_io", format!("cannot write {}: {e}", tmp.display())))?;
        f.write_all(data)
            .and_then(|_| f.flush())
            .map_err(|e| SshError::new("local_io", format!("cannot write {}: {e}", tmp.display())))?;
    }
    usage_core::replace_file(&tmp, &path)
        .map_err(|e| SshError::new("local_io", format!("cannot store {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_write_is_atomic_and_0600() {
        let dir = std::env::temp_dir().join(format!("tokenme-fetch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_private(&dir, "bundle.jsonl.gz", b"hello").unwrap();
        let path = dir.join("bundle.jsonl.gz");
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(!dir.join("bundle.jsonl.gz.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Overwrite keeps working (rename replaces).
        write_private(&dir, "bundle.jsonl.gz", b"world").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_shape_roundtrips() {
        // The fields `fetch_bundle` depends on, built as the export writes them.
        let raw = serde_json::json!({
            "format": "tokenme-sync/1",
            "origin": "box",
            "schema_version": "1",
            "window_lo_ms": 1_000,
            "window_hi_ms": 2_000,
            "rows": 12,
            "sums": {"in_tok": 0, "cc_tok": 0, "cr_tok": 0, "out_tok": 0, "reason_tok": 0, "credits": 0},
            "tools": {},
            "sha256": "abc",
            "generated_at_ms": 3
        });
        let manifest: SyncManifest = serde_json::from_value(raw).unwrap();
        assert_eq!(manifest.format, usage_index::SYNC_FORMAT);
        assert!(manifest.window_lo_ms < manifest.window_hi_ms);
        assert_eq!(manifest.rows, 12);
    }
}
