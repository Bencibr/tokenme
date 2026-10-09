use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What the menu-bar status item shows: the six combinations of
/// {icon?} × {tokens?} × {cost?} — exactly these, nothing between them.
/// The `alias`es keep settings.json written by the old three-mode era loading
/// (cost/tokens/quiet map onto their closest new mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrayMode {
    /// 仅托盘 — icon only, no text.
    #[serde(alias = "quiet")]
    TrayOnly,
    /// 仅Token — text only, icon hidden.
    TokensOnly,
    /// 仅花费 — text only, icon hidden.
    CostOnly,
    /// 托盘·Token — the branded default.
    #[default]
    TrayTokens,
    /// 托盘·花费
    #[serde(alias = "cost")]
    TrayCost,
    /// 托盘·Token·花费
    TrayTokensCost,
}

impl TrayMode {
    /// Whether the dual-ring icon is part of the display.
    pub fn shows_icon(self) -> bool {
        !matches!(self, TrayMode::TokensOnly | TrayMode::CostOnly)
    }
}

/// Panel appearance. New installs use the dark palette; `System` remains
/// available when the user explicitly wants the OS media query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

/// The edge bubble's appearance; old settings keep the original waterdrop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BubbleSkin {
    #[default]
    Waterdrop,
    Kitten,
}

/// Which of the two banner lines the tier machine may announce. `Both` is the
/// shipped behaviour; `Off` silences banners without touching the tier state,
/// so switching back does not re-announce windows that already crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotifyTier {
    /// Warn at 80 % and again at 100 %.
    #[default]
    Both,
    /// Only the exhaustion line.
    Exhausted,
    /// No banners; the quota section still updates.
    Off,
}

impl NotifyTier {
    /// The lowest tier that may post, or `None` when nothing may. The tier
    /// machine numbers its lines 1 = 80 % and 2 = 100 %, so the threshold rule
    /// lives here once and `notify::Gate` only asks it.
    pub fn min_tier(self) -> Option<u8> {
        match self {
            NotifyTier::Both => Some(1),
            NotifyTier::Exhausted => Some(2),
            NotifyTier::Off => None,
        }
    }
}

/// Checks run at boot; offering an update the user would otherwise miss is the
/// point of shipping a latest.json at all.
fn default_auto_update_check() -> bool {
    true
}

/// The engine's fallback poll cadence when the file watcher is quiet.
fn default_refresh_secs() -> u64 {
    30
}

/// Dollar figures default OFF on every platform: most metered tools here are
/// points/credit-based, so the computed figure is a live estimate, not a
/// bill, and tokens/credits read cleaner without it. settings.json keeps the
/// user's explicit choice when the field is present.
fn default_show_money() -> bool {
    false
}

/// The Windows-only edge bubble is opt-out: it is deliberately absent from
/// non-Windows settings UI, but old settings files still get the safe default.
fn default_bubble_enabled() -> bool {
    true
}

/// See the field: pausing an absent host's probes is the polite default.
fn default_host_exit_pause() -> bool {
    true
}

/// Polling is the feature; the switch exists to stop it, not to start it.
fn default_quota_polling() -> bool {
    true
}

