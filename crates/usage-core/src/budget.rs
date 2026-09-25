//! Spend caps the user sets in tokenme itself.
//!
//! ## Why this exists next to the vendor probes
//!
//! Most tools simply do not report a limit: Cline keeps no budget anywhere on
//! disk (its `~/.cline/data` holds sessions and provider config, no quota), and
//! ZCode's `session_target.token_budget` is NULL unless a user typed an objective
//! budget. A panel that only shows vendor-reported windows is therefore blank for
//! exactly the tools a user most wants a ceiling on. A budget the user declares
//! here is a real answer to "how much of my limit have I used" — it is *our*
//! number, computed from *our* cost, so it is labelled
//! [`QuotaOrigin::Budget`] and never mixed with a vendor's own window in the copy.
//!
//! Money, not tokens, because that is the unit a person can decide a limit in
//! without knowing each vendor's price sheet — and the cost side already exists
//! centrally in [`crate::pricing`].
//!
//! ## Storage
//!
//! The same `tokenme/settings.json` the menu-bar app writes, under the platform
//! config dir. Only the `budgets` key is read or rewritten here, and the rest of
//! the file is preserved as-is, so the app's tray/autostart settings survive a
//! `tokenme budget set …`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{QuotaOrigin, QuotaView};

/// A tool's own caps. Zero means "no cap set for that window", which is
/// different from a cap of zero dollars.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default)]
    pub daily_usd: f64,
    #[serde(default)]
    pub monthly_usd: f64,
}

impl Budget {
    pub fn is_empty(&self) -> bool {
        self.daily_usd <= 0.0 && self.monthly_usd <= 0.0
    }
}

/// `~/Library/Preferences/tokenme/settings.json` on macOS, `$XDG_CONFIG_HOME`
/// elsewhere — the file the menu-bar app owns.
pub fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("tokenme").join("settings.json"))
}

/// The `budgets` map from the settings file. A missing, unreadable or malformed
/// file means "no budgets", never an error: the report must still render.
pub fn load_budgets() -> BTreeMap<String, Budget> {
    let Some(path) = settings_path() else { return BTreeMap::new() };
    let Ok(raw) = std::fs::read_to_string(path) else { return BTreeMap::new() };
    parse_budgets(&raw)
}

pub fn parse_budgets(raw: &str) -> BTreeMap<String, Budget> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            serde_json::from_value(v.get("budgets")?.clone()).ok()
        })
        .unwrap_or_default()
}

/// Insert or clear one tool's budget, leaving every other key in the file alone.
pub fn store_budget(tool: &str, budget: Budget) -> std::io::Result<PathBuf> {
    let path = settings_path().ok_or_else(|| std::io::Error::other("no platform config dir"))?;
    store_budget_at(&path, tool, budget)?;
    Ok(path)
}

