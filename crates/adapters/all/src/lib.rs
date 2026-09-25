//! Static registry of built-in adapters.
//!
//! Deliberately compile-time wiring (mirrors `ccusage-adapter-all`): a dynamic
//! plugin host would cost code signing, ABI stability and safety guarantees for
//! no real user benefit, since a new source needs parsing logic anyway.
//!
//! Owned by the index/CLI workstream: keep `TOOL_IDS`, `builtin_adapters`,
//! `adapters_for` and `detect_all` exactly as declared — `usage-cli` and the
//! menu-bar app call them by these signatures.

use usage_core::{DetectedSource, SourceAdapter};

pub const TOOL_IDS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "pi",
    "cline",
    "zcode",
    "qoder",
    "antigravity",
    "ccswitch",
    "agnes",
    "atomcode",
    "workbuddy",
    "catpaw",
    // Siblings of the adapters above: same storage dialect, different product.
    "crow5",
    "mimocode",
    "cola",
];

pub fn builtin_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(usage_adapter_claude::ClaudeAdapter),
        Box::new(usage_adapter_codex::CodexAdapter),
        Box::new(usage_adapter_opencode::OpenCodeAdapter),
        Box::new(usage_adapter_pi::PiAdapter),
        Box::new(usage_adapter_cline::ClineAdapter),
        Box::new(usage_adapter_zcode::ZcodeAdapter),
        Box::new(usage_adapter_qoder::QoderAdapter),
        Box::new(usage_adapter_antigravity::AntigravityAdapter),
        Box::new(usage_adapter_ccswitch::CcSwitchAdapter),
        Box::new(usage_adapter_agnes::AgnesAdapter),
        Box::new(usage_adapter_atomcode::AtomCodeAdapter),
        Box::new(usage_adapter_workbuddy::WorkBuddyAdapter),
        Box::new(usage_adapter_catpaw::CatpawAdapter),
        Box::new(usage_adapter_opencode::Crow5Adapter),
        Box::new(usage_adapter_opencode::MimocodeAdapter),
        Box::new(usage_adapter_pi::ColaAdapter),
    ]
}

/// `ids` are `--tool` filters; an empty slice means every built-in adapter.
pub fn adapters_for(ids: &[&str]) -> Vec<Box<dyn SourceAdapter>> {
    if ids.is_empty() {
        return builtin_adapters();
    }
    builtin_adapters()
        .into_iter()
        .filter(|a| ids.iter().any(|id| *id == a.id()))
        .collect()
}

pub fn detect_all() -> Vec<DetectedSource> {
    builtin_adapters().iter().filter_map(|a| a.probe()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--tool` accepts an id only if an adapter answers to it, and the bar app's
    /// `TOOL_ORDER` list is kept in step with this one by the same rule.
    #[test]
    fn every_registered_adapter_is_listed_and_only_once() {
        let adapters = builtin_adapters();
        assert_eq!(adapters.len(), TOOL_IDS.len(), "one entry per adapter");
        for id in TOOL_IDS {
            assert_eq!(
                adapters.iter().filter(|a| a.id() == *id).count(),
                1,
                "{id} must be registered exactly once"
            );
        }
        for adapter in &adapters {
            assert!(
                TOOL_IDS.contains(&adapter.id()),
                "{} is registered but not listed in TOOL_IDS",
                adapter.id()
            );
            assert!(!adapter.display_name().is_empty());
        }
    }

    /// A sibling must not be reachable only through the product it copies.
    #[test]
    fn filtering_by_tool_id_selects_exactly_that_product() {
        let ids = ["crow5", "mimocode", "cola"];
        let picked: Vec<&str> = adapters_for(&ids).iter().map(|a| a.id()).collect();
        assert_eq!(picked, ids, "one adapter per id, in the order asked for");
        assert_eq!(adapters_for(&[]).len(), TOOL_IDS.len());
    }
}
