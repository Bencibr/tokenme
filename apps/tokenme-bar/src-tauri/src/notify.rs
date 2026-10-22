//! Threshold banners for vendor quota windows, plus one native notification
//! when a package enters the user's selected expiry lead-time window.
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
use crate::settings::{NotifyTier, Settings};

/// What the user's settings say about one crossing: which quota lines and
/// package-expiry reminders may post, and which tools stay quiet. Built once per
/// report from [`Settings`] so the tier machine never reads global state itself.
///
/// A crossing that may not post still advances the window's recorded tier: the
/// delivery policy is not the tier machine's business, and un-muting a tool (or
/// turning the tiers back on) an hour later must not retro-fire a line it
/// crossed while it was supposed to be quiet.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    tiers: NotifyTier,
    muted: Vec<String>,
    plan_expiry_notify_days: Option<u8>,
}

impl From<&Settings> for Gate {
    fn from(s: &Settings) -> Self {
        Self {
            tiers: s.notify_tiers,
            muted: s.notify_muted.clone(),
            plan_expiry_notify_days: s.plan_expiry_notify_days,
        }
    }
}

impl Gate {
    fn allows(&self, tool: &str, tier: u8) -> bool {
        !self.muted.iter().any(|m| m == tool)
            && self.tiers.min_tier().is_some_and(|floor| tier >= floor)
    }

    fn allows_expiry(&self, tool: &str) -> bool {
        self.plan_expiry_notify_days.is_some() && !self.muted.iter().any(|m| m == tool)
    }
}

/// The warning line: used ≥ 80 % of the window.
const WARN_PCT: f64 = 80.0;
/// The exhaustion line: used ≥ 100 %.
const EXHAUSTED_PCT: f64 = 100.0;
/// How far below the exhaustion line a reading must fall before a spent
/// window believes it and re-arms. Vendors round and jitter a fraction of a
/// percent across the line; without this margin, a 100 ↔ 99.8 flap re-arms
/// the spent window on every crossing and the 已用完 banner fires again each
/// time (measured 2026-10-10: intermittent repeats through the morning).
const EXHAUSTED_HYSTERESIS_PCT: f64 = 5.0;
/// A window that announced exhaustion stays quiet this long, whatever
/// upstream identity churn did to re-arm it — a collided or renamed window id
/// can make one poll's quiet reading re-arm another poll's spent state, and
/// this cooldown is the last line under the one-banner-per-refresh contract.
/// `last_fired_ms` survives re-arms, so the clock starts at the real post.
const EXHAUSTED_REFIRE_COOLDOWN_MS: i64 = 4 * 60 * 60 * 1000;
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
#[cfg(target_os = "macos")]
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
    /// Whether the selected lead-time reminder already posted for this expiry.
    #[serde(default)]
    expiry_notified: bool,
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

/// Existing quota providers use a zero-length window with a positive reset for
/// a package/credit expiry. Cola's zero-length plan row is different: its
/// timestamp is the daily quota reset, so it must not become an expiry alert.
fn is_package_expiry(q: &QuotaView) -> bool {
    q.window_minutes == 0 && q.resets_at_ms > 0 && q.tool != "cola"
}

fn expiry_due(resets_at_ms: i64, now: i64, days: Option<u8>) -> bool {
    let Some(days) = days else { return false };
    resets_at_ms > now && resets_at_ms - now <= i64::from(days) * 86_400_000
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
            ..Entry::default()
        },
        fire,
        persist: true,
    }
}