fn store_budget_at(path: &std::path::Path, tool: &str, budget: Budget) -> std::io::Result<()> {
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let budgets = root
        .as_object_mut()
        .expect("checked above")
        .entry("budgets")
        .or_insert_with(|| serde_json::json!({}));
    if !budgets.is_object() {
        *budgets = serde_json::json!({});
    }
    let map = budgets.as_object_mut().expect("just made it an object");
    if budget.is_empty() {
        map.remove(tool);
    } else {
        map.insert(
            tool.to_string(),
            serde_json::json!({ "daily_usd": budget.daily_usd, "monthly_usd": budget.monthly_usd }),
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
    crate::replace_file(&tmp, path)
}

/// One bar per open window, from what tokenme already charged.
///
/// `spent_today` / `spent_month` are per-tool cost sums for the same local
/// boundaries the report's day/month windows use, so the bar and the number the
/// user sees above it can never disagree.
pub fn views(
    budgets: &BTreeMap<String, Budget>,
    spent_today: &BTreeMap<String, f64>,
    spent_month: &BTreeMap<String, f64>,
    now_ms: i64,
    day_reset_ms: i64,
    month_reset_ms: i64,
) -> Vec<QuotaView> {
    let mut out = Vec::new();
    for (tool, budget) in budgets {
        if budget.is_empty() {
            continue;
        }
        for (limit, spent, window, resets, what) in [
            (budget.daily_usd, spent_today, 1_440i64, day_reset_ms, "日"),
            (budget.monthly_usd, spent_month, 43_200, month_reset_ms, "月"),
        ] {
            if limit <= 0.0 {
                continue;
            }
            let used = spent.get(tool).copied().unwrap_or(0.0);
            out.push(QuotaView {
                tool: tool.clone(),
                used_percent: used / limit * 100.0,
                window_minutes: window,
                resets_at_ms: resets,
                sampled_at_ms: now_ms,
                label: Some(format!("{what} · ${used:.2}/${limit:.0}")),
                id: Some(if window == 1_440 { "day".into() } else { "month".into() }),
                origin: QuotaOrigin::Budget,
            });
        }
    }
    out.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.window_minutes.cmp(&b.window_minutes)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_budgets_key_is_read_and_a_stranger_is_ignored() {
        let map = parse_budgets(r#"{"tray_mode":"cost","budgets":{"zcode":{"daily_usd":5},"cline":{"monthly_usd":20,"daily_usd":0}},"autostart":true}"#);
        assert_eq!(map.len(), 2);
        assert_eq!(map["zcode"].daily_usd, 5.0);
        assert_eq!(map["zcode"].monthly_usd, 0.0);
        assert_eq!(map["cline"].monthly_usd, 20.0);
        assert!(parse_budgets("not json").is_empty());
        assert!(parse_budgets(r#"{"tray_mode":"cost"}"#).is_empty());
        assert!(parse_budgets(r#"{"budgets":{"x":{}}}"#).get("x").is_some_and(|b| b.is_empty()));
    }

    #[test]
    fn a_cap_becomes_one_bar_per_window_with_the_money_in_the_label() {
        let mut budgets = BTreeMap::new();
        budgets.insert("zcode".to_string(), Budget { daily_usd: 5.0, monthly_usd: 20.0 });
        budgets.insert("cline".to_string(), Budget { daily_usd: 0.0, monthly_usd: 0.0 });
        let mut spent = BTreeMap::new();
        spent.insert("zcode".to_string(), 1.25);
        let views = views(&budgets, &spent, &BTreeMap::new(), 1_790_000_000_000, 1_790_086_400_000, 1_790_086_400_000);
        assert_eq!(views.len(), 2, "an unset budget contributes nothing: {views:?}");
        assert_eq!(views[0].window_minutes, 1_440);
        assert_eq!(views[0].used_percent, 25.0);
        assert_eq!(views[0].label.as_deref(), Some("日 · $1.25/$5"));
        assert_eq!(views[0].origin, QuotaOrigin::Budget);
        assert_eq!(views[1].used_percent, 0.0, "a fresh month is 0 %, not absent");
    }

    #[test]
    fn going_over_cap_shows_the_real_number_not_a_clamped_one() {
        let mut budgets = BTreeMap::new();
        budgets.insert("codex".to_string(), Budget { daily_usd: 10.0, monthly_usd: 0.0 });
        let mut spent = BTreeMap::new();
        spent.insert("codex".to_string(), 23.4);
        let views = views(&budgets, &spent, &BTreeMap::new(), 0, 1, 1);
        assert_eq!(views[0].used_percent, 234.0, "the bar clamps, the figure must not");
    }

    /// `store_budget` rewrites one key of the shared file; the merge it performs
    /// on the JSON tree is what must keep the app's own settings alive.
    #[test]
    fn writing_a_budget_leaves_the_app_settings_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"tray_mode":"tokens","autostart":true,"budgets":{}}"#).unwrap();

        store_budget_at(&path, "zcode", Budget { daily_usd: 5.0, monthly_usd: 0.0 }).unwrap();
        // Updating an existing settings file is the Windows-specific case:
        // plain std::fs::rename reports ERROR_ALREADY_EXISTS there.
        store_budget_at(&path, "zcode", Budget { daily_usd: 7.0, monthly_usd: 20.0 }).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse_budgets(&saved)["zcode"].daily_usd, 7.0);
        let back: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(back["budgets"]["zcode"]["monthly_usd"], 20.0);
        assert_eq!(back["tray_mode"], "tokens");
        assert_eq!(back["autostart"], true);
    }
}
