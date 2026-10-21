//! Server pull: register a Linux box over SSH from the panel, provision a
//! dedicated key and a static collector binary on it, then keep fetching its
//! `tokenme export` bundles on a schedule.
//!
//! Nothing runs on the server between pulls — no daemon, no systemd unit. The
//! panel connects out, runs fixed commands (`uname`, `mkdir`/`chmod`/`mv`,
//! `tokenme export`, `tokenme detect`) and copies back the same gzipped bundle
//! the manual `install-linux.sh --push` path writes. The bundle lands in
//! `~/tokenme-sync` and the engine merges it through the frozen import gate,
//! exactly like a hand-copied one.
//!
//! State split: `servers.json` under the panel's config dir holds what must
//! survive a restart (host, user, interval, window, pinned fingerprint — never
//! a password); scheduling state (next due time, in-flight pulls) lives in
//! memory only.

pub mod collector;
pub mod commands;
#[cfg(test)]
mod e2e;
pub mod fetch;
pub mod hub;
pub mod keys;
pub mod remote;
pub mod schedule;
pub mod ssh;
pub mod wizard;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::Emitter;

pub use ssh::{Auth, SshError};

/// Who the panel tells the UI it changed something.
pub const EV_SERVERS_UPDATED: &str = "servers-updated";
/// One wizard install step finished (full snapshot of the step list).
pub const EV_INSTALL_PROGRESS: &str = "server-install-progress";

const STORE_FILE: &str = "servers.json";
const STORE_VERSION: u32 = 1;
/// How many recent sync events per server the detail sheet can show.
const HISTORY_CAP: usize = 20;

