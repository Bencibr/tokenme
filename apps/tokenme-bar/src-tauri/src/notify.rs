//! Threshold banners for vendor quota windows: a native notification when a
//! window crosses 80 % used, and one when it is used up.
//!
//! Edge-triggered by design. Every window carries an entry in
//! `<data dir>/notify-state.json`; a banner only goes out when an observation
//! climbs above the highest tier the entry has recorded, and the tier advances
//! **only after the banner actually posts** — a delivery that macOS refused is
//! retried on a later pass, not forgotten. A tier that falls back (a window
//! renewed, credits were topped up) silently re-arms the line, a first sight of
//! a window is a silent baseline unless it is already used up (that one
//! announces itself once), and a reset instant that jumps means the vendor
//! rolled the window over: new instance, fresh baseline. The state is on disk
//! so a restart does not replay a banner that was already shown, and the
//! quota-pass TTL can re-publish the same sample all it likes — the machine is
//! idempotent.
//!
//! Delivery goes through notify-rust with its `preview-macos-un` feature —
//! `UNUserNotificationCenter`, the path the spike proved posts from an
//! ad-hoc-signed bundle — which also returns real delivery results and the
//! permission query/request entry points. tauri-plugin-notification was tried
//! and dropped: on desktop its permission API is an empty stub, its `show`
//! discards the error, and its default macOS backend is the deprecated
//! NSUserNotification path. On Windows the same crate's WinRT toast backend
//! carries the banner; the app's bundle identifier becomes its AppUserModelID
//! when installed (in dev the default identity shows instead of a toast that
//! would be dropped for an unregistered id).
//!
//! The engine thread only enqueues observations; a dedicated worker thread owns
//! the tier state, the permission flow and the disk writes, so nothing here can
//! stall a publish. Asking for permission happens lazily — at the first banner
//! that actually needs it — so a user who never crosses a line never sees the
//! prompt.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use usage_core::report::{QuotaOrigin, QuotaView};

use crate::lang::Lang;

/// The warning line: used ≥ 80 % of the window.
const WARN_PCT: f64 = 80.0;
/// The exhaustion line: used ≥ 100 %.
const EXHAUSTED_PCT: f64 = 100.0;
/// A reset instant that moved more than this was a window rollover, not probe
/// jitter — the same window's reset wobbles by seconds between samples.
const RENEW_SLACK_MS: i64 = 300_000;
/// Floor between delivery attempts for one window: a banner that failed to post
/// is retried, but not on every publish.
const RETRY_GAP: Duration = Duration::from_secs(60);
/// Entries not seen for this long are dropped at load — a window that left the
/// report months ago is not coming back with its old tier.
const STATE_RETENTION_MS: i64 = 30 * 24 * 3_600_000;
/// How long a cached authorization answer is trusted before it is queried again.
const SETTINGS_TTL: Duration = Duration::from_secs(60);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn state_path() -> Option<PathBuf> {
    crate::engine::index_path()
        .parent()
        .map(|dir| dir.join("notify-state.json"))
}

/// One window's memory: the highest tier it is known to have reached (a posted
/// banner, or the silent first-sight baseline), the reset instant it was
/// observed under, and the bookkeeping times.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    tier: u8,
    /// 0 = the vendor advertises no reset for this window (credit meters).
    #[serde(default)]
    resets_at_ms: i64,
    #[serde(default)]
    last_fired_ms: i64,
    #[serde(default)]
    last_seen_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct NotifyState {
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
}

/// Which line an observation sits on: quiet, the 80 % warning, or used up.
fn tier_of(used_percent: f64) -> u8 {
    if used_percent >= EXHAUSTED_PCT {
        2
    } else if used_percent >= WARN_PCT {
        1
    } else {
        0
    }
}

