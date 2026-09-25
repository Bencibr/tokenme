//! macOS application icons for the panel's tool rows.
//!
//! Only pixels, never numbers: nothing that is counted comes from here, so an
//! unreadable icon degrades to a coloured monogram and no figure is ever at risk.
//! The row identity stays the tool's own name — this is decoration on top of a
//! label, not a replacement for it.
//!
//! A bundle's `Contents/Resources/*.icns` is parsed just enough to lift out an
//! embedded PNG. Apple's own icon containers store those entries as ordinary PNG
//! files, so no image codec is needed: find the entry whose `IHDR` width is
//! closest to the 24 px slot and hand the bytes over untouched. Tools with no
//! bundle to read keep a PNG shipped next to this file instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `(tool id, bundle names to try, in order)`.
///
/// Codex is the coding face of a ChatGPT account, so it deliberately borrows
/// the ChatGPT app's badge; Cline ships a desktop app. A tool that names no
/// bundle and no shipped icon (`BUNDLED_ICONS` below) falls to the panel's
/// coloured monogram.
pub const APP_BUNDLES: &[(&str, &[&str])] = &[
    ("claude", &["Claude"]),
    ("codex", &["ChatGPT"]),
    ("opencode", &[]),
    ("pi", &[]),
    ("cline", &["Cline"]),
    ("zcode", &["ZCode"]),
    ("qoder", &["Qoder"]),
    ("antigravity", &["Antigravity"]),
    ("ccswitch", &["CC Switch"]),
    ("agnes", &["AgnesCode"]),
    ("atomcode", &[]),
    ("workbuddy", &["WorkBuddy AI", "WorkBuddy"]),
    ("crow5", &["Crow5"]),
    ("mimocode", &[]),
    ("cola", &["Cola"]),
];

/// PNGs shipped with the panel, consulted only when the bundle lookup above
/// produced no pixels — which is always the case on Windows and Linux, where
/// there are no `.app` bundles to read. Extracted from each vendor's own
/// installed bundle (smallest tile ≥ 48 px, same rule as the live lookup), so
/// a Windows panel shows the same marks a Mac one does. Still pure decoration:
/// a corrupt file costs the row its icon, never a figure.
pub const BUNDLED_ICONS: &[(&str, &[u8])] = &[
    ("claude", include_bytes!("../assets/claude.png")),
    ("codex", include_bytes!("../assets/codex.png")),
    ("zcode", include_bytes!("../assets/zcode.png")),
    ("qoder", include_bytes!("../assets/qoder.png")),
    ("antigravity", include_bytes!("../assets/antigravity.png")),
    ("ccswitch", include_bytes!("../assets/ccswitch.png")),
    ("agnes", include_bytes!("../assets/agnes.png")),
    ("crow5", include_bytes!("../assets/crow5.png")),
    ("cola", include_bytes!("../assets/cola.png")),
    ("workbuddy", include_bytes!("../assets/workbuddy.png")),
    ("cline", include_bytes!("../assets/cline.png")),
    // OpenCode is a CLI, but the vendor ships an official mark (their own
    // apple-touch icon) — shipped, not scraped from an unrelated bundle.
    ("opencode", include_bytes!("../assets/opencode.png")),
];

/// Where a user-installed app lives. `~/Applications` first: a per-user install
/// of the same name is the one this person chose.
pub fn bundle_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(Path::new(&home).join("Applications"));
    }
    out.push(PathBuf::from("/Applications"));
    if let Some(s) = std::env::var_os("TOKENME_APP_DIRS") {
        // A test or a review run can point the lookup at a fixture tree instead.
        out.extend(
            s.to_string_lossy()
                .split(':')
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
        );
    }
    out
}

/// `tool id → data:image/png;base64,…`, only for tools whose icon could be read.
pub fn icon_data_urls() -> BTreeMap<String, String> {
    let mut out = icons_from(&bundle_dirs(), APP_BUNDLES);
    insert_bundled(&mut out, BUNDLED_ICONS);
    out
}

/// Fills the shipped icons in, but never over pixels a real bundle produced.
fn insert_bundled(out: &mut BTreeMap<String, String>, bundled: &[(&str, &[u8])]) {
    for (tool, png) in bundled {
        out.entry((*tool).to_string())
            .or_insert_with(|| format!("data:image/png;base64,{}", base64(png)));
    }
}