fn plan(prev: Option<&Entry>, tier: u8, resets_at_ms: i64, now: i64, used_percent: f64) -> Decision {
    let Some(prev) = prev else {
        // First sight: the crossing may have happened before this process ever
        // ran, so nothing is announced except an already-exhausted window —
        // "用完" is the alarm, and meeting it silently would be the wrong quiet.
        return fresh_baseline(tier, resets_at_ms, now);
    };
    // Only a forward jump past the slack reads as the vendor's next window; a
    // reset that moves the other way is the vendor's own jitter, not a
    // rollover, and a spent window must not re-announce off it.
    let renewed = prev.resets_at_ms > 0
        && resets_at_ms > 0
        && resets_at_ms > prev.resets_at_ms + RENEW_SLACK_MS;
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
        // A spent window that was re-armed may not re-announce inside the
        // cooldown: `last_fired_ms` survives re-arms precisely so this clock
        // starts at the real post, not at the re-arm. A first announcement
        // (last_fired_ms == 0) is never gated.
        if tier == 2
            && prev.last_fired_ms > 0
            && now - prev.last_fired_ms < EXHAUSTED_REFIRE_COOLDOWN_MS
        {
            return Decision { entry, fire: None, persist: false };
        }
        return Decision { entry, fire: Some(tier), persist: false };
    }
    if tier < prev.tier {
        // The line re-arms silently: a renewed window (whose reset instant did
        // not move enough to read as a rollover) or credits topped back up.
        // But a spent window only believes a real step down — one that clears
        // the hysteresis margin. A reading that merely wobbles under the line
        // keeps the window spent: it stays quiet and stays tier 2.
        if prev.tier >= 2 && used_percent > EXHAUSTED_PCT - EXHAUSTED_HYSTERESIS_PCT {
            return Decision { entry, fire: None, persist: false };
        }
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
        Some(l) if !l.is_empty() => format!("{display} · {}", localize_label(l, lang)),
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

fn copy_for_expiry(display: &str, label: Option<&str>, days: u8, lang: Lang) -> (String, String) {
    let title = match label {
        Some(l) if !l.is_empty() => format!("{display} · {}", localize_label(l, lang)),
        _ => display.to_string(),
    };
    let body = match lang {
        Lang::Zh => format!("套餐将在 {days} 天后到期"),
        Lang::En => format!("Package expires in {days} {}", if days == 1 { "day" } else { "days" }),
    };
    (title, body)
}

/// The engine's quota labels are zh-authored data — the same table the strip's
/// `quota.label.*` head map matches — and the banner title is where they
/// display, so the known engine nouns and the meter verbs translate here while
/// vendor plan names (Bonus Pack, GLM Coding Lite, CodeBuddy个人体验版) pass
/// through verbatim. Keep the zh heads byte-identical with the strip's table:
/// the two maps answer for the same strings. zh reads as identity, which also
/// keeps the label-derived hysteresis keys stable across language switches.
fn localize_label(label: &str, lang: Lang) -> String {
    if lang == Lang::Zh {
        return label.to_string();
    }
    label.split(" · ").map(localize_segment).collect::<Vec<_>>().join(" · ")
}

/// One ` · ` segment of a label: an exact engine noun, a composed phrase, or
/// something vendor-shaped that stays as it is.
fn localize_segment(seg: &str) -> String {
    /// The engine nouns, verbatim from the providers that compose them.
    const NOUNS: &[(&str, &str)] = &[
        ("5 小时", "5 hours"), ("7 天", "7 days"), ("天", "Day"), ("周", "Week"),
        ("月", "Month"), ("每周", "Weekly"), ("窗口", "Window"), ("今日额度", "Today's quota"),
        ("月度 Premium 请求", "Monthly Premium requests"), ("套餐积分", "Plan credits"),
        ("套餐内", "Included plan"), ("积分包", "Credit pack"), ("资源包", "Add-on pack"),
        ("专属资源包", "Dedicated add-on pack"), ("共享资源包", "Shared add-on pack"),
        ("积分", "Credits"), ("余额", "Balance"), ("每日限额", "Daily limit"),
        ("无可查限额", "No visible limit"), ("免费版", "Free tier"), ("会员", "Member"),
        ("模型额度", "Model quota"), ("会话", "Sessions"), ("工具调用", "Tool calls"),
        ("试用", "trial"), ("已自动签到", "Auto-claimed"), ("今日已签到", "Checked in today"),
        ("签到奖励", "Check-in bonus"), ("签到奖励(今)", "Check-in bonus (today)"),
        ("签到奖励(昨)", "Check-in bonus (yesterday)"),
    ];
    if let Some((_, en)) = NOUNS.iter().find(|(zh, _)| *zh == seg) {
        return (*en).to_string();
    }
    // Trailing nouns a composed head carries after its own name or figure;
    // rewritten in place so a leading verb ("剩余 8578 点") still matches below.
    let owned;
    let seg = if let Some(name) = seg.strip_suffix(" 套餐") {
        owned = format!("{name} plan"); &owned
    } else if let Some(n) = seg.strip_suffix(" 分钟") {
        owned = format!("{n} min"); &owned
    } else if let Some(n) = seg.strip_suffix(" 点") {
        owned = format!("{n} pts"); &owned
    } else {
        seg
    };
    // Leading verbs and the phrases that ride them.
    if let Some(r) = seg.strip_prefix("已用 ") {
        return format!("used {r}");
    }
    if let Some(r) = seg.strip_prefix("剩余 ") {
        return format!("{r} left");
    }
    if let Some(r) = seg.strip_prefix("限时 ") {
        return format!("limited {r}");
    }
    if let Some(r) = seg.strip_prefix("每日刷新 ") {
        return format!("daily refresh {r}");
    }
    if let Some(r) = seg.strip_prefix("赠送 ") {
        return format!("gift {r}");
    }
    if let Some(r) = seg.strip_prefix("积分 ") {
        return format!("{r} credits");
    }
    if let Some(r) = seg.strip_prefix("余额 ") {
        return format!("Balance {r}");
    }
    seg.to_string()
}

enum Msg {
    /// A fresh report's quota views. `displays` maps tool id → human name.
    Observe(Vec<QuotaView>, HashMap<String, String>, Lang, Gate),
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
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

pub fn init(app: &tauri::AppHandle) {
    #[cfg(target_os = "windows")]
    {
        let _ = IDENTIFIER.set(app.config().identifier.clone());
    }
    let _ = APP.set(app.clone());
}

/// Offer a fresh report's quota views to the notification worker. Budget views
/// (caps the user set in tokenme, not vendor quota) are not this feature's
/// business; vendor package expiry rows are handled alongside quota tiers.
pub fn observe(quotas: &[QuotaView], displays: &HashMap<String, String>, lang: Lang, gate: Gate) {
    let relevant: Vec<QuotaView> = quotas
        .iter()
        .filter(|q| q.origin != QuotaOrigin::Budget && q.used_percent.is_finite())
        .cloned()
        .collect();
    if relevant.is_empty() {
        return;
    }
    let _ = channel().send(Msg::Observe(relevant, displays.clone(), lang, gate));
}

/// Post a test banner through the same gate a real one walks (permission
/// request included), for `TOKENME_TEST_NOTIFY=1` QA runs.
///
/// Delayed a beat: the delivery wait needs the main thread's run loop pumping,
/// and setup runs before `app.run()` starts it — firing there failed with
/// "Mainthread not running" the moment permission was actually granted (it had
/// only looked fine before because a denial short-circuits before the post).
pub fn send_test() {
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(4));
        let _ = channel().send(Msg::Test);
    });
}

