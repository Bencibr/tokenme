//! Where Codex keeps its rollout logs, and how the date-partitioned tree is
//! pruned before anything is opened.
//!
//! Both layouts occur in the wild: `sessions/2026-09-23/rollout-*.jsonl` and
//! `sessions/2026/09/23/rollout-*.jsonl`. A chain of date-like directory names is
//! accumulated and collapsed into the coarsest span it can prove, so a subtree
//! that is provably outside [`DateFilter`] is skipped whole. That is what keeps
//! the first index pass bounded to the retention window instead of stat-ing
//! years of a 3.8 GB tree.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use usage_core::{DateFilter, FileKind, SourceFile};

/// Guards against an unexpected deep or symlinked tree inside `~/.codex`.
const MAX_DEPTH: usize = 6;

pub(crate) fn codex_home() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("CODEX_HOME") {
        if !v.trim().is_empty() {
            return Some(PathBuf::from(v.trim()));
        }
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

pub(crate) fn sessions_root(home: &Path) -> PathBuf {
    home.join("sessions")
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `rollout-2026-09-23T19-25-11-01a0ce03-33ce-7172-848c-7199f1589e2e` and the
/// `_`-separated spelling both end in the 36-char session uuid, which is the
/// fallback session id when a file has no `session_meta`.
pub(crate) fn uuid_from_file_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let tail = match stem.rsplit_once('_') {
        Some((_, t)) if t.len() == 36 => t,
        _ if stem.len() >= 36 => &stem[stem.len() - 36..],
        _ => return None,
    };
    let is_uuid = tail.len() == 36
        && tail.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    is_uuid.then(|| tail.to_ascii_lowercase())
}

fn local_midnight(y: i32, m: u32, d: u32) -> Option<i64> {
    let naive = NaiveDate::from_ymd_opt(y, m, d)?.and_hms_opt(0, 0, 0)?;
    // A DST-skipped midnight returns `None`: no span, hence no pruning, which is
    // the safe direction (never drop real data over a boundary ambiguity).
    Local.from_local_datetime(&naive).single().map(|dt| dt.timestamp_millis())
}

/// A directory name that is a whole date, one numeric date component, or noise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DatePart {
    Day(NaiveDate),
    Num(u32),
    NotDate,
}

fn date_part(name: &str) -> DatePart {
    let n = name.trim();
    if let Ok(d) = NaiveDate::parse_from_str(n, "%Y-%m-%d") {
        return DatePart::Day(d);
    }
    if !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(v) = n.parse::<u32>() {
            return DatePart::Num(v);
        }
    }
    DatePart::NotDate
}

fn day_span(y: i32, m: u32, d: u32) -> Option<(i64, i64)> {
    let start = local_midnight(y, m, d)?;
    let end = match (m, d) {
        (12, 31) => local_midnight(y + 1, 1, 1)?,
        (_, 31) => local_midnight(y, m + 1, 1)?,
        (_, d) => local_midnight(y, m, d + 1)?,
    };
    Some((start, end))
}