fn icons_from(dirs: &[PathBuf], wanted: &[(&str, &[&str])]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (tool, names) in wanted {
        for name in *names {
            let Some(png) = dirs
                .iter()
                .map(|d| d.join(format!("{name}.app")))
                .find(|p| p.is_dir())
                .and_then(|bundle| icns_bytes(&bundle).and_then(|b| png_from_icns(&b, 24)))
            else {
                continue;
            };
            out.insert((*tool).to_string(), format!("data:image/png;base64,{}", base64(&png)));
            break;
        }
    }
    out
}

/// The bundle's icon container. Preference order, then the alphabetically first
/// `.icns`, so the answer never depends on directory iteration order.
fn icns_bytes(bundle: &Path) -> Option<Vec<u8>> {
    let res = bundle.join("Contents/Resources");
    for name in ["AppIcon.icns", "icon.icns", "electron.icns"] {
        let p = res.join(name);
        if p.is_file() {
            return std::fs::read(p).ok();
        }
    }
    // Nothing by the usual names: take the alphabetically first `.icns`, so the
    // answer never depends on the order the directory happens to come back in.
    let mut others: Vec<PathBuf> = std::fs::read_dir(&res).ok()?.flatten().map(|e| e.path()).collect();
    others.sort();
    let path = others.into_iter().find(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("icns"))?;
    std::fs::read(path).ok()
}

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The embedded PNG whose width best fits `slot_px` at 2x, i.e. ~48 px.
///
/// An `.icns` is a `icns` magic, a big-endian total length, then entries of
/// four-byte type + big-endian length (header included) + payload. The modern
/// `icNN` types carry whole PNG files, so the payload is returned untouched.
fn png_from_icns(bytes: &[u8], slot_px: u32) -> Option<Vec<u8>> {
    if bytes.len() < 8 || &bytes[..4] != b"icns" {
        return None;
    }
    let want = slot_px * 2;
    let mut best: Option<(u32, Vec<u8>)> = None;
    let mut off = 8usize;
    while off + 8 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[off + 4], bytes[off + 5], bytes[off + 6], bytes[off + 7]]) as usize;
        if len < 8 || off + len > bytes.len() {
            break;
        }
        let payload = &bytes[off + 8..off + len];
        if let Some(width) = png_width(payload) {
            // Never upscale: a 32 px tile in a 48 px slot is soft, while halving a
            // 256 px one still reads. So the smallest tile that covers the slot
            // wins, and only if none does, the largest available.
            let cost = if width >= want { width - want } else { (u32::MAX / 2).saturating_sub(width) };
            if best.as_ref().is_none_or(|(c, _)| cost < *c) {
                best = Some((cost, payload.to_vec()));
            }
        }
        off += len;
    }
    best.map(|(_, v)| v)
}