/// The whole persisted surface: a tiny JSON file under the platform config dir.
///
/// `budgets` is shared with the CLI: `tokenme budget set …` writes that one key of
/// the same file, so a cap set in a terminal reaches the panel on the next refresh
/// without either side owning the file.
///
/// `quota_tools`/`quota_rows` are the drag order of the quota section: tool ids,
/// and `<tool>/<row id>` pairs. Absent entries fall back to the report's own
/// (rank-stable) order, so a window that appears for the first time just joins
/// the end instead of scrambling the saved arrangement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub tray_mode: TrayMode,
    pub autostart: bool,
    pub theme: Theme,
    /// Fallback poll cadence; watcher wake-ups still re-index immediately.
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
    /// Whether converted-API-dollar figures render at all. Subscribers pay a
    /// fixed plan, so the number is an equal-value yardstick, not a bill — and
    /// some users would rather not see it.
    #[serde(default = "default_show_money")]
    pub show_money: bool,
    /// Also list tools whose session count for the period is zero on the
    /// tools page. Default off: a tool that did nothing this window is noise,
    /// and the page's job is comparing what ran. The tools list still keeps
    /// detected-but-quiet sources discoverable by turning this on.
    #[serde(default)]
    pub show_empty_tools: bool,
    #[serde(default = "default_bubble_enabled")]
    pub bubble_enabled: bool,
    #[serde(default)]
    pub bubble_skin: BubbleSkin,
    /// Stop asking vendors for a tool's quota once its host application has
    /// exited: the number cannot change, and the calls are the user's own
    /// account traffic. Default on; the last known answer stays on screen.
    #[serde(default = "default_host_exit_pause")]
    pub host_exit_pause: bool,
    /// The daily check-in guarantee for the tools that own a claim (Trae CN,
    /// Qoder): on, the host-exit pause stops applying to them — a claim is an
    /// HTTPS call against the account, not a read of the host's state, so a
    /// closed IDE is no reason to miss a day, and their rows stay live in the
    /// strip. Off keeps the opportunistic claim: it fires only while the host
    /// runs. The master switch and the per-tool switches stand above this
    /// either way.
    #[serde(default)]
    pub auto_checkin: bool,
    /// The master switch for vendor quota polling. Off means one request fewer:
    /// no HTTP, and no `gh` / `agy` child process (the Windows console-flash
    /// family of side effects dies here too). The quota section keeps its last
    /// answer and says it stopped — a switch that blanks the numbers the user
    /// was reading is a switch that gets turned back on in annoyance.
    #[serde(default = "default_quota_polling")]
    pub quota_polling: bool,
    /// Tools whose probe the user stopped by hand, while the master switch stays
    /// on. This is the only lever for the probes with no host mapping
    /// (`copilot`, `gemini`, `kimicode`, `minimaxcode`): `host_exit_pause`
    /// cannot see a process that never exists.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub quota_probes_off: Vec<String>,
    /// Which banner lines fire.
    #[serde(default)]
    pub notify_tiers: NotifyTier,
    /// Tools that never post a banner, whatever their windows do.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notify_muted: Vec<String>,
    /// Check GitHub's latest.json on boot and offer the update when a newer
    /// release exists. A check is one anonymous GET — nothing is downloaded and
    /// no install runs unless the user clicks the offer in the settings sheet.
    #[serde(default = "default_auto_update_check")]
    pub auto_update_check: bool,
    pub budgets: BTreeMap<String, usage_core::Budget>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quota_tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quota_rows: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tray_mode: TrayMode::default(),
            autostart: false,
            theme: Theme::default(),
            refresh_secs: default_refresh_secs(),
            show_money: default_show_money(),
            show_empty_tools: false,
            bubble_enabled: default_bubble_enabled(),
            bubble_skin: BubbleSkin::default(),
            host_exit_pause: default_host_exit_pause(),
            auto_checkin: false,
            quota_polling: default_quota_polling(),
            quota_probes_off: Vec::new(),
            notify_tiers: NotifyTier::default(),
            notify_muted: Vec::new(),
            auto_update_check: default_auto_update_check(),
            budgets: BTreeMap::new(),
            quota_tools: Vec::new(),
            quota_rows: Vec::new(),
        }
    }
}

impl Settings {
    /// Whether this tool's probe may run this pass: the master switch first, then
    /// the user's own exclusions. The host-exit test composes on top of this in
    /// `engine::quota_pass`, because that one is re-decided every pass while
    /// these two are the user's standing instruction.
    pub fn probe_allowed(&self, tool: &str) -> bool {
        self.quota_polling && !self.quota_probes_off.iter().any(|t| t == tool)
    }

    /// The one path both the app and `tokenme budget` use.
    pub fn path() -> Option<PathBuf> {
        usage_core::budget::settings_path()
    }

    /// A corrupt or unreadable file must not stop the app from launching.
    pub fn load() -> Self {
        Self::path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::path().ok_or_else(|| io::Error::other("no platform config dir"))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bubble_skin_defaults_to_waterdrop_for_old_settings() {
        let s: Settings = serde_json::from_str(
            r#"{"tray_mode":"tray_tokens","autostart":false,"theme":"light",
                "bubble_enabled":false,"quota_polling":false,
                "quota_probes_off":["copilot"],"budgets":{}}"#,
        ).expect("a settings file without bubble_skin must still load");
        assert_eq!(s.bubble_skin, BubbleSkin::Waterdrop);
        assert_eq!(Settings::default().bubble_skin, BubbleSkin::Waterdrop);
        assert_eq!(s.theme, Theme::Light);
        assert!(!s.bubble_enabled);
        assert!(!s.quota_polling);
        assert_eq!(s.quota_probes_off, vec!["copilot".to_string()]);
    }