/// Normalised the way the report's own window merge normalises labels, so
/// "5 小时" and "5小时" are one window here as they are there.
fn label_key(label: Option<&str>) -> String {
    label
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The window's identity across polls. The probe-written `id` is the stable
/// one; a label it cannot offer (log rows, older probes) is the fallback, and
/// the length keeps two same-named meters of one tool apart.
fn key_of(q: &QuotaView) -> String {
    let identity = match q.id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => label_key(q.label.as_deref()),
    };
    format!("{}|{}|{}", q.tool, identity, q.window_minutes)
}

/// What one observation decides: the entry to store, the tier to announce (if
/// any), and whether the store must hit the disk. For an announcement the
/// entry's tier deliberately stays where it was — the caller advances it only
/// after the banner posts, so a failed delivery is retried instead of being
/// silently swallowed.
struct Decision {
    entry: Entry,
    fire: Option<u8>,
    persist: bool,
}

/// A fresh-baseline decision — a first sight, or a window the vendor just
/// rolled over. A quiet tier is settled silently at what it observed; an
/// already-exhausted window announces itself, and until that post lands the
/// entry deliberately stays at tier 0 — settled, not merely stored — so a
/// refused delivery is offered again on a later pass instead of being recorded
/// as told. (A live run caught the original shape persisting tier 2 with
/// `fired = 0` when the system refused the banner, which swallowed the retry
/// the log itself promises.)
fn fresh_baseline(tier: u8, resets_at_ms: i64, now: i64) -> Decision {
    let fire = (tier == 2).then_some(2);
    Decision {
        entry: Entry {
            tier: if fire.is_some() { 0 } else { tier },
            resets_at_ms,
            last_fired_ms: 0,
            last_seen_ms: now,
        },
        fire,
        persist: true,
    }
}

fn plan(prev: Option<&Entry>, tier: u8, resets_at_ms: i64, now: i64) -> Decision {
    let Some(prev) = prev else {
        // First sight: the crossing may have happened before this process ever
        // ran, so nothing is announced except an already-exhausted window —
        // "用完" is the alarm, and meeting it silently would be the wrong quiet.
        return fresh_baseline(tier, resets_at_ms, now);
    };
    let renewed = prev.resets_at_ms > 0
        && resets_at_ms > 0
        && (resets_at_ms - prev.resets_at_ms).abs() > RENEW_SLACK_MS;
    if renewed {
        // The vendor rolled the window over while we watched: a new instance
        // gets the first-sight treatment, not a banner for the old one's tally.
        return fresh_baseline(tier, resets_at_ms, now);
    }
    let mut entry = prev.clone();
    if resets_at_ms > 0 {
        entry.resets_at_ms = resets_at_ms;
    }
    entry.last_seen_ms = now;
    if tier > prev.tier {
        return Decision { entry, fire: Some(tier), persist: false };
    }
    if tier < prev.tier {
        // The line re-arms silently: a renewed window (whose reset instant did
        // not move enough to read as a rollover) or credits topped back up.
        entry.tier = tier;
        return Decision { entry, fire: None, persist: true };
    }
    Decision { entry, fire: None, persist: false }
}

fn prune_at(state: &mut NotifyState, now: i64) {
    state.entries.retain(|_, e| now - e.last_seen_ms <= STATE_RETENTION_MS);
}

/// The banner's two lines. Title names the subject; the body carries the state.
fn copy_for(
    display: &str,
    label: Option<&str>,
    used_percent: f64,
    tier: u8,
    lang: Lang,
) -> (String, String) {
    let title = match label {
        Some(l) if !l.is_empty() => format!("{display} · {l}"),
        _ => display.to_string(),
    };
    let body = if tier >= 2 {
        lang.str("额度已用完（100%）", "Quota used up (100%)").to_string()
    } else {
        let used = used_percent.round() as i64;
        let left = (100.0 - used_percent).max(0.0).round() as i64;
        match lang {
            Lang::Zh => format!("已用 {used}%（剩余 {left}%）"),
            Lang::En => format!("{used}% used · {left}% left"),
        }
    };
    (title, body)
}

enum Msg {
    /// A fresh report's quota views. `displays` maps tool id → human name.
    Observe(Vec<QuotaView>, HashMap<String, String>, Lang),
    /// A one-shot banner for manual QA of the permission flow.
    Test,
}