/// `IHDR` width, or `None` when the payload is not a PNG (JPEG-2000-era entries).
fn png_width(payload: &[u8]) -> Option<u32> {
    if payload.len() < 24 || &payload[..8] != PNG_MAGIC || &payload[12..16] != b"IHDR" {
        return None;
    }
    Some(u32::from_be_bytes([payload[16], payload[17], payload[18], payload[19]]))
}

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard alphabet, `=` padded — what a `data:` URL needs.
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but structurally real PNG payload: signature, `IHDR` with the
    /// given width, and nothing after it — the reader only ever looks that far.
    fn png(width: u32) -> Vec<u8> {
        let mut v = PNG_MAGIC.to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&[0u8; 9]);
        v
    }

    fn icns(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (kind, payload) in entries {
            body.extend_from_slice(kind.as_bytes());
            body.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
            body.extend_from_slice(payload);
        }
        let mut out = b"icns".to_vec();
        out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn the_entry_nearest_the_display_size_2x_wins() {
        let file = icns(&[("ic04", png(16)), ("ic13", png(256)), ("ic12", png(64)), ("ic10", png(1024))]);
        let picked = png_from_icns(&file, 24).expect("a png entry is there");
        assert_eq!(png_width(&picked), Some(64), "64 px is 2x of a 24 px slot");
    }

    #[test]
    fn an_oversized_tile_beats_a_small_one_rather_than_upscaling() {
        let file = icns(&[("ic11", png(32)), ("ic08", png(256))]);
        let picked = png_from_icns(&file, 24).unwrap();
        assert_eq!(png_width(&picked), Some(256), "downscale rather than stretch");
    }

    /// A corrupt or hostile container: a declared width that would overflow the
    /// cost arithmetic must not panic the panel.
    #[test]
    fn an_absurd_width_is_skipped_rather_than_panicking() {
        let file = icns(&[("ic07", png(u32::MAX)), ("ic12", png(64))]);
        let picked = png_from_icns(&file, 24).unwrap();
        assert_eq!(png_width(&picked), Some(64), "the sane tile is still found");
        assert!(png_from_icns(&icns(&[("ic07", png(u32::MAX))]), 24).is_some(), "even alone it is only a bad pixel size, not a crash");
    }

    #[test]
    fn a_container_without_a_png_or_a_sane_header_is_no_icon() {
        // JPEG-2000-only legacy container.
        let file = icns(&[("jp2 ", vec![0, 0, 0, 12, b'j', b'P', b' ', b' '])]);
        assert!(png_from_icns(&file, 24).is_none());
        assert!(png_from_icns(b"not an icns file at all", 24).is_none());
        // A length that runs past the end of the buffer must not walk off it.
        let mut truncated = icns(&[("ic07", png(128))]);
        truncated.truncate(truncated.len() - 20);
        assert!(png_from_icns(&truncated, 24).is_none_or(|v| png_width(&v).is_some()));
    }

    #[test]
    fn base64_matches_the_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// A fixture bundle tree, so the whole lookup is testable without depending on
    /// what happens to be installed.
    fn fixture_bundle(root: &Path, name: &str, icon: &str) {
        let res = root.join(format!("{name}.app")).join("Contents/Resources");
        std::fs::create_dir_all(&res).unwrap();
        std::fs::write(res.join(icon), icns(&[("ic12", png(64)), ("ic07", png(128))])).unwrap();
    }

    #[test]
    fn a_tool_only_resolves_to_the_bundle_named_for_it() {
        let dir = std::env::temp_dir().join(format!("tokenme-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture_bundle(&dir, "Qoder", "icon.icns");
        fixture_bundle(&dir, "NotOurTool", "AppIcon.icns");

        let found = icons_from(
            std::slice::from_ref(&dir),
            &[("qoder", &["Qoder"]), ("codex", &[]), ("else", &["Missing"])],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        let url = &found["qoder"];
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
        assert!(!found.contains_key("else"), "a missing bundle yields no entry");
        assert!(!found.contains_key("codex"), "nothing shipped or named for it");
        // `AppIcon` is preferred over `icon`, and both are only ever read from the
        // bundle the tool itself named.
        assert!(dir.join("NotOurTool.app").exists(), "the fixture exists but is unlisted");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shipped_icon_fills_in_but_never_overrides_a_bundle() {
        let mut found = BTreeMap::from([("cline".to_string(), "data:image/png;base64,Qg==".to_string())]);
        insert_bundled(&mut found, &[("cline", b"whatever"), ("codex", b"neither")]);
        assert_eq!(found["cline"], "data:image/png;base64,Qg==", "a resolved bundle keeps its pixels");
        assert!(
            found["codex"].starts_with("data:image/png;base64,"),
            "a tool with no bundle takes the shipped one"
        );
    }

    #[test]
    fn the_shipped_cline_icon_is_a_real_png() {
        let (_, png) = BUNDLED_ICONS.iter().find(|(tool, _)| *tool == "cline").expect("cline ships an icon");
        assert_eq!(png_width(png), Some(128), "{:?}", &png[..16]);
    }

    /// Live proof on the machine that has the apps; run with
    /// `cargo test -p usage-core icons -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the real /Applications of this machine"]
    fn live_machine_icons_resolve() {
        let map = icon_data_urls();
        for (tool, url) in &map {
            let b64 = url.split_once(',').map(|(_, rest)| rest).unwrap_or_default();
            println!("{tool}: ~{} bytes", b64.len() * 3 / 4);
        }
        assert!(map.contains_key("codex"), "ChatGPT.app is installed here");
        assert!(map.contains_key("cline"), "cline ships a bundled icon");
        assert!(map.contains_key("opencode"), "opencode ships a bundled icon");
    }
}
