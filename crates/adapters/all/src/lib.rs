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
    "agnes",
    "atomcode",
    "workbuddy",
    "hermes",
    "funide",
    "catpaw",
    "dsh",
    // Siblings of the adapters above: same storage dialect, different product.
    "crow5",
    "mimocode",
    "cola",
    "joycode",
    "trae",
    "kimicode",
    "minimaxcode",
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
        Box::new(usage_adapter_agnes::AgnesAdapter),
        Box::new(usage_adapter_atomcode::AtomCodeAdapter),
        Box::new(usage_adapter_workbuddy::WorkBuddyAdapter),
        Box::new(usage_adapter_hermes::HermesAdapter),
        Box::new(usage_adapter_funide::FunIdeAdapter),
        Box::new(usage_adapter_catpaw::CatpawAdapter),
        Box::new(usage_adapter_dsh::DshAdapter),
        Box::new(usage_adapter_opencode::Crow5Adapter),
        Box::new(usage_adapter_opencode::MimocodeAdapter),
        Box::new(usage_adapter_pi::ColaAdapter),
        Box::new(usage_adapter_joycode::JoycodeAdapter),
        Box::new(usage_adapter_trae::TraeAdapter),
        Box::new(usage_adapter_kimicode::KimiCodeAdapter),
        Box::new(usage_adapter_minimaxcode::MiniMaxCodeAdapter),
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

    /// The READMEs are the product's inventory, so they are held to the registry:
    /// one row per adapter in each language's tool table, no tool listed twice,
    /// the two tables agreeing, and the count in prose being the real number.
    /// Docs drifted silently before this — a released note claimed a probe
    /// "never refreshes" credentials after that probe started renewing tokens,
    /// and a tool ended up in the table twice because a count keyed on the rows
    /// that carry an icon could not see the one row that does not.
    #[test]
    fn both_readmes_list_every_adapter_once_and_count_them_honestly() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let adapters = builtin_adapters();
        let mut previous: Option<usize> = None;
        for (name, header, count_form) in [
            ("README.md", "| Tool | Live quota |", "{} of them"),
            ("README_CN.md", "| 工具 | 实时配额 |", "共 {} 款"),
        ] {
            let text = std::fs::read_to_string(root.join(name))
                .unwrap_or_else(|e| panic!("{name} must be readable at the repo root: {e}"));
            let rows: Vec<&str> = text
                .lines()
                .skip_while(|line| !line.starts_with(header))
                // past the header row and its | :--- | rule, until the table ends
                .skip(2)
                .take_while(|line| line.starts_with("| "))
                .collect();
            assert_eq!(
                rows.len(),
                adapters.len(),
                "{name}: the tool table holds {} rows for {} adapters — one row each, \
                 and a quota-only source belongs in the note below the table, not in it",
                rows.len(),
                adapters.len()
            );
            if let Some(previous) = previous {
                assert_eq!(
                    previous, rows.len(),
                    "{name}'s tool table no longer matches the other README's"
                );
            }
            previous = Some(rows.len());
            for adapter in &adapters {
                let cell = format!("**{}**", adapter.display_name());
                let listed = rows.iter().filter(|row| row.contains(&cell)).count();
                assert_eq!(
                    listed, 1,
                    "{name} lists {} {listed} times; it must appear exactly once",
                    adapter.display_name()
                );
            }
            // Two rows could share a name that is no adapter's display name, so
            // compare the bold cells themselves, not just the known ones.
            let mut listed: Vec<&str> = rows.iter().filter_map(|row| row.split("**").nth(1)).collect();
            let rows_named = listed.len();
            listed.sort_unstable();
            listed.dedup();
            assert_eq!(
                listed.len(),
                rows_named,
                "{name}: {} rows but only {} distinct tool names — something is listed twice",
                rows_named,
                listed.len()
            );
            let stated = count_form.replace("{}", &adapters.len().to_string());
            assert!(
                text.contains(&stated),
                "{name} does not state the {stated} the registry actually has"
            );
        }
    }

    /// The hero banner carries the same two numbers the docs do — the version and
    /// the tool count. To a reader it is a picture, to git it is text nobody
    /// reviews, and it had drifted to `v0.1.4` / `20 AI Tools` while the READMEs
    /// said 0.1.5 and 22.
    #[test]
    fn the_hero_banner_carries_the_current_version_and_tool_count() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let version = format!("v{} · MIT License", env!("CARGO_PKG_VERSION"));
        let count = format!(">{} AI Tools<", builtin_adapters().len());
        for name in ["assets/tokenme-hero-dark.svg", "assets/tokenme-hero-light.svg"] {
            let svg = std::fs::read_to_string(root.join(name))
                .unwrap_or_else(|e| panic!("{name} must be readable: {e}"));
            assert!(
                svg.contains(&version),
                "{name} still shows a version older than {version}"
            );
            assert!(
                svg.contains(&count),
                "{name} still counts tools differently from {count}"
            );
        }
    }

    /// The version has four build-side homes, and `scripts/release.sh` writes only
    /// three of them — `apps/tokenme-bar/package.json` is moved by hand (AGENTS.md
    /// says so), so drift is the default state rather than the accident.
    /// `latest.json` is the fifth home and deliberately not in this gate: it is a
    /// *published* manifest and lags honestly, which the next test polices instead.
    /// This crate's `CARGO_PKG_VERSION` is the root `[workspace.package]` value
    /// baked in at compile time and is the yardstick: the CLI prints that number,
    /// and the panel compares *its own* baked version (`updater.rs:206`) against
    /// GitHub's latest tag to decide whether an update exists, so a home that drifts
    /// is a wrong answer a user sees. Agreement is also a check on the build itself
    /// — the panel links these crates as path dependencies through its own target
    /// dir and those rlibs have shipped stale before, so matching here can mean
    /// "this binary really was compiled from these files".
    #[test]
    fn the_version_lives_in_four_homes_and_they_all_agree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let here = env!("CARGO_PKG_VERSION");
        let read = |rel: &str| -> String {
            std::fs::read_to_string(root.join(rel))
                .unwrap_or_else(|e| panic!("{rel} must be readable at the repo root: {e}"))
        };
        let disagree = |file: &str, found: &str| {
            format!(
                "{file} says version {found} but this crate compiled as {here} — the four \
                 version homes move together (scripts/release.sh bumps three; \
                 apps/tokenme-bar/package.json is manual)"
            )
        };

        // The two Cargo.tomls are otherwise a list of dependency versions, so read
        // the key inside the table that owns the package version and nowhere else.
        let cargo_version = |rel: &str, table: &str| -> String {
            let text = read(rel);
            toml_table_value(&text, table, "version").unwrap_or_else(|| {
                panic!(
                    "{rel}: [{table}] carries no version — release.sh seds exactly that line, \
                     and the panel's Cargo.toml is its own workspace so it inherits nothing"
                )
            })
        };
        let workspace_version = cargo_version("Cargo.toml", "workspace.package");
        assert_eq!(
            here, workspace_version,
            "{}",
            disagree("Cargo.toml [workspace.package]", &workspace_version)
        );
        let panel_version = cargo_version("apps/tokenme-bar/src-tauri/Cargo.toml", "package");
        assert_eq!(
            here, panel_version,
            "{}",
            disagree("apps/tokenme-bar/src-tauri/Cargo.toml [package]", &panel_version)
        );

        // The bundler stamps the installer filename from tauri.conf.json and the
        // frontend's own version comes from package.json; each must hold exactly
        // one version field, wherever the config puts it (`"version"` at the top
        // level today, `package.version` is the other shape Tauri accepts).
        for rel in [
            "apps/tokenme-bar/src-tauri/tauri.conf.json",
            "apps/tokenme-bar/package.json",
        ] {
            let text = read(rel);
            let found = json_string_fields(&text, "version");
            assert_eq!(
                found.len(),
                1,
                "{rel} declares {found:?} — exactly one version field, or nothing says which \
                 one is the release the installer carries"
            );
            let value = found[0].1.clone();
            assert_eq!(here, value, "{}", disagree(rel, &value));
        }
    }

    /// `latest.json` is the published release manifest, not a build input:
    /// `scripts/release.sh` rewrites it at tag time, `release.yml:247` attaches it as
    /// a release asset, and the panel GETs it at boot from
    /// `raw.githubusercontent.com/Bencibr/tokenme/main/latest.json`
    /// (`src/lib/about.ts:12` → `src/lib/update.ts:30` → `App.tsx:108`), reads its
    /// `version` and `url`, and puts that `url` behind the update chip
    /// (`components/StatusBar.tsx:79`). Rust's in-app updater does not read it at
    /// all — `updater.rs:25` asks the GitHub API for `releases/latest` and resolves
    /// the asset set by name — so for the reader of this file the manifest's `url`
    /// is the only download they ever reach.
    ///
    /// What this polices is self-consistency, not currency. The manifest may lag:
    /// it announces 0.1.4 while the tree is 0.1.5, because 0.1.5 was never run
    /// through `release.sh`, and lag is honest — nobody is promised a binary that
    /// does not exist. What it may never do is advertise bytes it does not have. So
    /// a version pinned in a download tag, or in the installer filename the url
    /// ends with, must be the version it declares; and a manifest that pins one
    /// concrete installer must state its sha256 in the shape the release already
    /// publishes (`release.yml:238`, 64 lowercase hex), because the panel's own
    /// updater will not stage a bundle before comparing a hash (`updater.rs:298`)
    /// and a bare file link leaves the reader nothing to compare. Bumping only the
    /// `version` string by hand is precisely how a stale manifest turns into a
    /// lying one: `url` would still resolve to the previous release's artifacts.
    #[test]
    fn the_release_manifest_never_lies_about_its_own_assets() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let text = std::fs::read_to_string(root.join("latest.json"))
            .expect("latest.json must be readable at the repo root — release.sh writes it there");

        let versions = json_string_fields(&text, "version");
        assert_eq!(
            versions.len(),
            1,
            "latest.json declares {versions:?} — one version, or the panel cannot tell which \
             release it is announcing"
        );
        let (version_line, version) = versions[0].clone();
        assert!(
            is_plain_release(&version),
            "latest.json:{version_line} announces version {version}, which is not x.y.z — the \
             updater's semver comparison reads three numbers and anything else never updates"
        );

        let urls = json_string_fields(&text, "url");
        assert_eq!(
            urls.len(),
            1,
            "latest.json declares {urls:?} — one download pointer for the whole release"
        );
        let (url_line, url) = urls[0].clone();
        assert!(
            url.starts_with("https://"),
            "latest.json:{url_line} points users at {url}, which is not https"
        );

        // The concrete asset this manifest promises: a `/releases/download/<tag>/…`
        // URL names the bytes it will hand over, and so does one whose last path
        // segment is an installer (`TokenMe_0.1.5_x64-setup.exe`, or a
        // `tokenme_0.1.5_aarch64.dmg`). A `releases/latest` pointer names nothing,
        // and that is exactly what keeps a lagging manifest truthful.
        let hashes = json_string_fields(&text, "sha256");
        // Any hash the manifest does publish has to be one.
        for (line, value) in &hashes {
            assert!(
                is_sha256_hex(value),
                "latest.json:{line} sha256 is {value:?} — 64 lowercase hex, the shape of the \
                 release's own sha256.txt"
            );
        }
        let asset = url.rsplit('/').next().unwrap_or_default().to_string();
        if let Some(released) = url.split("/releases/download/").nth(1) {
            // `…/releases/download/<tag>/<file>` pins the exact bytes the reader gets.
            let tag = released.split('/').next().unwrap_or_default();
            assert_eq!(
                version,
                tag.trim_start_matches('v'),
                "latest.json:{url_line} announces {version} but its url downloads the release \
                 tagged {tag} — a reader is handed the previous release's installer"
            );
        }
        if [".exe", ".dmg", ".zip", ".tar.gz"]
            .iter()
            .any(|suffix| asset.ends_with(suffix))
        {
            assert!(
                asset.contains(&version),
                "latest.json:{url_line} announces {version} but advertises {asset}, whose \
                 filename is stamped with a different one — name and version are the same release"
            );
            assert_eq!(
                hashes.len(),
                1,
                "latest.json:{url_line} promises {asset} with no single sha256 field beside it \
                 ({hashes:?}) — the updater verifies a hash before it stages anything"
            );
        }
    }

    /// The value of `key = "value"` inside one TOML table (`[workspace.package]`,
    /// `[package]`) and nowhere else — these files are otherwise a list of
    /// dependency versions, and a whole-file scan would read one of those as the
    /// package version.
    fn toml_table_value(text: &str, table: &str, key: &str) -> Option<String> {
        let header = format!("[{table}]");
        let mut inside = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                inside = line == header;
                continue;
            }
            if !inside || line.starts_with('#') {
                continue;
            }
            if let Some((name, value)) = line.split_once('=') {
                if name.trim() == key {
                    return Some(value.trim().trim_matches('"').to_string());
                }
            }
        }
        None
    }

    /// Every `"key": "<string>"` in a JSON file with its line number, by string
    /// matching: this crate depends on `serde` but has no `serde_json`, and a
    /// release gate does not need a parser — it needs the count and the values,
    /// and a colon on the same line as the key is enough to keep an array of
    /// strings or a comment from inventing either.
    fn json_string_fields(text: &str, key: &str) -> Vec<(usize, String)> {
        let needle = format!("\"{key}\"");
        let mut found = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let mut rest = line;
            while let Some(at) = rest.find(&needle) {
                rest = &rest[at + needle.len()..];
                let Some(after_key) = rest.trim_start().strip_prefix(':') else { continue };
                let Some(quoted) = after_key.trim_start().strip_prefix('"') else { continue };
                if let Some(end) = quoted.find('"') {
                    found.push((number + 1, quoted[..end].to_string()));
                }
            }
        }
        found
    }

    /// Three ASCII number groups and nothing else: the shape both update checkers
    /// parse (`src/lib/update.ts:9` on the frontend, `updater.rs:192` in Rust), so
    /// a manifest announcing `v0.1.5-dev` or `0.1` reads as no version at all and
    /// the prompt silently never fires.
    fn is_plain_release(version: &str) -> bool {
        version.split('.').count() == 3
            && version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    }

    /// 64 lowercase hex — what `release.yml:238` writes into `sha256.txt` and what
    /// `updater.rs:298` compares a download against before it stages the bundle.
    fn is_sha256_hex(value: &str) -> bool {
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }
}
