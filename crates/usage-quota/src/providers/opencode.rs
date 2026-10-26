//! OpenCode Go quota: the official subscription's three usage windows.
//!
//! The endpoint is a real provider statistics API (`rolling`, `weekly`, and
//! `monthly`), but its percentages are usage against monthly dollar limits.
//! They must not be treated as a uniform Token percentage when several models
//! are used in one cycle.
//!
//! The credential is the one the user's own OpenCode install holds for the
//! official Go provider, read in this order: `OPENCODE_API_KEY`, the V2
//! credential store, the official provider block in `opencode.json`, and
//! finally the legacy `auth.json`. No credential, no row.
//!
//! The request shape is `GET https://opencode.ai/zen/go/v1/usage` with
//! `Authorization: Bearer <key>` + `Accept: application/json` — identical in both
//! independent reference implementations (`slkiser/opencode-quota:src/lib/opencode-go.ts:5,153-160`,
//! `nicezic/lean-quota-monitor:docs/provider-acquisition-spec.md` "OpenCode Go").
//! A `User-Agent` is optional (the endpoint answers without it).
//!
//! ## Where V2 keeps the credential (and why a file-only lookup missed it)
//!
//! Measured on this machine, 2026-10-14: the OpenCode Console login the desktop
//! and the CLI both use is a row of `credential` in OpenCode's own database,
//! `~/.local/share/opencode/opencode.db` —
//!
//! ```text
//! integration_id  label    active  value
//! opencode        Default  1       {"type":"oauth","methodID":"device",
//!                                   "refresh":…,"access":…,"expires":…,
//!                                   "metadata":{"server","accountID","email","orgID",…}}
//! ```
//!
//! `auth.json` still exists, but it is the **legacy** file the V2 CLI imports
//! once (`import_legacy_credentials`) and no longer writes: on this machine it
//! holds two local-relay keys and nothing official. A probe that read only that
//! file therefore answered nothing while the account held a live Go
//! subscription — the same row's `access` sent to `/zen/go/v1/usage` answers
//! `200` with the three windows — and the relay key it fell back to was
//! *sent to the vendor*, which answers
//! `401 {"type":"error","error":{"type":"AuthError","message":"Unauthorized"}}`.
//! `auth.json` is now read for exactly the ids OpenCode writes a Go key under,
//! never for "any api entry" it happens to carry.
//!
//! A device-flow `access` token is used as-is and never refreshed here: that row
//! belongs to OpenCode, and two writers rotating one refresh token is how a
//! login gets broken. An expired token is skipped, named in the host's quota log
//! (`probes-empty:`), and picked up again the moment OpenCode has refreshed it.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http::get_json_any_status;
use crate::notes::note;
use crate::QuotaProbe;

const ENDPOINT: &str = "https://opencode.ai/zen/go/v1/usage";
const OFFICIAL_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));

/// The provider ids OpenCode writes for a Go key (`opencode auth login -p
/// opencode-go`), in preference order, with `opencode` kept as the legacy alias
/// (`slkiser/opencode-quota:src/lib/opencode-go-auth.ts:12`) — and the id the V2
/// store's OpenCode Console login carries (measured 2026-10-14).
const AUTH_IDS: &[&str] = &["opencode-go", "opencode"];

/// The store V2 moved credentials into, joined on OpenCode's data directory.
const DB_FILE: &str = "opencode.db";
/// The legacy credential file, kept for installs that never ran V2.
const AUTH_FILE: &str = "auth.json";

/// `(json key, label, window)` — `rolling` is the 5-hour window, not a day:
/// `$12 rolling 5h / $30 weekly / $60 monthly`
/// (`nicezic/lean-quota-monitor:docs/provider-acquisition-spec.md:568-576`,
/// `opgginc/opencode-bar:CopilotMonitor/CopilotMonitor/Providers/OpenCodeGoProvider.swift:94-99`).
const WINDOWS: &[(&str, &str, i64)] = &[("rolling", "5 小时", 300), ("weekly", "7 天", 10_080), ("monthly", "月", 43_200)];

