//! Tauri command surface for the server sheet. Thin by design: every command
//! reads managed state, validates through `remote.rs`, and hands off to the
//! hub or wizard.

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use super::hub::HubMsg;
use super::wizard::{self, InstallOutcome, InstallReq, ProbeOutcome, ProbeReq};
use super::{fetch, host_port_key, keys, remote, ssh, ServerView, ServersState, SshError};

fn servers(app: &AppHandle) -> Arc<ServersState> {
    app.state::<Arc<ServersState>>().inner().clone()
}

#[tauri::command]
pub fn get_servers(app: AppHandle) -> Vec<ServerView> {
    servers(&app).views(usage_core::report::now_ms())
}

/// The public half of the dedicated key pair, for users who want to inspect
/// or pre-provision it. Generated on first call.
#[tauri::command]
pub fn server_public_key() -> Result<serde_json::Value, SshError> {
    let info = keys::ensure_keypair()?;
    Ok(serde_json::json!({ "path": info.path, "line": info.line() }))
}

/// Connect + authenticate + read arch/hostname. Never returns Err: the
/// outcome object carries the classified failure so the wizard can show it
/// next to the right field.
#[tauri::command]
pub async fn server_probe(app: AppHandle, req: ProbeReq) -> ProbeOutcome {
    wizard::probe(&servers(&app), req).await
}

/// Run the nine-step install. Progress arrives on `server-install-progress`;
/// the returned outcome is the terminal state.
#[tauri::command]
pub async fn server_install(app: AppHandle, req: InstallReq) -> InstallOutcome {
    wizard::install(&app, &servers(&app), req).await
}

/// Close a wizard (user cancelled): drops the session, zeroizing any password.
#[tauri::command]
pub fn server_abort_session(app: AppHandle, session: u64) {
    wizard::abort(&servers(&app), session);
}

/// Pull this server now (the detail sheet's 立即同步).
#[tauri::command]
pub fn server_sync_now(app: AppHandle, id: u64) {
    servers(&app).hub_send(HubMsg::SyncNow(id));
}

#[derive(Debug, Deserialize)]
pub struct UpdateReq {
    pub id: u64,
    #[serde(default)]
    pub every_secs: Option<u64>,
    #[serde(default)]
    pub days: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[tauri::command]
pub fn server_update(app: AppHandle, req: UpdateReq) -> Result<Vec<ServerView>, SshError> {
    if let Some(every) = req.every_secs {
        if !remote::valid_every(every) {
            return Err(SshError::new("proto", "sync interval out of range"));
        }
    }
    if let Some(days) = req.days {
        if !remote::valid_days(days) {
            return Err(SshError::new("proto", "sync window out of range"));
        }
    }
    let state = servers(&app);
    let reenabled = state
        .mutate(|store| {
            let Some(rec) = store.servers.iter_mut().find(|r| r.id == req.id) else {
                return None;
            };
            let was = rec.enabled;
            if let Some(every) = req.every_secs {
                rec.every_secs = every;
            }
            if let Some(days) = req.days {
                rec.days = days;
            }
            if let Some(enabled) = req.enabled {
                rec.enabled = enabled;
            }
            Some(!was && rec.enabled)
        })
        .map_err(|e| SshError::new("local_io", e))?;
    if reenabled == Some(true) {
        if let Ok(mut due) = state.next_due.lock() {
            due.insert(req.id, Instant::now());
        }
    }
    state.hub_send(HubMsg::Changed);
    state.emit_updated(&app);
    Ok(state.views(usage_core::report::now_ms()))
}

#[derive(Debug, Serialize)]
pub struct RemoveOutcome {
    pub removed: bool,
    /// `Some(true)` cleanup ran and succeeded; `Some(false)` it was wanted but
    /// failed (`cleanup_error` says why); `None` it was not requested.
    pub cleaned: Option<bool>,
    pub cleanup_error: Option<SshError>,
}

/// Forget a server. The local side (stop pulling, drop config + fingerprint)
/// happens first and unconditionally; the optional remote cleanup — remove
/// the collector binary and exactly our own authorized_keys line — runs after
/// and reports its own result without undoing the local removal.
#[tauri::command]
pub async fn server_remove(app: AppHandle, id: u64, cleanup_remote: bool) -> RemoveOutcome {
    let state = servers(&app);
    let rec = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.servers.iter().find(|r| r.id == id).cloned());
    let Some(rec) = rec else {
        return RemoveOutcome { removed: false, cleaned: None, cleanup_error: None };
    };

    let _ = state.mutate(|store| {
        store.servers.retain(|r| r.id != id);
        // The fingerprint pin belongs to the host, not the account: only drop
        // it when no other registered server still uses this host:port.
        if !store.servers.iter().any(|r| r.host == rec.host && r.port == rec.port) {
            store.known_hosts.remove(&host_port_key(&rec.host, rec.port));
        }
    });
    if let Ok(mut due) = state.next_due.lock() {
        due.remove(&id);
    }
    state.emit_updated(&app);
    state.hub_send(HubMsg::Changed);

    if !cleanup_remote {
        return RemoveOutcome { removed: true, cleaned: None, cleanup_error: None };
    }

    let cleanup: Result<(), SshError> = async {
        let key = keys::ensure_keypair()?;
        let mut conn = fetch::connect_dedicated(&rec).await?;
        let rm = ssh::exec(&conn.handle, remote::CMD_CLEANUP_BIN, ssh::EXEC_TIMEOUT).await?;
        if !rm.ok() {
            return Err(SshError::new("remote_cmd", format!("removing the collector failed: {}", rm.summary())));
        }
        let remove_line = remote::cmd_pubkey_remove(&key.blob)?;
        let pr = ssh::exec(&conn.handle, &remove_line, ssh::EXEC_TIMEOUT).await?;
        if !pr.ok() {
            return Err(SshError::new("remote_cmd", format!("removing the key line failed: {}", pr.summary())));
        }
        ssh::sftp_close(&mut conn).await;
        Ok(())
    }
    .await;

    match cleanup {
        Ok(()) => RemoveOutcome { removed: true, cleaned: Some(true), cleanup_error: None },
        Err(e) => RemoveOutcome { removed: true, cleaned: Some(false), cleanup_error: Some(e) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal payloads the detail sheet's edits send (`types.ts`
    /// `ServerUpdateReq`): partial updates must deserialize with every
    /// unspecified knob left alone.
    #[test]
    fn frontend_update_payloads_deserialize() {
        let r: UpdateReq = serde_json::from_str(r#"{"id":3}"#).unwrap();
        assert_eq!(r.id, 3);
        assert!(r.every_secs.is_none() && r.days.is_none() && r.enabled.is_none());

        let r: UpdateReq =
            serde_json::from_str(r#"{"id":3,"every_secs":300,"days":7,"enabled":false}"#).unwrap();
        assert_eq!(r.every_secs, Some(300));
        assert_eq!(r.days, Some(7));
        assert_eq!(r.enabled, Some(false));
    }
}
