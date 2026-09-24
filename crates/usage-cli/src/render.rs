//! Terminal rendering: unicode tables, human units, tty-only colour.

use std::io::IsTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

pub fn colour_wanted() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("TERM").ok().as_deref() == Some("dumb") {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Wraps in an SGR sequence only when colour is on, so widths stay honest.
pub fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str, color: bool) -> String {
    paint(s, "1", color)
}

pub fn dim(s: &str, color: bool) -> String {
    paint(s, "2", color)
}

/// Green when spending fell, red when it rose: the number a user acts on.
pub fn signed_pct(p: f64, color: bool) -> String {
    let body = format!("{}{:.1}%", if p >= 0.0 { "+" } else { "" }, p);
    if !color {
        return body;
    }
    let code = if p > 0.0 { "31" } else if p < 0.0 { "32" } else { "2" };
    format!("\x1b[{code}m{body}\x1b[0m")
}

/// East-Asian wide characters and box drawing take two cells; without this the
/// columns drift for CJK project names.
pub fn width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let cp = c as u32;
    // Combining marks render zero-width.
    if (0x300..=0x36f).contains(&cp) {
        return 0;
    }
    let wide = matches!(cp,
        0x1100..=0x115f | 0x2e80..=0x303e | 0x3041..=0x33ff | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff | 0xa000..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff
        | 0xfe30..=0xfe6f | 0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x1f300..=0x1f64f
        | 0x20000..=0x3fffd);
    if wide {
        2
    } else {
        1
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > max - 1 {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

pub fn pad(s: &str, target: usize, align: Align) -> String {
    let gap = target.saturating_sub(width(s));
    match align {
        Align::Right => format!("{}{}", " ".repeat(gap), s),
        Align::Left => format!("{}{}", s, " ".repeat(gap)),
    }
}

/// Thousands-separated integer, for request and session counts.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `12.4M`, `845k`, `42` — the units a glanceable cost line uses.
pub fn tokens(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1e9 {
        format!("{:.2}B", n / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}k", n / 1e3)
    } else {
        format!("{}", n.round() as i64)
    }
}

pub fn money(value: f64) -> String {
    format!("${value:.2}")
}

/// Money honesty: an unknown price is `no price`, never `$0.00`.
pub fn cost_cell(value: Option<f64>) -> String {
    match value {
        Some(v) => money(v),
        None => "no price".into(),
    }
}

pub fn pct(p: f64) -> String {
    format!("{p:.1}%")
}

pub fn bar(used_percent: f64, cells: usize) -> String {
    let filled = (used_percent / 100.0 * cells as f64).round().clamp(0.0, cells as f64) as usize;
    format!("{}{}", "▰".repeat(filled), "▱".repeat(cells.saturating_sub(filled)))
}

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn minutes_window(m: i64) -> String {
    if m <= 0 {
        return "—".into();
    }
    if m % 1440 == 0 {
        format!("{}d", m / 1440)
    } else if m % 60 == 0 {
        format!("{}h", m / 60)
    } else {
        format!("{m}m")
    }
}

fn human_span(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0 => "0s".into(),
        1..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h{}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d{}h", s / 86400, (s % 86400) / 3600),
    }
}

pub fn ago(ms_diff: i64) -> String {
    if ms_diff <= 1000 {
        "just now".into()
    } else {
        format!("{} ago", human_span(ms_diff))
    }
}

pub fn until(in_ms: i64) -> String {
    if in_ms <= 0 {
        "now".into()
    } else {
        format!("in {}", human_span(in_ms))
    }
}

/// Home-directory shorthand keeps the roots column readable.
pub fn shorten_path(p: &str) -> String {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !home.is_empty() && p.starts_with(&home) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
    footers: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            aligns: vec![Align::Left; headers.len()],
            rows: Vec::new(),
            footers: Vec::new(),
        }
    }

    pub fn right(mut self, cols: &[usize]) -> Self {
        for c in cols {
            if let Some(slot) = self.aligns.get_mut(*c) {
                *slot = Align::Right;
            }
        }
        self
    }

    pub fn push(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Builder-style row for one-shot tables.
    pub fn row(mut self, cells: Vec<String>) -> Self {
        self.rows.push(cells);
        self
    }

    pub fn footer(&mut self, cells: Vec<String>) {
        self.footers.push(cells);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn render(&self, color: bool) -> String {
        let cols = self.headers.len();
        let mut w = vec![0usize; cols];
        let bump = |w: &mut Vec<usize>, row: &Vec<String>| {
            for (i, cell) in row.iter().enumerate().take(cols) {
                w[i] = w[i].max(width(cell));
            }
        };
        bump(&mut w, &self.headers);
        for row in self.rows.iter().chain(self.footers.iter()) {
            bump(&mut w, row);
        }

        let line = |left: &str, mid: &str, right: &str, out: &mut String| {
            out.push_str(left);
            for (i, cw) in w.iter().enumerate() {
                if i > 0 {
                    out.push_str(mid);
                }
                out.push_str(&"─".repeat(*cw + 2));
            }
            out.push_str(right);
            out.push('\n');
        };

        let cells = |row: &Vec<String>, out: &mut String| {
            out.push('│');
            for (i, raw) in (0..cols).map(|i| row.get(i).map(String::as_str).unwrap_or("")).enumerate() {
                out.push(' ');
                out.push_str(&pad(raw, w[i], self.aligns[i]));
                out.push_str(" │");
            }
            out.push('\n');
        };

        let mut out = String::new();
        line("┌", "┬", "┐", &mut out);
        let mut header = Vec::new();
        header.extend(self.headers.iter().map(|h| bold(h, color)));
        cells(&header, &mut out);
        line("├", "┼", "┤", &mut out);
        for row in &self.rows {
            cells(row, &mut out);
        }
        if !self.footers.is_empty() {
            line("├", "┼", "┤", &mut out);
            for row in &self.footers {
                cells(row, &mut out);
            }
        }
        line("└", "┴", "┘", &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_units() {
        assert_eq!(tokens(12_400_000.0), "12.4M");
        assert_eq!(tokens(845.0), "845");
        assert_eq!(tokens(1_234.0), "1.2k");
        assert_eq!(tokens(2_600_000_000.0), "2.60B");
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(cost_cell(None), "no price");
        assert_eq!(cost_cell(Some(4.213)), "$4.21");
    }

    #[test]
    fn wide_cells_stay_aligned() {
        let mut t = Table::new(&["name", "cost"]).right(&[1]);
        t.push(vec!["模型".into(), "$1.00".into()]);
        t.push(vec!["claude".into(), "$12.00".into()]);
        let rendered = t.render(false);
        let rows: Vec<&str> = rendered.lines().collect();
        assert_eq!(width(rows[1]), width(rows[3]), "{rows:?}");
        assert!(rows[1].ends_with('│'));
    }

    #[test]
    fn truncation_keeps_display_width() {
        assert_eq!(truncate("/Users/dev/.claude/projects", 12), "/Users/dev/…");
        assert_eq!(width(&truncate("模型模型模型", 5)), 4 + 1);
    }

    #[test]
    fn byte_and_label_humans() {
        assert_eq!(bytes(900), "900 B");
        assert_eq!(bytes(3_200_000), "3.1 MB");
        assert_eq!(capitalised("day"), "Day");
    }

    #[test]
    fn time_humans() {
        assert_eq!(minutes_window(10_080), "7d");
        assert_eq!(minutes_window(60), "1h");
        assert_eq!(minutes_window(45), "45m");
        assert_eq!(ago(5_000), "5s ago");
        assert_eq!(until(3_720_000), "in 1h2m");
        assert_eq!(bar(42.0, 10), "▰▰▰▰▱▱▱▱▱▱");
        assert_eq!(bar(0.0, 10), "▱▱▱▱▱▱▱▱▱▱");
        assert_eq!(bar(100.0, 10), "▰▰▰▰▰▰▰▰▰▰");
    }
}