pub struct OpenCodeQuota;

impl QuotaProbe for OpenCodeQuota {
    fn tool(&self) -> &'static str {
        "opencode"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    let key = match api_key() {
        Ok(key) => key,
        Err(why) => {
            note("opencode", why);
            return None;
        }
    };
    let Some((status, body)) = get_json_any_status(
        ENDPOINT,
        &[
            ("authorization", &format!("Bearer {key}")),
            ("accept", "application/json"),
            ("user-agent", USER_AGENT),
        ],
    ) else {
        note("opencode", "request failed");
        return None;
    };
    if !(200u16..300).contains(&status) {
        note("opencode", refusal(status, &body));
        return None;
    }
    let mut samples = samples_from(&body);
    for sample in &mut samples {
        sample.id = Some(super::account_window_id(&key, sample.window_minutes));
    }
    Some(samples)
}

/// The vendor's own word for a refusal, short enough for one log line. The
/// error type is what separates "this account has no Go allowance"
/// (`EntitlementError`) from "this is not an OpenCode credential at all"
/// (`AuthError`, the shape a relay key draws).
fn refusal(status: u16, body: &Value) -> String {
    match body.pointer("/error/type").and_then(Value::as_str) {
        Some(kind) => format!("HTTP {status} {kind}"),
        None => format!("HTTP {status}"),
    }
}

/// `{"usage":{"rolling":{"status","percent","resetsAt"}, …}}`. `percent` is
/// already used-percent (0-100), so it is never inverted. A window whose `status`
/// is `rate-limited` is exhausted whatever `percent` says
/// (`slkiser/opencode-quota:src/lib/opencode-go.ts:106-112`); the other windows in
/// the same response still count.
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    // Some deployments answer with the buckets at the top level instead.
    let usage = body.get("usage").unwrap_or(body);
    let mut out = Vec::new();
    for (bucket, label, minutes) in WINDOWS.iter().copied() {
        let Some(entry) = usage.get(bucket).and_then(Value::as_object) else { continue };
        let Some(percent) = entry.get("percent").and_then(Value::as_f64) else { continue };
        let exhausted = entry.get("status").and_then(Value::as_str) == Some("rate-limited");
        let used_percent = if exhausted { 100.0 } else { percent.clamp(0.0, 100.0) };
        out.push(QuotaSample {
            used_percent,
            window_minutes: minutes,
            resets_at_ms: entry.get("resetsAt").and_then(Value::as_str).and_then(parse_ts_ms).unwrap_or(0),
            label: Some(label.to_string()),
            id: None,
        });
    }
    out.sort_by_key(|s| s.window_minutes);
    out
}

/// The credential, or the reason there is none — the reason is what the host's
/// quota log prints, because "the row is simply absent" is not a thing a user
/// can act on.
fn api_key() -> Result<String, &'static str> {
    let roots = data_roots();
    let db = first_in_roots(&roots, DB_FILE);
    let legacy = first_in_roots(&roots, AUTH_FILE);
    let env = std::env::var("OPENCODE_API_KEY").ok().filter(|k| !k.is_empty());
    resolve(env, db.as_deref(), &official_config_paths(), legacy.as_deref())
}

/// The source order, over already-located inputs: the explicit override, the
/// store the tool itself writes, the official provider block, the legacy file.
fn resolve(
    env: Option<String>,
    db: Option<&Path>,
    configs: &[PathBuf],
    legacy: Option<&Path>,
) -> Result<String, &'static str> {
    if let Some(key) = env {
        return Ok(key);
    }
    let mut why = "no official credential";
    if let Some(path) = db {
        match db_key(path) {
            Credential::Token(key) => return Ok(key),
            Credential::Expired => why = "credential expired",
            Credential::Unreadable => why = "opencode.db unreadable",
            Credential::Absent => {}
        }
    }
    for path in configs {
        if let Some(key) = config_key(path) {
            return Ok(key);
        }
    }
    if let Some(path) = legacy {
        if let Some(key) = auth_key(path) {
            return Ok(key);
        }
    }
    Err(why)
}