    #[test]
    fn bubble_skins_round_trip_with_the_event_and_settings_names() {
        for (name, skin) in [("waterdrop", BubbleSkin::Waterdrop), ("kitten", BubbleSkin::Kitten)] {
            let payload = serde_json::to_value(skin).unwrap();
            assert_eq!(payload, serde_json::json!(name), "event payload must be a string");
            assert_eq!(serde_json::from_value::<BubbleSkin>(payload).unwrap(), skin);
            let s = Settings { bubble_skin: skin, ..Settings::default() };
            let json = serde_json::to_value(&s).unwrap();
            assert_eq!(json["bubble_skin"], serde_json::json!(name));
            assert_eq!(serde_json::from_value::<Settings>(json).unwrap().bubble_skin, skin);
        }
    }

    #[test]
    fn bubble_skin_rejects_invalid_names() {
        for name in ["dog", "water_drop", "Waterdrop", "Kitten", ""] {
            assert!(serde_json::from_value::<BubbleSkin>(serde_json::json!(name)).is_err(), "{name}");
            assert!(serde_json::from_value::<Settings>(serde_json::json!({"bubble_skin": name})).is_err(), "{name}");
        }
    }

    /// What a settings.json written before this feature looks like: none of the
    /// new keys. Loading it must keep polling on and banners at both lines —
    /// an upgrade that silently stops probing is worse than no switch at all.
    #[test]
    fn a_pre_switch_file_keeps_polling_and_both_tiers() {
        let s: Settings = serde_json::from_str(
            r#"{"tray_mode":"tray_tokens","autostart":false,"theme":"light","refresh_secs":30,
                "show_money":false,"show_empty_tools":false,"bubble_enabled":true,
                "host_exit_pause":true,"auto_update_check":true,"budgets":{}}"#,
        )
        .expect("the shipped shape must still parse");
        assert!(s.quota_polling);
        assert!(s.quota_probes_off.is_empty());
        assert_eq!(s.notify_tiers, NotifyTier::Both);
        assert!(s.notify_muted.is_empty());
        assert!(!s.auto_checkin, "the guarantee is opt-in, never implied by an old file");
    }

    /// The auto check-in guarantee widens what leaves the machine — claims
    /// fire with the host closed — so only an explicit `true` turns it on.
    #[test]
    fn the_auto_checkin_guarantee_is_opt_in() {
        assert!(!Settings::default().auto_checkin);
        let on: Settings = serde_json::from_str(r#"{"auto_checkin":true}"#).unwrap();
        assert!(on.auto_checkin);
    }

    #[test]
    fn empty_lists_stay_out_of_the_file() {
        let json = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!json.contains("quota_probes_off"), "{json}");
        assert!(!json.contains("notify_muted"), "{json}");
        // The two switches that change behaviour are always written, so a
        // hand-edited file cannot leave them ambiguous.
        assert!(json.contains("\"quota_polling\":true"), "{json}");
    }

    #[test]
    fn the_master_switch_overrides_every_per_tool_choice() {
        let mut s = Settings::default();
        assert!(s.probe_allowed("codex"));
        s.quota_probes_off = vec!["copilot".into()];
        assert!(!s.probe_allowed("copilot"));
        assert!(s.probe_allowed("codex"));
        s.quota_polling = false;
        assert!(!s.probe_allowed("codex"), "off means not one request leaves");
        assert!(!s.probe_allowed("copilot"));
    }

    /// The tier-choice → tier-number map the banner gate reads. Muting and delivery
    /// live in `notify::Gate`, whose own test covers them; this is the half the
    /// settings model owns, and the only place the 80 %/100 % pair is spelled.
    #[test]
    fn the_tier_choice_names_the_lowest_line_that_may_post() {
        assert_eq!(NotifyTier::Both.min_tier(), Some(1));
        assert_eq!(NotifyTier::Exhausted.min_tier(), Some(2));
        assert_eq!(NotifyTier::Off.min_tier(), None, "off must be quiet for everyone");
        assert_eq!(NotifyTier::default(), NotifyTier::Both, "the shipped pair stays the default");
    }

    #[test]
    fn tiers_round_trip_through_their_serialised_names() {
        for (json, want) in [
            (r#""both""#, NotifyTier::Both),
            (r#""exhausted""#, NotifyTier::Exhausted),
            (r#""off""#, NotifyTier::Off),
        ] {
            let s: Settings =
                serde_json::from_str(&format!(r#"{{"notify_tiers":{json}}}"#)).unwrap();
            assert_eq!(s.notify_tiers, want, "{json}");
        }
    }
}