/// Inclusive-start / exclusive-end local span proved by a chain of directory
/// names, or `None` when the chain proves no date (then nothing gets pruned).
fn span_ms(parts: &[DatePart]) -> Option<(i64, i64)> {
    if let Some(DatePart::Day(d)) = parts.first() {
        return day_span(d.year(), d.month(), d.day());
    }
    let nums: Vec<u32> = parts
        .iter()
        .map(|p| match p {
            DatePart::Num(v) => Some(*v),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let year = |y: u32| (1990..=2200).contains(&y);
    match nums.as_slice() {
        [y] if year(*y) => Some((local_midnight(*y as i32, 1, 1)?, local_midnight(*y as i32 + 1, 1, 1)?)),
        [y, mo] if year(*y) && (1..=12).contains(mo) => Some((
            local_midnight(*y as i32, *mo, 1)?,
            if *mo == 12 { local_midnight(*y as i32 + 1, 1, 1)? } else { local_midnight(*y as i32, mo + 1, 1)? },
        )),
        [y, mo, d] if year(*y) && (1..=12).contains(mo) && (1..=31).contains(d) => {
            day_span(*y as i32, *mo, *d)
        }
        _ => None,
    }
}

fn overlaps(filter: &DateFilter, start_ms: i64, end_ms: i64) -> bool {
    filter.since_ms.is_none_or(|s| end_ms > s) && filter.until_ms.is_none_or(|u| start_ms <= u)
}

/// `false` only when the whole subtree is provably outside `filter`.
fn dir_in_filter(filter: &DateFilter, parts: &[DatePart]) -> bool {
    match span_ms(parts) {
        Some((s, e)) => overlaps(filter, s, e),
        None => true,
    }
}

fn is_rollout(name: &str) -> bool {
    name.ends_with(".jsonl")
}

fn walk(dir: &Path, parts: &[DatePart], filter: &DateFilter, depth: usize, out: &mut Vec<SourceFile>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            let part = date_part(name);
            // Noise in the chain breaks the date path: restart the accumulation
            // so `sessions/backup/2026-09-23` is still understood as a day.
            let mut child: Vec<DatePart> = if part == DatePart::NotDate {
                Vec::new()
            } else if matches!(part, DatePart::Day(_)) {
                vec![part]
            } else {
                let mut v = parts.to_vec();
                v.push(part);
                v
            };
            if child.len() > 3 {
                child.truncate(3);
            }
            if !dir_in_filter(filter, &child) {
                continue;
            }
            walk(&path, &child, filter, depth + 1, out);
            continue;
        }
        if !is_rollout(name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        out.push(SourceFile {
            kind: FileKind::Jsonl,
            size: meta.len(),
            mtime_ms: mtime_ms(&meta),
            path,
        });
    }
}

pub(crate) fn list_rollouts(root: &Path, filter: &DateFilter) -> Vec<SourceFile> {
    let mut out = Vec::new();
    walk(root, &[], filter, 0, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Sub-directories in ascending name order, i.e. oldest date first.
fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<(String, PathBuf)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let name = p.file_name()?.to_str()?.to_string();
                e.file_type().ok().filter(|ft| ft.is_dir()).map(|_| (name, p))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    dirs.into_iter().map(|(_, p)| p).collect()
}

/// Newest rollout file that can actually be opened. Visits a bounded number of
/// directory entries: `probe` runs at every startup and must not walk 3.8 GB.
pub(crate) fn first_rollout(root: &Path) -> Option<PathBuf> {
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if visited > 400 || depth > MAX_DEPTH {
            continue;
        }
        visited += 1;
        // Pushed oldest-first so the LIFO pop reaches the newest dates first.
        // `read_dir`'s `file_type` does not follow symlinks, so no cycle is possible.
        for child in child_dirs(&dir) {
            stack.push((child, depth + 1));
        }
        let mut files: Vec<String> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let n = p.file_name()?.to_str()?.to_string();
                    (is_rollout(&n) && e.file_type().map(|ft| ft.is_file()).unwrap_or(false)).then_some(n)
                })
                .collect(),
            Err(_) => continue,
        };
        files.sort();
        if let Some(name) = files.into_iter().rev().find(|n| {
            let p = dir.join(n);
            std::fs::File::open(&p).is_ok()
        }) {
            return Some(dir.join(name));
        }
    }
    None
}

/// `cli_version` off the first `session_meta` line, used as the sources-list hint.
pub(crate) fn cli_version_hint(path: &Path) -> Option<String> {
    use std::io::{BufRead, Read};
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file.take(1024 * 1024));
    for line in reader.lines().map_while(Result::ok).take(8) {
        if !line.contains("session_meta") {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if let Some(ver) = v.get("payload").and_then(|p| p.get("cli_version")).and_then(|c| c.as_str()) {
            return Some(ver.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(names: &[&str]) -> Vec<DatePart> {
        names.iter().map(|n| date_part(n)).collect()
    }

    #[test]
    fn flat_and_nested_date_dirs_both_prove_a_day() {
        let flat = span_ms(&parts(&["2026-09-23"])).expect("flat day");
        let nested = span_ms(&parts(&["2026", "09", "23"])).expect("nested day");
        assert_eq!(flat, nested, "both spellings mean the same local day");
        assert_eq!(nested.1 - nested.0, 86_400_000);
    }

    #[test]
    fn year_and_month_chains_prune_whole_subtrees() {
        let until = DateFilter::new(None, Some(local_midnight(2026, 8, 31).unwrap() + 1_000));
        let since = DateFilter::new(Some(local_midnight(2026, 9, 20).unwrap()), None);
        assert!(!dir_in_filter(&until, &parts(&["2026", "09"])), "September is out of range");
        assert!(dir_in_filter(&until, &parts(&["2026", "08"])));
        assert!(dir_in_filter(&until, &parts(&["2026"])), "2026 still contains August");
        assert!(dir_in_filter(&until, &parts(&["2025"])), "an older year is inside `until`");
        assert!(!dir_in_filter(&since, &parts(&["2025"])), "and outside `since`");
        assert!(!dir_in_filter(&since, &parts(&["2024"])));
    }

    #[test]
    fn non_date_dirs_are_never_pruned() {
        let f = DateFilter::new(Some(local_midnight(2030, 1, 1).unwrap()), None);
        assert!(dir_in_filter(&f, &parts(&["archived"])));
        assert!(dir_in_filter(&f, &[]));
        assert!(span_ms(&parts(&["2026", "13"])).is_none(), "month 13 proves nothing");
    }

    #[test]
    fn uuid_survives_both_file_name_spellings() {
        let u = "01a0ce03-33ce-7172-848c-7199f1589e2e";
        assert_eq!(uuid_from_file_name(Path::new(&format!("rollout-2026-09-23T19-25-11-{u}"))).as_deref(), Some(u));
        assert_eq!(uuid_from_file_name(Path::new(&format!("rollout-2026-09-23T19-25-11_{u}"))).as_deref(), Some(u));
        assert_eq!(uuid_from_file_name(Path::new("notes")), None);
        assert_eq!(uuid_from_file_name(Path::new("rollout-not-a-uuid")), None);
    }
}