/// The roots an OpenCode install keeps its store in, in the product's own
/// priority (`usage-adapter-opencode`'s `paths::data_dirs_for`): the documented
/// override pins exactly the roots it names, else the XDG root the V2 CLI
/// resolves through (`$XDG_DATA_HOME/opencode`), then the home default.
fn data_roots() -> Vec<PathBuf> {
    if let Ok(value) = std::env::var("OPENCODE_DATA_DIR") {
        let pinned: Vec<PathBuf> = value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if !pinned.is_empty() {
            return pinned;
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        // A relative XDG root is undefined by the spec, so it is ignored
        // rather than resolved against an arbitrary working directory.
        if xdg.is_absolute() {
            roots.push(xdg.join("opencode"));
        }
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".local").join("share").join("opencode"));
    }
    roots
}

/// The first root holding `name`: a machine that relocated once can keep its
/// store and its legacy file in different roots, and the store is what matters.
fn first_in_roots(roots: &[PathBuf], name: &str) -> Option<PathBuf> {
    roots.iter().map(|root| root.join(name)).find(|path| path.is_file())
}

/// What the V2 store had to say about a credential.
enum Credential {
    Token(String),
    /// Carried an expiry that is behind us. Never refreshed here (see the
    /// module docs); OpenCode owns the row.
    Expired,
    /// The file exists but no rung of the ladder could read it.
    Unreadable,
    /// No store, no `credential` table, or no row for these ids — a v1 install.
    Absent,
}

/// `credential` in OpenCode's own database: V2's store, one row per integration
/// and account. `active` marks the one `opencode auth switch` selected; a row
/// with no `active` is a migrated legacy entry.
fn db_key(path: &Path) -> Credential {
    let Some(conn) = open_readonly(path) else { return Credential::Unreadable };
    let ids: Vec<String> = AUTH_IDS.iter().map(|id| format!("'{id}'")).collect();
    let sql = format!(
        "SELECT value FROM credential WHERE integration_id IN ({}) \
         ORDER BY coalesce(active, 0) DESC, time_created DESC",
        ids.join(",")
    );
    let Ok(mut stmt) = conn.prepare(&sql) else { return Credential::Absent };
    let Ok(mut rows) = stmt.query([]) else { return Credential::Absent };
    let now = crate::cache::now_ms();
    let mut expired = false;
    while let Ok(Some(row)) = rows.next() {
        let Ok(raw) = row.get::<_, String>(0) else { continue };
        match token_from_value(&raw, now) {
            Ok(key) => return Credential::Token(key),
            Err(true) => expired = true,
            Err(false) => {}
        }
    }
    if expired {
        Credential::Expired
    } else {
        Credential::Absent
    }
}

/// One `credential.value` blob as the bearer this probe can send: `access` for
/// an account login (`type:"oauth"`), `key` for a pasted key (`type:"api"`, the
/// same shape `auth.json` carries). `Err(true)` is specifically "it carried an
/// expiry and that expiry is past"; `Err(false)` is "a shape this probe cannot
/// use".
fn token_from_value(raw: &str, now_ms: i64) -> Result<String, bool> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else { return Err(false) };
    if value.get("expires").and_then(Value::as_i64).is_some_and(|at| at <= now_ms) {
        return Err(true);
    }
    let field = match value.get("type").and_then(Value::as_str) {
        Some("oauth") => "access",
        Some("api") => "key",
        _ => return Err(false),
    };
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .ok_or(false)
}