static CHANNEL: OnceLock<Sender<Msg>> = OnceLock::new();

/// Windows posts under the app's AppUserModelID; that is the bundle identifier
/// the installer registers, captured once at setup.
#[cfg(target_os = "windows")]
static IDENTIFIER: OnceLock<String> = OnceLock::new();

fn channel() -> &'static Sender<Msg> {
    CHANNEL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Msg>();
        thread::Builder::new()
            .name("tokenme-notify".into())
            .spawn(move || worker(rx))
            .expect("failed to spawn the notify worker");
        tx
    })
}

/// Called once at setup.
pub fn init(app: &tauri::AppHandle) {
    #[cfg(target_os = "windows")]
    {
        let _ = IDENTIFIER.set(app.config().identifier.clone());
    }
    #[cfg(not(target_os = "windows"))]
    let _ = app;
}

/// Offer a fresh report's quota views to the tier machine. Budget views (caps
/// the user set in tokenme, not vendor quota) are not this feature's business.
pub fn observe(quotas: &[QuotaView], displays: &HashMap<String, String>, lang: Lang) {
    let relevant: Vec<QuotaView> = quotas
        .iter()
        .filter(|q| q.origin != QuotaOrigin::Budget && q.used_percent.is_finite())
        .cloned()
        .collect();
    if relevant.is_empty() {
        return;
    }
    let _ = channel().send(Msg::Observe(relevant, displays.clone(), lang));
}

/// Post a test banner through the same gate a real one walks (permission
/// request included), for `TOKENME_TEST_NOTIFY=1` QA runs.
pub fn send_test() {
    let _ = channel().send(Msg::Test);
}

fn worker(rx: Receiver<Msg>) {
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = mac_usernotifications::check_bundle() {
            crate::logging::info(&format!(
                "notify: this process has no bundle identifier ({e}) — desktop banners are off (expected under `cargo run`, not under the .app)"
            ));
        }
    }
    let mut w = Worker::new();
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Observe(quotas, displays, lang) => w.observe_views(&quotas, &displays, lang),
            Msg::Test => w.test(),
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    Granted,
    Denied,
    NotDetermined,
}

enum DeliveryError {
    /// The user said no; only they can undo it.
    Denied,
    /// No answer yet — the prompt is up (or was just raised).
    NotDetermined,
    Other(String),
}

struct Worker {
    state: NotifyState,
    /// Last delivery attempt per window — the retry floor.
    last_attempt: HashMap<String, Instant>,
    /// One line per window per verdict, so a failing window cannot flood the log.
    noted_once: HashSet<String>,
    #[cfg(target_os = "macos")]
    auth_cache: Option<(Instant, Auth)>,
    #[cfg(target_os = "macos")]
    requested: bool,
}

impl Worker {
    fn new() -> Self {
        Self {
            state: load(),
            last_attempt: HashMap::new(),
            noted_once: HashSet::new(),
            #[cfg(target_os = "macos")]
            auth_cache: None,
            #[cfg(target_os = "macos")]
            requested: false,
        }
    }

    fn observe_views(&mut self, quotas: &[QuotaView], displays: &HashMap<String, String>, lang: Lang) {
        let now = now_ms();
        let mut dirty = false;
        for q in quotas {
            let key = key_of(q);
            let prev = self.state.entries.get(&key).cloned();
            let decision = plan(prev.as_ref(), tier_of(q.used_percent), q.resets_at_ms, now);
            let mut entry = decision.entry;
            if let Some(tier) = decision.fire {
                if self.attempt_due(&key) {
                    let display = displays.get(&q.tool).map(String::as_str).unwrap_or(&q.tool);
                    let (title, body) = copy_for(display, q.label.as_deref(), q.used_percent, tier, lang);
                    match self.deliver(&key, &title, &body) {
                        Ok(()) => {
                            entry.tier = tier;
                            entry.last_fired_ms = now;
                            dirty = true;
                            crate::logging::info(&format!("notify: posted [{key}] {title} — {body}"));
                        }
                        Err(e) => self.note_failure(&key, e),
                    }
                }
            } else if decision.persist {
                dirty = true;
            }
            self.state.entries.insert(key, entry);
        }
        if dirty {
            save(&self.state);
        }
    }

