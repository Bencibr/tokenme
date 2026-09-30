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

/// Panel appearance. `System` leaves the OS media query in charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
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

/// Dollar figures default on: they are the one scale that compares models
/// across tools. Off leaves pure tokens/credits.
fn default_show_money() -> bool {
    true
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
    #[serde(default = "default_bubble_enabled")]
    pub bubble_enabled: bool,
    /// Stop asking vendors for a tool's quota once its host application has
    /// exited: the number cannot change, and the calls are the user's own
    /// account traffic. Default on; the last known answer stays on screen.
    #[serde(default = "default_host_exit_pause")]
    pub host_exit_pause: bool,
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
            bubble_enabled: default_bubble_enabled(),
            host_exit_pause: default_host_exit_pause(),
            auto_update_check: default_auto_update_check(),
            budgets: BTreeMap::new(),
            quota_tools: Vec::new(),
            quota_rows: Vec::new(),
        }
    }
}

impl Settings {
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