/// A new secondary asked to pair: post directly, bypassing the quota-alert
/// gate — this is a person asking at the door, not a threshold event, and it
/// must reach the owner even with quota alerts muted.
pub fn post_pairing_request(device_name: &str, auto_approved: bool) {
    let zh = crate::lang::get() == crate::lang::Lang::Zh;
    let title = if zh { "TokenMe 设备同步" } else { "TokenMe device sync" };
    let body = match (zh, auto_approved) {
        (true, true) => format!("「{device_name}」已免认证接入，同步开始。"),
        (true, false) => format!("「{device_name}」请求接入，请打开同步面板确认。"),
        (false, true) => format!("\"{device_name}\" was auto-approved — sync has started."),
        (false, false) => format!("\"{device_name}\" wants to pair — approve it in the sync panel."),
    };
    if let Err(e) = post(title, &body) {
        crate::logging::error(&format!("notify: pairing toast failed — {e}"));
    }
}

/// The primary cut this device off (learned from a status poll): the panel
/// alone is too easy to miss, so toast it. `reason` is the runtime's short
/// tag ("revoked" / "security" / "rejected").
pub fn post_cut_off(reason: &str) {
    let zh = crate::lang::get() == crate::lang::Lang::Zh;
    let title = if zh { "TokenMe 设备同步" } else { "TokenMe device sync" };
    let body = match (zh, reason) {
        (true, "revoked") => "主端已移除本设备的同步授权，同步已停止。可重新扫描配对恢复。".to_string(),
        (true, "security") => "同步因安全校验失败而停止。请重新扫描配对。".to_string(),
        (true, _) => "配对请求被主端拒绝，同步未建立。".to_string(),
        (false, "revoked") => "The primary removed this device's access — sync stopped. Re-pair to restore.".to_string(),
        (false, "security") => "Sync stopped on a security verification failure. Re-pair to restore.".to_string(),
        (false, _) => "The primary rejected the pairing request — sync was not established.".to_string(),
    };
    if let Err(e) = post(title, &body) {
        crate::logging::error(&format!("notify: cut-off toast failed — {e}"));
    }
}

/// A paired secondary explicitly disconnected: the primary's list is about to
/// refresh; toast the owner so the departure is not silent.
pub fn post_secondary_left(device_name: &str) {
    let zh = crate::lang::get() == crate::lang::Lang::Zh;
    let title = if zh { "TokenMe 设备同步" } else { "TokenMe device sync" };
    let body = if zh {
        format!("「{device_name}」已断开同步连接。")
    } else {
        format!("\"{device_name}\" has disconnected from sync.")
    };
    if let Err(e) = post(title, &body) {
        crate::logging::error(&format!("notify: secondary-left toast failed — {e}"));
    }
}

/// The panel's whole view of the permission: whether the OS will deliver a
/// banner at all, as one word — `granted` / `denied` / `not_determined`, or
/// `unknown` where the platform has no answer (Windows). `TOKENME_NOTIFY_STATE`
/// forces an answer for QA screenshots, same family as `TOKENME_SHOW_PANEL`.
pub fn permission_state() -> String {
    if let Some(forced) = std::env::var("TOKENME_NOTIFY_STATE")
        .ok()
        .and_then(|v| parse_override(&v))
    {
        static NOTED: OnceLock<()> = OnceLock::new();
        NOTED.get_or_init(|| {
            crate::logging::info(&format!(
                "notify: permission state forced to {forced} by TOKENME_NOTIFY_STATE (QA)"
            ));
        });
        return forced.to_string();
    }
    platform_state()
}