/// The store opened without a write, on the same three rungs the adapter walks
/// (`usage-adapter-opencode` `paths::open_readonly`): `mode=ro&nolock=1` takes
/// no lock but cannot read a live WAL store, plain `mode=ro` reads the running
/// app's store, and `mode=rw` exists only so SQLite may rebuild a `-shm` the
/// app deleted on a clean exit — fenced with `query_only` the moment it opens,
/// so this probe can never write the user's session store.
fn open_readonly(path: &Path) -> Option<Connection> {
    const LADDER: [(&str, OpenFlags, bool); 3] = [
        ("mode=ro&nolock=1", OpenFlags::SQLITE_OPEN_READ_ONLY, false),
        ("mode=ro", OpenFlags::SQLITE_OPEN_READ_ONLY, false),
        ("mode=rw", OpenFlags::SQLITE_OPEN_READ_WRITE, true),
    ];
    for (query, flags, fence) in LADDER {
        let uri = format!("file:{}?{query}", encode_uri_path(path));
        let Ok(conn) = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_URI | flags) else {
            continue;
        };
        if fence {
            let _ = conn.execute_batch("PRAGMA query_only = 1;");
        }
        let _ = conn.busy_timeout(std::time::Duration::from_millis(250));
        // Schema presence is the usability proof: both documented failure modes
        // are an *open that succeeds* followed by a read that fails, so a rung
        // that cannot answer this question falls through to the next.
        if conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(())).is_ok() {
            return Some(conn);
        }
    }
    None
}