    fn test(&mut self) {
        let lang = crate::lang::get();
        let title = "TokenMe".to_string();
        let body = lang
            .str(
                "通知已就绪 — 额度到 80% 和 100% 时会在此提醒",
                "Notifications are ready — you'll be alerted at 80% and 100%",
            )
            .to_string();
        match self.deliver("__test__", &title, &body) {
            Ok(()) => crate::logging::info("notify test: posted — check the notification centre"),
            Err(DeliveryError::Denied) => crate::logging::info(
                "notify test: blocked — notifications are denied (System Settings → Notifications → TokenMe)",
            ),
            Err(DeliveryError::NotDetermined) => crate::logging::info(
                "notify test: permission was just requested — allow it, then run the test again",
            ),
            Err(DeliveryError::Other(e)) => {
                crate::logging::error(&format!("notify test: delivery failed — {e}"))
            }
        }
    }

    fn attempt_due(&mut self, key: &str) -> bool {
        let now = Instant::now();
        match self.last_attempt.get(key) {
            Some(at) if at.elapsed() < RETRY_GAP => false,
            _ => {
                self.last_attempt.insert(key.to_string(), now);
                true
            }
        }
    }

    fn note_failure(&mut self, key: &str, e: DeliveryError) {
        match e {
            DeliveryError::Denied => {
                if self.noted_once.insert(format!("{key}#denied")) {
                    crate::logging::info(&format!(
                        "notify: [{key}] blocked — notifications are denied; allow TokenMe under System Settings → Notifications and the next pass will post it"
                    ));
                }
            }
            DeliveryError::NotDetermined => {
                if self.noted_once.insert(format!("{key}#asked")) {
                    crate::logging::info(&format!(
                        "notify: [{key}] waiting for the permission answer — the banner goes out on the next pass once it is allowed"
                    ));
                }
            }
            DeliveryError::Other(msg) => {
                crate::logging::error(&format!("notify: [{key}] delivery failed — {msg}"));
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn deliver(&mut self, _key: &str, title: &str, body: &str) -> Result<(), DeliveryError> {
        match self.auth()? {
            Auth::Granted => {}
            Auth::Denied => return Err(DeliveryError::Denied),
            Auth::NotDetermined => {
                self.kick_request();
                return Err(DeliveryError::NotDetermined);
            }
        }
        post(title, body).map_err(DeliveryError::Other)
    }

    #[cfg(not(target_os = "macos"))]
    fn deliver(&mut self, _key: &str, title: &str, body: &str) -> Result<(), DeliveryError> {
        post(title, body).map_err(DeliveryError::Other)
    }

    #[cfg(target_os = "macos")]
    fn auth(&mut self) -> Result<Auth, DeliveryError> {
        if let Some((at, state)) = self.auth_cache {
            if at.elapsed() < SETTINGS_TTL {
                return Ok(state);
            }
        }
        let settings = mac_usernotifications::blocking::get_notification_settings()
            .map_err(|e| DeliveryError::Other(format!("settings query failed: {e}")))?;
        let state = match settings.authorization_status {
            mac_usernotifications::AuthorizationStatus::Authorized
            | mac_usernotifications::AuthorizationStatus::Provisional
            | mac_usernotifications::AuthorizationStatus::Ephemeral => Auth::Granted,
            mac_usernotifications::AuthorizationStatus::Denied => Auth::Denied,
            _ => Auth::NotDetermined,
        };
        self.auth_cache = Some((Instant::now(), state));
        Ok(state)
    }

    /// Raise the system permission prompt, once per process. The request blocks
    /// until the user answers — on its own thread, so a prompt nobody answers
    /// cannot wedge the worker; the pending banner posts on a later pass once
    /// the answer lands.
    #[cfg(target_os = "macos")]
    fn kick_request(&mut self) {
        if self.requested {
            return;
        }
        self.requested = true;
        crate::logging::info("notify: asking for notification permission (system banner)");
        thread::spawn(|| match mac_usernotifications::blocking::request_auth() {
            Ok(true) => crate::logging::info("notify: permission granted by the user"),
            Ok(false) => crate::logging::info("notify: permission denied by the user"),
            Err(e) => crate::logging::error(&format!("notify: permission request failed — {e}")),
        });
    }
}

/// The platform post. notify-rust on macOS rides its `preview-macos-un` feature
/// (UNUserNotificationCenter); on Windows its WinRT toast backend.
#[cfg(target_os = "macos")]
fn post(title: &str, body: &str) -> Result<(), String> {
    notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .show()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "windows")]
fn post(title: &str, body: &str) -> Result<(), String> {
    let mut n = notify_rust::Notification::new();
    n.summary(title).body(body);
    // In dev the identifier is not registered as an AppUserModelID and the
    // toast would be dropped; the default identity is the honest fallback.
    if !tauri::is_dev() {
        if let Some(id) = IDENTIFIER.get() {
            if !id.is_empty() {
                n.app_id(id);
            }
        }
    }
    n.show().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn post(_title: &str, _body: &str) -> Result<(), String> {
    Err("desktop notifications are not wired on this platform".into())
}

fn load() -> NotifyState {
    let Some(path) = state_path() else { return NotifyState::default() };
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return NotifyState::default(),
    };
    match serde_json::from_str::<NotifyState>(&raw) {
        Ok(mut state) => {
            prune_at(&mut state, now_ms());
            state
        }
        Err(e) => {
            crate::logging::info(&format!(
                "notify: state file unusable ({e}) — every window baselines again this run"
            ));
            NotifyState::default()
        }
    }
}

/// 0600 + tmp + rename, the snapshot writer's pattern: a torn write can leave
/// the tmp file behind but never a half state that reads as "fired".
fn save(state: &NotifyState) {
    let Some(path) = state_path() else { return };
    let json = match serde_json::to_string(state) {
        Ok(json) => json,
        Err(e) => {
            crate::logging::error(&format!("notify: state did not serialise — {e}"));
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    let result = (|| -> Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        fs::rename(&tmp, &path).map_err(|e| e.to_string())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        crate::logging::error(&format!("notify: state could not be written ({e})"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::report::QuotaView;

    const NOW: i64 = 1_759_810_000_000;
    /// A reset instant far enough out that "no reset" tests do not fake one.
    const RESET: i64 = NOW + 3_600_000;

    fn view(tool: &str, used: f64, resets: i64, label: Option<&str>, id: Option<&str>) -> QuotaView {
        QuotaView {
            tool: tool.into(),
            used_percent: used,
            window_minutes: 300,
            resets_at_ms: resets,
            sampled_at_ms: NOW,
            label: label.map(Into::into),
            id: id.map(Into::into),
            origin: QuotaOrigin::Probe,
        }
    }

    #[test]
    fn the_tiers_split_at_the_two_lines() {
        assert_eq!(tier_of(0.0), 0);
        assert_eq!(tier_of(79.99), 0);
        assert_eq!(tier_of(80.0), 1, "the warning line itself counts");
        assert_eq!(tier_of(99.99), 1);
        assert_eq!(tier_of(100.0), 2, "and so does the exhaustion line");
        assert_eq!(tier_of(104.5), 2);
    }

    #[test]
    fn the_window_key_prefers_the_stable_id_and_normalises_labels() {
        // An id outranks a label even when both are present.
        let with_id = view("qoder", 81.0, RESET, Some("5 小时"), Some("qw-5h"));
        assert_eq!(key_of(&with_id), "qoder|qw-5h|300");
        // Label fallback: whitespace and case must not fork the identity.
        let labelled = view("qoder", 81.0, RESET, Some("5 小时"), None);
        let same = view("qoder", 81.0, RESET, Some("5小时"), None);
        assert_eq!(key_of(&labelled), key_of(&same));
        // An empty id string is not an identity.
        let empty_id = view("qoder", 81.0, RESET, Some("weekly"), Some(""));
        assert_eq!(key_of(&empty_id), "qoder|weekly|300");
        // Same label, different lengths: two meters, two keys.
        let mut monthly = labelled.clone();
        monthly.window_minutes = 43_200;
        assert_ne!(key_of(&labelled), key_of(&monthly));
    }

    #[test]
    fn a_first_sight_is_a_silent_baseline_unless_it_is_already_done() {
        let quiet = plan(None, 0, RESET, NOW);
        assert_eq!(quiet.fire, None);
        assert!(quiet.persist, "the baseline must survive a restart");
        let warned = plan(None, 1, RESET, NOW);
        assert_eq!(warned.fire, None, "the crossing predates this process — no banner");
        let done = plan(None, 2, RESET, NOW);
        assert_eq!(done.fire, Some(2), "已经用完 is the alarm; meeting it quietly is wrong");
        assert_eq!(done.entry.tier, 0, "the announcement is pending until it actually posts");
    }

    #[test]
    fn a_refused_announcement_is_offered_again_and_not_recorded_as_told() {
        // First sight of an exhausted window whose banner the system refused:
        // the persisted entry stays at the pre-announcement baseline, so the
        // next pass offers the banner again — a retry, not a silent swallow.
        let first = plan(None, 2, RESET, NOW);
        assert_eq!(first.fire, Some(2));
        let retry = plan(Some(&first.entry), 2, RESET, NOW + 60_000);
        assert_eq!(retry.fire, Some(2), "still owed to the user");
        assert_eq!(retry.entry.tier, 0);
        // Once a post lands, the caller advances the tier and the line quiets.
        let settled = Entry { tier: 2, last_fired_ms: NOW, ..first.entry.clone() };
        assert_eq!(plan(Some(&settled), 2, RESET, NOW + 120_000).fire, None);
        // A renewal met at 100 % is the same contract: announced, or retried.
        let old = Entry { tier: 1, resets_at_ms: RESET, last_fired_ms: NOW, last_seen_ms: NOW };
        let renewed = plan(Some(&old), 2, RESET + RENEW_SLACK_MS + 1, NOW);
        assert_eq!(renewed.fire, Some(2));
        assert_eq!(renewed.entry.tier, 0, "fresh instance, announcement pending");
    }

    #[test]
    fn crossing_a_line_announces_it_and_only_advances_on_that_post() {
        let prev = Entry { tier: 0, resets_at_ms: RESET, last_fired_ms: 0, last_seen_ms: NOW };
        let d = plan(Some(&prev), 1, RESET, NOW);
        assert_eq!(d.fire, Some(1));
        assert_eq!(d.entry.tier, 0, "the tier advances in the caller, after the post");
        assert!(!d.persist, "nothing durable happened yet");
        // Straight past both lines in one sample: one banner, the stronger tier.
        let d = plan(Some(&prev), 2, RESET, NOW);
        assert_eq!(d.fire, Some(2));
        // The same observation again: already claimed, nothing to say.
        let at_one = Entry { tier: 1, ..prev.clone() };
        let d = plan(Some(&at_one), 1, RESET, NOW);
        assert_eq!(d.fire, None);
        assert!(!d.persist);
    }

    #[test]
    fn a_drop_rearms_the_line_silently() {
        let down = Entry { tier: 2, resets_at_ms: RESET, last_fired_ms: NOW - 1, last_seen_ms: NOW };
        let d = plan(Some(&down), 0, RESET, NOW);
        assert_eq!(d.fire, None, "falling back never announces");
        assert_eq!(d.entry.tier, 0);
        assert!(d.persist, "the re-arm must survive a restart");
        // And the next climb announces again.
        let d = plan(Some(&d.entry), 2, RESET, NOW + 1_000);
        assert_eq!(d.fire, Some(2));
    }

    #[test]
    fn reset_jitter_is_not_a_renewal_but_a_jump_is() {
        let prev = Entry { tier: 1, resets_at_ms: RESET, last_fired_ms: 0, last_seen_ms: NOW };
        // Seconds of wobble between samples: same instance, same tier, quiet.
        let d = plan(Some(&prev), 1, RESET + 60_000, NOW);
        assert_eq!(d.fire, None);
        assert_eq!(d.entry.resets_at_ms, RESET + 60_000);
        // A reset that jumped is the vendor's next window: fresh baseline, no
        // "re-arm" for a line nobody is below.
        let d = plan(Some(&prev), 0, RESET + RENEW_SLACK_MS + 1, NOW);
        assert_eq!(d.fire, None);
        assert_eq!(d.entry.tier, 0, "a renewed window baselines at what it shows");
        assert_eq!(d.entry.last_fired_ms, 0, "the old instance's fire time does not carry over");
        // A renewed window met at 100 % announces once, like any first sight.
        let d = plan(Some(&prev), 2, RESET + RENEW_SLACK_MS + 1, NOW);
        assert_eq!(d.fire, Some(2));
        assert_eq!(d.entry.tier, 0, "pending like a first sight — the post settles it");
        // A window that never advertises a reset cannot renew by reset; the
        // drop re-arm covers its top-ups instead.
        let no_reset = Entry { tier: 2, resets_at_ms: 0, last_fired_ms: NOW - 1, last_seen_ms: NOW };
        let d = plan(Some(&no_reset), 0, 0, NOW);
        assert_eq!(d.fire, None);
        assert_eq!(d.entry.tier, 0);
    }

    #[test]
    fn the_banner_copy_rounds_and_respects_the_language() {
        let (title, body) = copy_for("Qoder", Some("5 小时"), 82.4, 1, Lang::Zh);
        assert_eq!(title, "Qoder · 5 小时");
        assert_eq!(body, "已用 82%（剩余 18%）");
        let (title, body) = copy_for("Qoder", None, 99.6, 1, Lang::En);
        assert_eq!(title, "Qoder");
        assert_eq!(body, "100% used · 0% left");
        let (_, body) = copy_for("ZCode", Some("每月"), 100.0, 2, Lang::Zh);
        assert_eq!(body, "额度已用完（100%）");
        let (_, body) = copy_for("ZCode", Some("每月"), 130.0, 2, Lang::En);
        assert_eq!(body, "Quota used up (100%)");
        // An empty label is the same as no label.
        let (title, _) = copy_for("Qoder", Some(""), 81.0, 1, Lang::Zh);
        assert_eq!(title, "Qoder");
    }

    #[test]
    fn state_survives_json_and_older_shapes() {
        let mut state = NotifyState::default();
        state.entries.insert("qoder|qw-5h|300".into(), Entry { tier: 2, resets_at_ms: RESET, last_fired_ms: NOW, last_seen_ms: NOW });
        let back: NotifyState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(state, back, "the file is the only route across a restart");
        // A hand-written or older payload with missing fields reads as quiet.
        let old: NotifyState = serde_json::from_str(r#"{"entries":{"x|y|300":{}}}"#).unwrap();
        assert_eq!(old.entries["x|y|300"], Entry::default());
        let empty: NotifyState = serde_json::from_str("{}").unwrap();
        assert!(empty.entries.is_empty());
    }

    #[test]
    fn stale_entries_are_pruned_at_load() {
        let mut state = NotifyState::default();
        state.entries.insert("fresh".into(), Entry { tier: 1, resets_at_ms: 0, last_fired_ms: 0, last_seen_ms: NOW - 1_000 });
        state.entries.insert("stale".into(), Entry { tier: 2, resets_at_ms: 0, last_fired_ms: 0, last_seen_ms: NOW - STATE_RETENTION_MS - 1 });
        prune_at(&mut state, NOW);
        assert!(state.entries.contains_key("fresh"));
        assert!(!state.entries.contains_key("stale"), "a window gone for a month is gone");
    }
}