fn parse_override(raw: &str) -> Option<&'static str> {
    match raw {
        "granted" => Some("granted"),
        "denied" => Some("denied"),
        "not_determined" => Some("not_determined"),
        "unknown" => Some("unknown"),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn platform_state() -> String {
    match mac_usernotifications::blocking::get_notification_settings() {
        Ok(s) => map_authorization(s.authorization_status).to_string(),
        Err(_) => "unknown".to_string(),
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_state() -> String {
    "unknown".to_string()
}

/// The panel's vocabulary for what the OS reports. Granted is granted whether
/// it is full, provisional or ephemeral — a banner can post under all three.
#[cfg(target_os = "macos")]
fn map_authorization(status: mac_usernotifications::AuthorizationStatus) -> &'static str {
    use mac_usernotifications::AuthorizationStatus as S;
    match status {
        S::Authorized | S::Provisional | S::Ephemeral => "granted",
        S::Denied => "denied",
        S::NotDetermined => "not_determined",
        S::Unknown => "unknown",
    }
}

/// The permission, for the quota section's guide.
#[tauri::command]
pub async fn notify_status() -> String {
    tauri::async_runtime::spawn_blocking(permission_state)
        .await
        .unwrap_or_else(|_| "unknown".to_string())
}

/// The guide's one button: an undecided permission raises the system prompt
/// (the prompt itself is the decision UI, and macOS never re-prompts a denial),
/// a denial opens the settings pane that can undo it, anything else is a no-op.
/// Returns the state the click was routed on; the panel watches for the answer.
#[tauri::command]
pub async fn notify_enable(app: tauri::AppHandle) -> Result<String, String> {
    let state = tauri::async_runtime::spawn_blocking(permission_state)
        .await
        .map_err(|e| e.to_string())?;
    match state.as_str() {
        "not_determined" => {
            ask_permission()?;
            Ok(state)
        }
        "denied" => {
            open_notification_settings(app.config().identifier.clone())?;
            Ok(state)
        }
        other => Ok(other.to_string()),
    }
}

/// The request blocks until the user answers — on its own thread, so the
/// command (and the panel) is free while the prompt is up.
#[cfg(target_os = "macos")]
fn ask_permission() -> Result<(), String> {
    crate::logging::info("notify: the panel asked; raising the system permission prompt");
    thread::spawn(|| match mac_usernotifications::blocking::request_auth() {
        Ok(true) => crate::logging::info("notify: permission granted by the user"),
        Ok(false) => crate::logging::info("notify: permission denied by the user"),
        Err(e) => crate::logging::error(&format!("notify: permission request failed — {e}")),
    });
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn ask_permission() -> Result<(), String> {
    Err("the system permission prompt is only wired on macOS".into())
}

/// macOS deep-links the app's own notification pane; the identifier rides in
/// `?id=` so the user lands on TokenMe, not the list.
#[cfg(target_os = "macos")]
fn open_notification_settings(identifier: String) -> Result<(), String> {
    let url = format!(
        "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id={identifier}"
    );
    crate::logging::info("notify: opening the system notification settings pane");
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "macos"))]
fn open_notification_settings(_identifier: String) -> Result<(), String> {
    Err("the notification settings pane is only reachable on macOS".into())
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
            Msg::Observe(quotas, displays, lang, gate) => w.observe_views(&quotas, &displays, lang, &gate),
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

/// `Denied`/`NotDetermined` are produced only by the macOS permission flow,
/// but every platform matches them in `note_failure`, so they cannot be
/// cfg'd away — elsewhere they are simply never constructed.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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

    fn observe_views(
        &mut self,
        quotas: &[QuotaView],
        displays: &HashMap<String, String>,
        lang: Lang,
        gate: &Gate,
    ) {
        let now = now_ms();
        let mut dirty = false;
        for q in quotas {
            let key = key_of(q);
            let prev = self.state.entries.get(&key).cloned();
            let decision = plan(prev.as_ref(), tier_of(q.used_percent), q.resets_at_ms, now, q.used_percent);
            let mut entry = decision.entry;
            if let Some(tier) = decision.fire {
                if !gate.allows(&q.tool, tier) {
                    // Quiet, not forgotten: the window moves to where it is.
                    entry.tier = tier;
                    dirty = true;
                    if self.noted_once.insert(format!("{key}:muted")) {
                        crate::logging::info(&format!(
                            "notify: 按设置静默 [{key}] tier {tier} / kept quiet by settings, tier recorded"
                        ));
                    }
                } else if self.attempt_due(&key) {
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
            if is_package_expiry(q)
                && !entry.expiry_notified
                && expiry_due(q.resets_at_ms, now, gate.plan_expiry_notify_days)
            {
                if !gate.allows_expiry(&q.tool) {
                    // Quiet, not forgotten: changing the setting later does not
                    // replay a reminder that was already due while disabled.
                    entry.expiry_notified = true;
                    dirty = true;
                    if self.noted_once.insert(format!("{key}:expiry-muted")) {
                        crate::logging::info(&format!(
                            "notify: 按设置静默 [{key}] package expiry / kept quiet by settings"
                        ));
                    }
                } else if let Some(days) = gate.plan_expiry_notify_days {
                    if self.attempt_due(&format!("{key}:expiry")) {
                        let display = displays.get(&q.tool).map(String::as_str).unwrap_or(&q.tool);
                        let (title, body) = copy_for_expiry(display, q.label.as_deref(), days, lang);
                        match self.deliver(&format!("{key}:expiry"), &title, &body) {
                            Ok(()) => {
                                entry.expiry_notified = true;
                                dirty = true;
                                crate::logging::info(&format!("notify: posted [{key}] {title} — {body}"));
                            }
                            Err(e) => self.note_failure(&format!("{key}:expiry"), e),
                        }
                    }
                }
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
                "通知已就绪 — 额度越线或套餐临近到期时会在此提醒",
                "Notifications are ready — quota crossings and package expiry reminders will appear here",
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
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn post(title: &str, body: &str) -> Result<(), String> {
    // notify-rust's UN path probes the main run loop from the calling thread
    // and answers "Mainthread not running" when that one-instant snapshot
    // finds the loop mid-dispatch — four passes in a row on 2026-10-13 while
    // the app was alive and polling. Called ON the main thread the probe is
    // bypassed (block_on_main owns the loop and waits for the delegate), so
    // the post rides the app's own dispatch instead of racing it.
    if let Some(app) = APP.get() {
        let (title, body) = (title.to_string(), body.to_string());
        let (tx, rx) = std::sync::mpsc::channel();
        let shown = app.run_on_main_thread(move || {
            let _ = tx.send(
                notify_rust::Notification::new()
                    .summary(&title)
                    .body(&body)
                    .show()
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            );
        });
        if let Err(e) = shown {
            return Err(format!("main-thread dispatch failed: {e}"));
        }
        return rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| Err("main-thread delivery timed out".into()));
    }
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

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
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
            cycle: None,
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
    fn expiry_alerts_fire_only_inside_the_selected_lead_time() {
        assert!(!expiry_due(NOW + 5 * 86_400_000 + 1, NOW, Some(5)));
        assert!(expiry_due(NOW + 5 * 86_400_000, NOW, Some(5)));
        assert!(expiry_due(NOW + 86_400_000, NOW, Some(5)));
        assert!(!expiry_due(NOW, NOW, Some(5)), "an already expired package is not a before-expiry reminder");
        assert!(!expiry_due(NOW + 86_400_000, NOW, None));
    }

    #[test]
    fn a_new_expiry_instance_rearms_the_package_reminder() {
        let previous = Entry {
            resets_at_ms: RESET,
            expiry_notified: true,
            ..Entry::default()
        };
        let next = plan(Some(&previous), 0, RESET + RENEW_SLACK_MS + 1, NOW, 0.0);
        assert!(!next.entry.expiry_notified, "a renewed package gets its own reminder");
    }

    #[test]
    fn only_zero_length_package_rows_are_expiry_candidates() {
        let mut pack = view("workbuddy", 12.0, NOW + 86_400_000, Some("Bonus Pack"), Some("pack"));
        pack.window_minutes = 0;
        assert!(is_package_expiry(&pack));
        pack.tool = "cola".into();
        assert!(!is_package_expiry(&pack), "Cola's zero-length row is a daily reset, not package expiry");
        pack.tool = "workbuddy".into();
        pack.window_minutes = 300;
        assert!(!is_package_expiry(&pack));
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
        let quiet = plan(None, 0, RESET, NOW, 0.0);
        assert_eq!(quiet.fire, None);
        assert!(quiet.persist, "the baseline must survive a restart");
        let warned = plan(None, 1, RESET, NOW, 85.0);
        assert_eq!(warned.fire, None, "the crossing predates this process — no banner");
        let done = plan(None, 2, RESET, NOW, 100.0);
        assert_eq!(done.fire, Some(2), "已经用完 is the alarm; meeting it quietly is wrong");
        assert_eq!(done.entry.tier, 0, "the announcement is pending until it actually posts");
    }

    #[test]
    fn a_refused_announcement_is_offered_again_and_not_recorded_as_told() {
        // First sight of an exhausted window whose banner the system refused:
        // the persisted entry stays at the pre-announcement baseline, so the
        // next pass offers the banner again — a retry, not a silent swallow.
        let first = plan(None, 2, RESET, NOW, 100.0);
        assert_eq!(first.fire, Some(2));
        let retry = plan(Some(&first.entry), 2, RESET, NOW + 60_000, 100.0);
        assert_eq!(retry.fire, Some(2), "still owed to the user");
        assert_eq!(retry.entry.tier, 0);
        // Once a post lands, the caller advances the tier and the line quiets.
        let settled = Entry { tier: 2, last_fired_ms: NOW, ..first.entry.clone() };
        assert_eq!(plan(Some(&settled), 2, RESET, NOW + 120_000, 100.0).fire, None);
        // A renewal met at 100 % is the same contract: announced, or retried.
        let old = Entry { tier: 1, resets_at_ms: RESET, last_fired_ms: NOW, last_seen_ms: NOW, ..Entry::default() };
        let renewed = plan(Some(&old), 2, RESET + RENEW_SLACK_MS + 1, NOW, 100.0);
        assert_eq!(renewed.fire, Some(2));
        assert_eq!(renewed.entry.tier, 0, "fresh instance, announcement pending");
    }

    #[test]
    fn crossing_a_line_announces_it_and_only_advances_on_that_post() {
        let prev = Entry { tier: 0, resets_at_ms: RESET, last_fired_ms: 0, last_seen_ms: NOW, ..Entry::default() };
        let d = plan(Some(&prev), 1, RESET, NOW, 85.0);
        assert_eq!(d.fire, Some(1));
        assert_eq!(d.entry.tier, 0, "the tier advances in the caller, after the post");
        assert!(!d.persist, "nothing durable happened yet");
        // Straight past both lines in one sample: one banner, the stronger tier.
        let d = plan(Some(&prev), 2, RESET, NOW, 100.0);
        assert_eq!(d.fire, Some(2));
        // The same observation again: already claimed, nothing to say.
        let at_one = Entry { tier: 1, ..prev.clone() };
        let d = plan(Some(&at_one), 1, RESET, NOW, 85.0);
        assert_eq!(d.fire, None);
        assert!(!d.persist);
    }

    #[test]
    fn a_drop_rearms_the_line_silently() {
        // Fired long enough ago that the refire cooldown has already run out.
        let down = Entry {
            tier: 2,
            resets_at_ms: RESET,
            last_fired_ms: NOW - EXHAUSTED_REFIRE_COOLDOWN_MS - 1,
            last_seen_ms: NOW,
            ..Entry::default()
        };
        let d = plan(Some(&down), 0, RESET, NOW, 0.0);
        assert_eq!(d.fire, None, "falling back never announces");
        assert_eq!(d.entry.tier, 0);
        assert!(d.persist, "the re-arm must survive a restart");
        // And the next climb announces again.
        let d = plan(Some(&d.entry), 2, RESET, NOW + 1_000, 100.0);
        assert_eq!(d.fire, Some(2));
    }

    #[test]
    fn reset_jitter_is_not_a_renewal_but_a_jump_is() {
        let prev = Entry { tier: 1, resets_at_ms: RESET, last_fired_ms: 0, last_seen_ms: NOW, ..Entry::default() };
        // Seconds of wobble between samples: same instance, same tier, quiet.
        let d = plan(Some(&prev), 1, RESET + 60_000, NOW, 85.0);
        assert_eq!(d.fire, None);
        assert_eq!(d.entry.resets_at_ms, RESET + 60_000);
        // A reset that jumped is the vendor's next window: fresh baseline, no
        // "re-arm" for a line nobody is below.
        let d = plan(Some(&prev), 0, RESET + RENEW_SLACK_MS + 1, NOW, 0.0);
        assert_eq!(d.fire, None);
        assert_eq!(d.entry.tier, 0, "a renewed window baselines at what it shows");
        assert_eq!(d.entry.last_fired_ms, 0, "the old instance's fire time does not carry over");
        // A renewed window met at 100 % announces once, like any first sight.
        let d = plan(Some(&prev), 2, RESET + RENEW_SLACK_MS + 1, NOW, 100.0);
        assert_eq!(d.fire, Some(2));
        assert_eq!(d.entry.tier, 0, "pending like a first sight — the post settles it");
        // A window that never advertises a reset cannot renew by reset; the
        // drop re-arm covers its top-ups instead.
        let no_reset = Entry { tier: 2, resets_at_ms: 0, last_fired_ms: NOW - 1, last_seen_ms: NOW, ..Entry::default() };
        let d = plan(Some(&no_reset), 0, 0, NOW, 0.0);
        assert_eq!(d.fire, None);
        assert_eq!(d.entry.tier, 0);
    }

    /// The 100 % banner is sticky across sub-line wobble. A vendor reporting
    /// 99.8 one poll and 100 the next is jittering on the line, not topping
    /// up — re-arming on the dip re-fired the banner on every crossing
    /// (measured 2026-10-10: intermittent repeats through the morning). The
    /// window stays spent until a reading clears the hysteresis margin, and
    /// only then may the next climb announce again.
    #[test]
    fn a_spent_window_ignores_sub_line_wobble() {
        let spent = Entry { tier: 2, resets_at_ms: RESET, last_fired_ms: NOW, last_seen_ms: NOW, ..Entry::default() };
        // 99.8 while spent: wobble, not a top-up — the window stays spent.
        let wobble = plan(Some(&spent), 1, RESET, NOW + 1_000, 99.8);
        assert_eq!(wobble.fire, None, "the same spent window must not re-fire");
        assert_eq!(wobble.entry.tier, 2, "and it is not re-armed by the wobble");
        assert!(!wobble.persist);
        // The next 100 is that same spent window: quiet.
        let again = plan(Some(&wobble.entry), 2, RESET, NOW + 2_000, 100.0);
        assert_eq!(again.fire, None);
        // A real top-up clears the margin and re-arms — but the climb seconds
        // later still sits inside the refire cooldown of the announcement this
        // window already made: quiet. (The announce-after-a-real-refill case
        // lives in `a_drop_rearms_the_line_silently`, whose fire is hours old.)
        let topped = plan(Some(&spent), 0, RESET, NOW + 3_000, 0.0);
        assert_eq!(topped.fire, None);
        assert_eq!(topped.entry.tier, 0);
        assert!(topped.persist, "the re-arm must survive a restart");
        let climbed = plan(Some(&topped.entry), 2, RESET, NOW + 4_000, 100.0);
        assert_eq!(climbed.fire, None, "one banner per window per cooldown");
    }

    /// The cooldown is the last line under the one-banner contract: even when
    /// upstream churn re-arms a spent window (the collided window ids this
    /// shipped with, measured 2026-10-10 — twelve banners in an hour), a climb
    /// inside the cooldown stays quiet, because `last_fired_ms` survives the
    /// re-arm and the clock reads the real post.
    #[test]
    fn a_recently_fired_window_stays_quiet_through_a_rearm_and_climb() {
        let spent = Entry { tier: 2, resets_at_ms: RESET, last_fired_ms: NOW, last_seen_ms: NOW, ..Entry::default() };
        let topped = plan(Some(&spent), 0, RESET, NOW + 60_000, 0.0);
        assert_eq!(topped.entry.tier, 0, "the re-arm itself is real and persists");
        let climbed = plan(Some(&topped.entry), 2, RESET, NOW + 120_000, 100.0);
        assert_eq!(climbed.fire, None, "inside the cooldown: quiet, not another banner");
        assert!(!climbed.persist);
        // Quiet does not mean forgotten: the window stays re-armed, and once
        // the cooldown runs out the next climb announces again.
        assert_eq!(climbed.entry.tier, 0);
        let later = plan(Some(&climbed.entry), 2, RESET, NOW + EXHAUSTED_REFIRE_COOLDOWN_MS + 1, 100.0);
        assert_eq!(later.fire, Some(2), "past the cooldown the climb announces like any other");
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
    fn expiry_copy_names_the_selected_lead_time() {
        let (title, body) = copy_for_expiry("WorkBuddy", Some("Bonus Pack"), 3, Lang::Zh);
        assert_eq!(title, "WorkBuddy · Bonus Pack");
        assert_eq!(body, "套餐将在 3 天后到期");
        let (_, body) = copy_for_expiry("WorkBuddy", None, 1, Lang::En);
        assert_eq!(body, "Package expires in 1 day");
    }

    /// The title's engine-authored label follows the banner language: the
    /// known nouns and meter verbs translate, vendor plan names and unknown
    /// shapes pass through, and zh reads byte-identical so the label-derived
    /// hysteresis keys never move.
    #[test]
    fn the_title_label_follows_the_language() {
        // zh passthrough, byte for byte.
        let (title, _) = copy_for("妙手", Some("套餐积分 · 已用 0/1200"), 0.0, 1, Lang::Zh);
        assert_eq!(title, "妙手 · 套餐积分 · 已用 0/1200");
        // The pack labels: known head + the used verb.
        let (title, _) = copy_for("妙手", Some("套餐积分 · 已用 0/1200"), 0.0, 1, Lang::En);
        assert_eq!(title, "妙手 · Plan credits · used 0/1200");
        let (title, _) = copy_for("Qoder", Some("专属资源包 · 已用 130/600"), 21.6, 1, Lang::En);
        assert_eq!(title, "Qoder · Dedicated add-on pack · used 130/600");
        let (title, _) = copy_for("WorkBuddy", Some("Bonus Pack · 已用 250/310"), 80.6, 1, Lang::En);
        assert_eq!(title, "WorkBuddy · Bonus Pack · used 250/310", "vendor plan names stay");
        // Window names and plan suffixes.
        let (title, _) = copy_for("ZCode", Some("5 小时 · GLM Coding Lite"), 85.0, 1, Lang::En);
        assert_eq!(title, "ZCode · 5 hours · GLM Coding Lite");
        let (title, _) = copy_for("Codex", Some("primary_window · 15 分钟"), 90.0, 1, Lang::En);
        assert_eq!(title, "Codex · primary_window · 15 min");
        // Remaining-first compositions (cola, agnes).
        let (title, _) = copy_for("Cola", Some("breeze 套餐 · 剩余 30% · 试用"), 70.0, 1, Lang::En);
        assert_eq!(title, "Cola · breeze plan · 30% left · trial");
        let (title, _) = copy_for("AgnesCode", Some("剩余 8578 点 · 限时 1240 · 每日刷新 7338"), 40.0, 1, Lang::En);
        assert_eq!(title, "AgnesCode · 8578 pts left · limited 1240 · daily refresh 7338");
        // Balance heads and the auto-claimed marker.
        let (title, _) = copy_for("DSH", Some("余额 ¥26.70"), 0.0, 1, Lang::En);
        assert_eq!(title, "DSH · Balance ¥26.70");
        let (title, _) = copy_for("MiniMax Code", Some("已自动签到 · +30"), 0.0, 1, Lang::En);
        assert_eq!(title, "MiniMax Code · Auto-claimed · +30");
        // A limit-less window.
        let (title, _) = copy_for("Cline", Some("Free · 无可查限额"), 0.0, 1, Lang::En);
        assert_eq!(title, "Cline · Free · No visible limit");
    }

    #[test]
    fn state_survives_json_and_older_shapes() {
        let mut state = NotifyState::default();
        state.entries.insert("qoder|qw-5h|300".into(), Entry { tier: 2, resets_at_ms: RESET, last_fired_ms: NOW, last_seen_ms: NOW, ..Entry::default() });
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
        state.entries.insert("fresh".into(), Entry { tier: 1, resets_at_ms: 0, last_fired_ms: 0, last_seen_ms: NOW - 1_000, ..Entry::default() });
        state.entries.insert("stale".into(), Entry { tier: 2, resets_at_ms: 0, last_fired_ms: 0, last_seen_ms: NOW - STATE_RETENTION_MS - 1, ..Entry::default() });
        prune_at(&mut state, NOW);
        assert!(state.entries.contains_key("fresh"));
        assert!(!state.entries.contains_key("stale"), "a window gone for a month is gone");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn authorization_status_maps_onto_the_panels_vocabulary() {
        use mac_usernotifications::AuthorizationStatus as S;
        // Any flavour of allowed means a banner can post.
        assert_eq!(map_authorization(S::Authorized), "granted");
        assert_eq!(map_authorization(S::Provisional), "granted");
        assert_eq!(map_authorization(S::Ephemeral), "granted");
        assert_eq!(map_authorization(S::Denied), "denied");
        assert_eq!(map_authorization(S::NotDetermined), "not_determined");
        assert_eq!(map_authorization(S::Unknown), "unknown");
    }

    #[test]
    fn the_qa_override_only_accepts_the_vocabulary() {
        for ok in ["granted", "denied", "not_determined", "unknown"] {
            assert_eq!(parse_override(ok), Some(ok));
        }
        // Anything else is a typo, not a state — the real query must answer.
        assert_eq!(parse_override("Granted"), None);
        assert_eq!(parse_override(""), None);
        assert_eq!(parse_override("yes"), None);
    }

    /// The gate decides who may post. It never decides what the tier machine
    /// remembers — that distinction is why un-muting a tool does not retro-fire
    /// the line it crossed while quiet.
    #[test]
    fn the_gate_filters_delivery_not_the_tier_state() {
        let mut s = Settings::default();
        let g = Gate::from(&s);
        assert!(g.allows("codex", 1) && g.allows("codex", 2));
        assert!(g.allows_expiry("codex"));
        assert!(!g.allows("codex", 0), "a window below the warning line never posts");

        s.notify_tiers = NotifyTier::Exhausted;
        let g = Gate::from(&s);
        assert!(!g.allows("codex", 1), "80 % stays quiet when only exhaustion is kept");
        assert!(g.allows("codex", 2));

        s.notify_tiers = NotifyTier::Both;
        s.notify_muted = vec!["qoder".into()];
        let g = Gate::from(&s);
        assert!(!g.allows("qoder", 2), "a muted tool never posts");
        assert!(g.allows("codex", 1), "muting one tool says nothing about the others");

        s.notify_tiers = NotifyTier::Off;
        assert!(!Gate::from(&s).allows("codex", 2), "off is off for everyone");

        s.plan_expiry_notify_days = None;
        assert!(!Gate::from(&s).allows_expiry("codex"));
        s.plan_expiry_notify_days = Some(1);
        s.notify_muted = vec!["codex".into()];
        assert!(!Gate::from(&s).allows_expiry("codex"));
    }
}