/// A filesystem path as a SQLite URI path: everything outside the unreserved
/// set is percent-encoded, so a store under a directory with a space, `?` or
/// `#` still opens (the same encoder the adapter carries).
fn encode_uri_path(path: &Path) -> String {
    let mut out = String::new();
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn official_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(root).join("opencode/opencode.json"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".config/opencode/opencode.json"));
    }
    if let Some(root) = dirs::config_dir() {
        paths.push(root.join("opencode/opencode.json"));
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Only the official OpenCode Go endpoint is accepted; custom proxies are not
/// quota credentials for this provider.
pub(crate) fn config_key(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let providers = value.get("provider")?.as_object()?;
    providers.values().find_map(|provider| {
        let options = provider.get("options")?.as_object()?;
        let base = options.get("baseURL").and_then(Value::as_str)?.trim_end_matches('/');
        if base != OFFICIAL_BASE_URL {
            return None;
        }
        options.get("apiKey").and_then(Value::as_str).map(str::trim).filter(|key| !key.is_empty()).map(str::to_string)
    })
}

/// `auth.json` maps a provider id to `{type, key}` — the legacy store V2
/// imports once and stops writing. Only the ids OpenCode writes a Go key under
/// count: any other `type:"api"` entry used to be a last resort, and that is
/// exactly how a local-relay token ended up being posted to `opencode.ai`
/// (measured 2026-10-14: `401 AuthError`), buying one wasted vendor call and a
/// leaked unrelated credential per pass.
pub(crate) fn auth_key(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let map = value.as_object()?;
    for id in AUTH_IDS {
        let Some(entry) = map.get(*id) else { continue };
        if entry.get("type").and_then(Value::as_str) != Some("api") {
            continue;
        }
        let key = entry.get("key").and_then(Value::as_str).map(str::trim).unwrap_or_default();
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Captured shape of a 200 from `GET /zen/go/v1/usage` (fake numbers, real
    /// field names and timestamp dialects: `resetsAt` arrives as `Z` *and* as an
    /// offset, and extra top-level keys ride along).
    const OK_SAMPLE: &str = r#"{
      "usage": {
        "rolling":  {"status":"ok","percent":12.5,"resetsAt":"2026-08-12T12:30:00Z"},
        "weekly":   {"status":"ok","percent":45,"resetsAt":"2026-08-16T18:00:00+02:00"},
        "monthly":  {"status":"ok","percent":80,"resetsAt":"2026-09-01T00:00:00-04:00"}
      },
      "ignored": true
    }"#;

    /// The V2 store's `credential` table, verbatim from this machine's
    /// `opencode.db` (measured 2026-10-14). Idempotent, so a test can seed a
    /// second account into a store it already built.
    fn seed_store(path: &Path, rows: &[(&str, &str, Option<i64>, i64, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS credential (id text PRIMARY KEY, integration_id text, label text NOT NULL, \
             value text NOT NULL, connector_id text, method_id text, active integer, \
             time_created integer NOT NULL, time_updated integer NOT NULL);",
        )
        .unwrap();
        for (id, integration, active, created, value) in rows {
            conn.execute(
                "INSERT INTO credential (id, integration_id, label, value, active, time_created, time_updated) \
                 VALUES (?1, ?2, 'Default', ?3, ?4, ?5, ?5)",
                rusqlite::params![id, integration, value, active, created],
            )
            .unwrap();
        }
    }

    fn oauth(access: &str, expires_ms: i64) -> String {
        format!(r#"{{"type":"oauth","methodID":"device","refresh":"r","access":"{access}","expires":{expires_ms}}}"#)
    }

    #[test]
    fn three_windows_in_stable_order() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(
            s.iter().map(|x| (x.window_minutes, x.used_percent)).collect::<Vec<_>>(),
            vec![(300, 12.5), (10_080, 45.0), (43_200, 80.0)],
            "sorted by window, not by the order the API happened to serialise them"
        );
        assert_eq!(s[0].label.as_deref(), Some("5 小时"), "rolling is the 5-hour window, not a day");
        assert!(s[0].resets_at_ms > 1_700_000_000_000);
    }

    #[test]
    fn an_offset_reset_is_read_as_utc_not_as_local() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(s[1].resets_at_ms, chrono::DateTime::parse_from_rfc3339("2026-08-16T16:00:00Z").unwrap().timestamp_millis());
    }

    #[test]
    fn a_rate_limited_window_is_exhausted_whatever_percent_says() {
        let body: Value = serde_json::from_str(
            r#"{"usage":{"rolling":{"status":"rate-limited","percent":42,"resetsAt":"2026-08-12T12:30:00Z"},
                 "weekly":{"status":"ok","percent":7}}}"#,
        )
        .unwrap();
        let s = samples_from(&body);
        assert_eq!(s[0].used_percent, 100.0, "the vendor's stale percent loses to its status");
        assert!(s[0].resets_at_ms > 0, "the reported reset time is still kept");
        assert_eq!(s[1].used_percent, 7.0, "healthy windows in the same body stay visible");
    }

    #[test]
    fn percent_is_used_percent_and_is_clamped_not_inverted() {
        let body: Value =
            serde_json::from_str(r#"{"usage":{"monthly":{"status":"ok","percent":130}}}"#).unwrap();
        let s = samples_from(&body);
        assert_eq!(s[0].used_percent, 100.0);
        assert_eq!(s[0].resets_at_ms, 0, "a missing resetsAt is unknown, not now");
    }

    #[test]
    fn the_not_subscribed_403_envelope_yields_nothing() {
        // A live Go account's answer for a key without the allowance.
        let body: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"EntitlementError","message":"OpenCode Go subscription required."}}"#,
        )
        .unwrap();
        assert!(samples_from(&body).is_empty());
    }

    #[test]
    fn partial_and_junk_bodies_are_not_errors() {
        let body: Value =
            serde_json::from_str(r#"{"usage":{"weekly":{"status":"ok","percent":3}}}"#).unwrap();
        assert_eq!(samples_from(&body).len(), 1);
        assert!(samples_from(&serde_json::from_str(r#"{"usage":{}}"#).unwrap()).is_empty());
        assert!(samples_from(&Value::Null).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"usage":{"rolling":"nope"}}"#).unwrap()).is_empty());
    }

    #[test]
    fn a_refusal_names_the_vendors_own_error_type() {
        let body: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"EntitlementError","message":"OpenCode Go subscription required."}}"#,
        )
        .unwrap();
        assert_eq!(refusal(403, &body), "HTTP 403 EntitlementError");
        let auth: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"AuthError","message":"Unauthorized"}}"#,
        )
        .unwrap();
        assert_eq!(refusal(401, &auth), "HTTP 401 AuthError");
        assert_eq!(refusal(500, &Value::Null), "HTTP 500", "no envelope still gets the status");
    }

    /// The measured shape of this machine's V2 store (see the module docs): the
    /// account login's `access` token is the credential the vendor accepts.
    #[test]
    fn the_v2_store_supplies_the_account_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        seed_store(&path, &[("cred_1", "opencode", Some(1), 100, &oauth("access-token", 4_000_000_000_000))]);
        match db_key(&path) {
            Credential::Token(key) => assert_eq!(key, "access-token"),
            _ => panic!("the active oauth row must answer"),
        }
    }

    #[test]
    fn a_pasted_key_in_the_v2_store_reads_like_a_legacy_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        seed_store(&path, &[("cred_1", "opencode-go", Some(1), 100, r#"{"type":"api","key":" sk-go "}"#)]);
        match db_key(&path) {
            Credential::Token(key) => assert_eq!(key, "sk-go", "trimmed, and the Go id counts too"),
            _ => panic!("an api row must answer"),
        }
    }

    /// `auth switch` marks the selected account; a row the user switched away
    /// from must not win, and a store holding only switched-away rows still
    /// answers rather than reporting nothing.
    #[test]
    fn the_active_account_wins_and_a_switched_away_one_still_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        seed_store(
            &path,
            &[
                ("cred_old", "opencode", Some(0), 10, &oauth("switched-away", 4_000_000_000_000)),
                ("cred_now", "opencode", Some(1), 20, &oauth("active", 4_000_000_000_000)),
            ],
        );
        let Credential::Token(key) = db_key(&path) else { panic!("a row must answer") };
        assert_eq!(key, "active");
    }

    /// An expired token is OpenCode's to refresh. Sending it would buy a 401 and
    /// hide the account; the reason travels to the host's log instead.
    #[test]
    fn an_expired_v2_token_waits_for_opencode_to_refresh_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        seed_store(&path, &[("cred_1", "opencode", Some(1), 100, &oauth("stale", 1))]);
        assert!(matches!(db_key(&path), Credential::Expired));
        // An expired row does not shadow a usable one underneath it.
        seed_store(
            &path,
            &[("cred_2", "opencode", Some(0), 200, &oauth("fresh", 4_000_000_000_000))],
        );
        let Credential::Token(key) = db_key(&path) else { panic!("the usable row must answer") };
        assert_eq!(key, "fresh");
    }

    #[test]
    fn a_store_without_the_table_is_absent_not_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join(DB_FILE);
        Connection::open(&v1).unwrap().execute_batch("CREATE TABLE message (id text);").unwrap();
        assert!(matches!(db_key(&v1), Credential::Absent), "a v1 store answers nothing, quietly");
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"not a database").unwrap();
        assert!(matches!(db_key(&junk), Credential::Unreadable), "unreadable is a different reason");
    }

    // NTFS reserves `?`, so the fixture directory cannot exist on Windows and
    // the test would die at create_dir_all before asserting anything.
    #[cfg(not(windows))]
    #[test]
    fn a_store_under_a_path_with_a_space_or_a_question_mark_still_opens() {
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("a dir?with#odds");
        std::fs::create_dir_all(&odd).unwrap();
        let path = odd.join(DB_FILE);
        seed_store(&path, &[("cred_1", "opencode", Some(1), 100, &oauth("odd-path", 4_000_000_000_000))]);
        assert!(encode_uri_path(&path).contains("%3F") && encode_uri_path(&path).contains("%23"));
        let Credential::Token(key) = db_key(&path) else { panic!("the URI path must still resolve") };
        assert_eq!(key, "odd-path");
    }

    /// The whole point of the change: the store the tool actually writes beats
    /// the legacy file, and the explicit override beats both.
    #[test]
    fn the_first_source_that_answers_wins() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(DB_FILE);
        seed_store(&db, &[("cred_1", "opencode", Some(1), 100, &oauth("db-token", 4_000_000_000_000))]);
        let config = dir.path().join("opencode.json");
        std::fs::write(
            &config,
            r#"{"provider":{"go":{"options":{"baseURL":"https://opencode.ai/zen/go/v1","apiKey":"sk-config"}}}}"#,
        )
        .unwrap();
        let legacy = dir.path().join(AUTH_FILE);
        std::fs::write(&legacy, r#"{"opencode-go":{"type":"api","key":"sk-legacy"}}"#).unwrap();
        let configs = vec![config.clone()];

        assert_eq!(resolve(Some("sk-env".into()), Some(&db), &configs, Some(&legacy)).unwrap(), "sk-env");
        assert_eq!(resolve(None, Some(&db), &configs, Some(&legacy)).unwrap(), "db-token");
        assert_eq!(resolve(None, None, &configs, Some(&legacy)).unwrap(), "sk-config");
        assert_eq!(resolve(None, None, &[], Some(&legacy)).unwrap(), "sk-legacy");
    }

    /// Every empty answer carries the reason the host prints.
    #[test]
    fn the_empty_answer_names_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve(None, None, &[], None), Err("no official credential"));

        let expired = dir.path().join(DB_FILE);
        seed_store(&expired, &[("cred_1", "opencode", Some(1), 100, &oauth("stale", 1))]);
        assert_eq!(resolve(None, Some(&expired), &[], None), Err("credential expired"));

        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"not a database").unwrap();
        assert_eq!(resolve(None, Some(&junk), &[], None), Err("opencode.db unreadable"));
    }

    /// `auth.json` is legacy and may sit under any name; only the two ids
    /// OpenCode writes a Go key under are read, so an unrelated relay token is
    /// never posted to the vendor.
    #[test]
    fn only_the_ids_opencode_writes_a_go_key_under_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUTH_FILE);
        std::fs::write(
            &path,
            r#"{"anthropic":{"type":"api","key":"sk-zzz"},"my-router":{"type":"api","key":"sk-relay"}}"#,
        )
        .unwrap();
        assert_eq!(auth_key(&path), None, "a relay key is not a Go credential");
        std::fs::write(&path, r#"{"opencode-go":{"type":"oauth","key":"sk-x"}}"#).unwrap();
        assert_eq!(auth_key(&path), None, "an oauth entry under the Go id is not a Go key");
        std::fs::write(
            &path,
            r#"{"opencode":{"type":"api","key":"sk-alias"},"opencode-go":{"type":"api","key":"sk-go"}}"#,
        )
        .unwrap();
        assert_eq!(auth_key(&path).as_deref(), Some("sk-go"), "the Go id outranks the legacy alias");
        std::fs::write(&path, b"{").unwrap();
        assert_eq!(auth_key(&path), None, "a torn auth.json is not a crash");
    }

    #[test]
    fn only_the_official_provider_block_supplies_a_config_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        std::fs::write(
            &path,
            r#"{"provider":{"proxy":{"options":{"baseURL":"https://proxy.example/v1","apiKey":"sk-proxy"}},"go":{"options":{"baseURL":"https://opencode.ai/zen/go/v1/","apiKey":" sk-go "}}}}"#,
        )
        .unwrap();
        assert_eq!(config_key(&path).as_deref(), Some("sk-go"));
        std::fs::write(
            &path,
            r#"{"provider":{"proxy":{"options":{"baseURL":"https://proxy.example/v1","apiKey":"sk-proxy"}}}}"#,
        )
        .unwrap();
        assert_eq!(config_key(&path), None, "a custom proxy must not become an official quota credential");
    }

    #[test]
    #[ignore]
    fn live_usage_endpoint_answers_for_this_account() {
        let samples = OpenCodeQuota.fetch();
        for (tool, why) in crate::notes::take() {
            println!("{tool}: {why}");
        }
        if samples.is_empty() {
            // Measured 2026-10-14: the credential V2 keeps in `opencode.db`
            // (`credential`, integration `opencode`, `type:"oauth"`) answers 200
            // with the three windows; a box with no such row prints the reason
            // above instead.
            println!("opencode: no Go window on this account (the line above says why)");
            return;
        }
        for s in &samples {
            println!("opencode quota: {:.2}% window={} label={:?}", s.used_percent, s.window_minutes, s.label);
        }
    }
}