/// The panel's own config directory — `usage_core` already pinned this for
/// `settings.json`; servers.json and the dedicated key pair live next to it.
pub fn panel_dir() -> Option<PathBuf> {
    usage_core::budget::settings_path().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

pub fn store_path() -> Option<PathBuf> {
    usage_core::budget::settings_path().map(|p| p.with_file_name(STORE_FILE))
}

/// `host:port` is the identity a host fingerprint is pinned against. Two
/// servers on the same host:port share the pin — the key belongs to the host,
/// not to the account.
pub fn host_port_key(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServersStore {
    pub version: u32,
    /// TOFU pins: `host:port` → `SHA256:…` as OpenSSH prints it.
    #[serde(default)]
    pub known_hosts: BTreeMap<String, String>,
    #[serde(default)]
    pub servers: Vec<ServerRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerRecord {
    pub id: u64,
    /// Becomes the bundle's `origin` on the server and the machine label in
    /// the panel; validated by `remote::valid_name` (and `origin_ok`).
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// The pinned host key fingerprint. Pulls refuse to run against a changed one.
    pub fingerprint: String,
    /// Sync cadence in seconds (300 / 900 / 3600 from the UI, any sane value here).
    pub every_secs: u64,
    /// Export window in days (7 / 30 / 90 from the UI).
    pub days: i64,
    pub enabled: bool,
    pub created_at_ms: i64,
    #[serde(default)]
    pub last_ok_ms: Option<i64>,
    #[serde(default)]
    pub last_error: Option<SshError>,
    #[serde(default)]
    pub fail_count: u32,
    #[serde(default)]
    pub last_rows: u64,
    #[serde(default)]
    pub last_took_ms: i64,
    /// Tool display names last reported by the server's `tokenme detect`.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub history: Vec<SyncEvent>,
}

impl ServerRecord {
    pub fn url(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }

    /// One completed pull attempt (success or failure) for the detail sheet.
    pub fn push_history(&mut self, ev: SyncEvent) {
        self.history.push(ev);
        let len = self.history.len();
        if len > HISTORY_CAP {
            self.history.drain(..len - HISTORY_CAP);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEvent {
    pub at_ms: i64,
    pub rows: u64,
    pub took_ms: i64,
    pub ok: bool,
}

/// The JSON-safe face of a record: live scheduling facts (status, next due)
/// merged in for the UI. The frontend never sees `ServersStore` itself.
#[derive(Debug, Clone, Serialize)]
pub struct ServerView {
    pub id: u64,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub fingerprint: String,
    pub every_secs: u64,
    pub days: i64,
    pub enabled: bool,
    /// `ok` | `warn` | `error` | `syncing` | `none`.
    pub status: String,
    pub last_ok_ms: Option<i64>,
    pub last_error: Option<SshError>,
    pub last_rows: u64,
    pub last_took_ms: i64,
    pub next_due_ms: Option<i64>,
    pub tools: Vec<String>,
    pub history: Vec<SyncEvent>,
}

/// Everything the commands, the hub and the wizard share. One `Arc` lives in
/// Tauri managed state; the hub thread holds a clone.
pub struct ServersState {
    pub store: Mutex<ServersStore>,
    /// Server ids whose pull is in flight right now (drives the pulsing dot).
    pub pulling: Mutex<BTreeSet<u64>>,
    /// When each enabled server should be pulled next (monotonic).
    pub next_due: Mutex<BTreeMap<u64, Instant>>,
    /// Open wizard sessions, keyed by id. Holds the one-time credentials until
    /// the wizard succeeds or is aborted.
    pub wizards: Mutex<BTreeMap<u64, wizard::WizardSession>>,
    next_id: AtomicU64,
    /// Where the hub listens; set once by `hub::start`.
    hub_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<hub::HubMsg>>>,
}

impl ServersState {
    pub fn new() -> Self {
        Self::from_store(load_store())
    }

    /// Builds the state around an in-memory store — `new` is this plus the disk
    /// read, and tests use it directly. Record ids survive a restart, so the
    /// counter starts past the highest one: resetting it to 1 would hand the
    /// next install an id an existing record already owns (two servers sharing
    /// an id means the detail sheet shows the wrong one, pulls run against the
    /// wrong host, and removing one deletes both).
    pub fn from_store(store: ServersStore) -> Self {
        let next_id = store
            .servers
            .iter()
            .map(|r| r.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        Self {
            store: Mutex::new(store),
            pulling: Mutex::new(BTreeSet::new()),
            next_due: Mutex::new(BTreeMap::new()),
            wizards: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(next_id),
            hub_tx: Mutex::new(None),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn set_hub_tx(&self, tx: tokio::sync::mpsc::UnboundedSender<hub::HubMsg>) {
        if let Ok(mut slot) = self.hub_tx.lock() {
            *slot = Some(tx);
        }
    }

    pub fn hub_send(&self, msg: hub::HubMsg) {
        if let Ok(slot) = self.hub_tx.lock() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send(msg);
            }
        }
    }

    pub fn views(&self, now_ms: i64) -> Vec<ServerView> {
        let store = match self.store.lock() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let pulling = self.pulling.lock().map(|p| p.clone()).unwrap_or_default();
        let due = self.next_due.lock().map(|d| d.clone()).unwrap_or_default();
        let now = Instant::now();
        store
            .servers
            .iter()
            .map(|rec| {
                let status = schedule::derive_status(rec, pulling.contains(&rec.id), now_ms);
                let next_due_ms = due.get(&rec.id).map(|t| {
                    let ms = t.saturating_duration_since(now).as_millis() as i64;
                    now_ms.saturating_add(ms)
                });
                ServerView {
                    id: rec.id,
                    name: rec.name.clone(),
                    host: rec.host.clone(),
                    port: rec.port,
                    user: rec.user.clone(),
                    fingerprint: rec.fingerprint.clone(),
                    every_secs: rec.every_secs,
                    days: rec.days,
                    enabled: rec.enabled,
                    status: status.to_string(),
                    last_ok_ms: rec.last_ok_ms,
                    last_error: rec.last_error.clone(),
                    last_rows: rec.last_rows,
                    last_took_ms: rec.last_took_ms,
                    next_due_ms,
                    tools: rec.tools.clone(),
                    history: rec.history.clone(),
                }
            })
            .collect()
    }

    /// Reads the store under lock, runs `f`, writes it back and persists.
    pub fn mutate<T>(&self, f: impl FnOnce(&mut ServersStore) -> T) -> Result<T, String> {
        let mut store = self.store.lock().map_err(|_| "servers busy".to_string())?;
        let out = f(&mut store);
        save_store(&store)?;
        Ok(out)
    }

    pub fn emit_updated<R: tauri::Runtime>(&self, app: &tauri::AppHandle<R>) {
        let now_ms = usage_core::report::now_ms();
        let _ = app.emit(EV_SERVERS_UPDATED, self.views(now_ms));
    }
}

impl Default for ServersState {
    fn default() -> Self {
        Self::new()
    }
}

/// `servers.json` — atomic tmp+rename, mode 0600 (it names hosts and users).
pub fn save_store(store: &ServersStore) -> Result<(), String> {
    let Some(path) = store_path() else {
        return Err("no config directory for servers.json".into());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        serde_json::to_writer_pretty(&mut f, store)
            .map_err(|e| format!("cannot serialize servers.json: {e}"))?;
        use std::io::Write;
        f.flush().map_err(|e| format!("cannot flush servers.json: {e}"))?;
    }
    usage_core::replace_file(&tmp, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

pub fn load_store() -> ServersStore {
    let Some(path) = store_path() else {
        return ServersStore { version: STORE_VERSION, ..Default::default() };
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return ServersStore { version: STORE_VERSION, ..Default::default() };
    };
    match serde_json::from_str::<ServersStore>(&raw) {
        Ok(mut store) => {
            store.version = STORE_VERSION;
            // An older build handed a re-run install an id that an existing
            // record already owned. Repair it on load so the damage stops
            // here instead of hiding behind a second row with the same id.
            if repair_ids(&mut store) {
                crate::logging::error("servers: servers.json held duplicate ids; renumbered the copies");
                if let Err(e) = save_store(&store) {
                    crate::logging::error(&format!("servers: cannot persist the renumbered servers.json: {e}"));
                }
            }
            store
        }
        Err(e) => {
            // Never let a corrupt file wipe the registration list silently:
            // keep the file, start empty, and say so in the log.
            crate::logging::error(&format!("servers: {} is unreadable ({e}); starting empty", path.display()));
            ServersStore { version: STORE_VERSION, ..Default::default() }
        }
    }
}

/// Gives every record a unique, non-zero id — the first duplicate (or `0`)
/// keeps the id it has, later ones move past the highest id in the file.
/// Returns whether anything moved.
fn repair_ids(store: &mut ServersStore) -> bool {
    let mut seen = BTreeSet::new();
    let mut next = store
        .servers
        .iter()
        .map(|r| r.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    let mut changed = false;
    for rec in store.servers.iter_mut() {
        if rec.id != 0 && seen.insert(rec.id) {
            continue;
        }
        rec.id = next;
        next = next.saturating_add(1);
        seen.insert(rec.id);
        changed = true;
    }
    changed
}

/// Managed-state constructor for `lib.rs`.
pub fn state() -> Arc<ServersState> {
    Arc::new(ServersState::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip_and_mode() {
        let dir = std::env::temp_dir().join(format!("tokenme-servers-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // save_store writes to the real config path; exercise the serde shape
        // directly instead so the test never touches user data.
        let store = ServersStore {
            version: STORE_VERSION,
            known_hosts: BTreeMap::from([("box:22".to_string(), "SHA256:abc".to_string())]),
            servers: vec![ServerRecord {
                id: 7,
                name: "box".into(),
                host: "box.local".into(),
                port: 22,
                user: "root".into(),
                fingerprint: "SHA256:abc".into(),
                every_secs: 900,
                days: 30,
                enabled: true,
                created_at_ms: 1,
                last_ok_ms: Some(2),
                last_error: None,
                fail_count: 0,
                last_rows: 12,
                last_took_ms: 800,
                tools: vec!["Claude Code".into()],
                history: vec![],
            }],
        };
        let json = serde_json::to_string(&store).unwrap();
        let back: ServersStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.servers.len(), 1);
        assert_eq!(back.servers[0].name, "box");
        assert_eq!(back.known_hosts.get("box:22").unwrap(), "SHA256:abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_port_key_shape() {
        assert_eq!(host_port_key("example.com", 22), "example.com:22");
        assert_eq!(host_port_key("192.0.2.4", 2222), "192.0.2.4:2222");
    }

    #[test]
    fn history_is_capped() {
        let mut rec = ServerRecord {
            id: 1,
            name: "n".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            fingerprint: "f".into(),
            every_secs: 60,
            days: 7,
            enabled: true,
            created_at_ms: 0,
            last_ok_ms: None,
            last_error: None,
            fail_count: 0,
            last_rows: 0,
            last_took_ms: 0,
            tools: vec![],
            history: vec![],
        };
        for i in 0..(HISTORY_CAP + 5) {
            rec.push_history(SyncEvent { at_ms: i as i64, rows: 1, took_ms: 1, ok: true });
        }
        assert_eq!(rec.history.len(), HISTORY_CAP);
        // The oldest fall off the front, not the newest.
        assert_eq!(rec.history.first().unwrap().at_ms, 5);
    }

    /// Record ids are persisted; the allocator must continue past them. It
    /// used to restart at 1 on every launch, so the next install reissued an
    /// existing server's id — the sheet then showed one machine's details for
    /// the other, the hub pulled whichever record came first, and removing one
    /// deleted both.
    #[test]
    fn next_id_continues_past_persisted_ids() {
        let store = ServersStore {
            version: STORE_VERSION,
            known_hosts: BTreeMap::new(),
            servers: vec![record(2, "a"), record(7, "b")],
        };
        let state = ServersState::from_store(store);
        assert_eq!(state.next_id(), 8);
        assert_eq!(state.next_id(), 9);
    }

    #[test]
    fn next_id_starts_at_one_for_an_empty_store() {
        let state = ServersState::from_store(ServersStore {
            version: STORE_VERSION,
            ..Default::default()
        });
        assert_eq!(state.next_id(), 1);
    }

    /// A store written by the old allocator can already hold one id twice;
    /// loading renumbers the copy (and is a no-op on healthy files).
    #[test]
    fn duplicate_ids_are_renumbered() {
        let mut store = ServersStore {
            version: STORE_VERSION,
            known_hosts: BTreeMap::new(),
            servers: vec![record(2, "a"), record(2, "b"), record(7, "c")],
        };
        assert!(repair_ids(&mut store));
        let ids: Vec<u64> = store.servers.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![2, 8, 7]);
        assert!(!repair_ids(&mut store), "a repaired store needs no second pass");
    }

    #[test]
    fn a_zero_id_is_renumbered() {
        let mut store = ServersStore {
            version: STORE_VERSION,
            known_hosts: BTreeMap::new(),
            servers: vec![record(0, "a")],
        };
        assert!(repair_ids(&mut store));
        assert_eq!(store.servers[0].id, 1);
    }

    fn record(id: u64, name: &str) -> ServerRecord {
        ServerRecord {
            id,
            name: name.into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            fingerprint: "f".into(),
            every_secs: 900,
            days: 30,
            enabled: true,
            created_at_ms: 0,
            last_ok_ms: None,
            last_error: None,
            fail_count: 0,
            last_rows: 0,
            last_took_ms: 0,
            tools: vec![],
            history: vec![],
        }
    }
}
