use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What the menu-bar title shows. `Cost` is the default; `Tokens` and `Quiet`
/// exist because a fixed-width money figure is not always what a user wants
/// glued to their status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrayMode {
    #[default]
    Cost,
    Tokens,
    Quiet,
}

impl TrayMode {
    pub const ALL: [TrayMode; 3] = [TrayMode::Cost, TrayMode::Tokens, TrayMode::Quiet];
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

/// The engine's fallback poll cadence when the file watcher is quiet.
fn default_refresh_secs() -> u64 {
    30
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub tray_mode: TrayMode,
    pub autostart: bool,
    pub theme: Theme,
    /// Fallback poll cadence; watcher wake-ups still re-index immediately.
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
    pub budgets: BTreeMap<String, usage_core::Budget>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quota_tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quota_rows: Vec<String>,
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
