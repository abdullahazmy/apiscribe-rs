//! The welcome box shown when the interactive session starts.

use std::path::Path;
use std::time::SystemTime;

use crate::ui;

/// The apiscribe mark: an API doc page with a JSON brace.
const LOGO: [&str; 5] = [
    "▗▛▀▀▀▀▀▀▀▜▖",
    "▐  {   }  ▌",
    "▐  ━━━━━  ▌",
    "▐  ━━━    ▌",
    "▝▙▄▄▄▄▄▄▄▟▘",
];

type Style = fn(&str) -> String;

fn plain(s: &str) -> String {
    s.to_string()
}

fn heading(s: &str) -> String {
    ui::accent(&ui::bold(s))
}

/// A run of text with one style; a cell is a row of segments.
type Cell = Vec<(String, Style)>;

fn seg(text: impl Into<String>, style: Style) -> (String, Style) {
    (text.into(), style)
}

/// Truncates (with …) or pads a cell to `width` columns, optionally centered.
fn fit(cell: &Cell, width: usize, center: bool) -> String {
    let mut room = width;
    let mut out = String::new();
    for (text, style) in cell {
        let n = text.chars().count();
        if n <= room {
            out += &style(text);
            room -= n;
        } else {
            let cut: String = text.chars().take(room.saturating_sub(1)).collect();
            out += &style(&(cut + "…"));
            room = 0;
            break;
        }
    }
    let used = width - room;
    let left = if center { (width - used) / 2 } else { 0 };
    format!(
        "{}{out}{}",
        " ".repeat(left),
        " ".repeat(width - used - left)
    )
}

/// Uses ~ for home and a leading … when the path is too long.
fn short_path(p: &Path, max: usize) -> String {
    let mut s = p.display().to_string();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        && let Ok(rest) = p.strip_prefix(&home)
    {
        s = if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        };
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s
    } else {
        format!(
            "…{}",
            chars[chars.len() - max + 1..].iter().collect::<String>()
        )
    }
}

fn ago(t: SystemTime) -> String {
    let s = t.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// "4 doc files · updated 2h ago", or a nudge to run /scan.
fn docs_summary(docs_dir: &Path) -> Cell {
    fn walk(dir: &Path, dist: &Path, count: &mut usize, newest: &mut Option<SystemTime>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p != dist {
                    walk(&p, dist, count, newest);
                }
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md")) {
                *count += 1;
                if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                    *newest = Some(newest.map_or(m, |n| n.max(m)));
                }
            }
        }
    }
    let (mut count, mut newest) = (0, None);
    walk(docs_dir, &docs_dir.join("dist"), &mut count, &mut newest);
    if count == 0 {
        return vec![seg("No docs yet — run ", ui::dim), seg("/scan", ui::accent)];
    }
    let plural = if count == 1 { "" } else { "s" };
    let updated = newest
        .map(|t| format!(" · updated {}", ago(t)))
        .unwrap_or_default();
    vec![
        seg(format!("{count} doc file{plural}"), plain),
        seg(updated, ui::dim),
    ]
}

pub struct BannerInfo<'a> {
    pub version: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub project_root: &'a Path,
    pub docs_dir: &'a Path,
}

/// Draws the welcome box: two columns on wide terminals, one on narrow.
pub fn render(info: &BannerInfo, columns: usize) -> Vec<String> {
    let width = columns.clamp(40, 100);
    let b = ui::accent;
    let title = format!(" apiscribe v{} ", info.version);
    let top = format!(
        "{}{}{}",
        b("╭───"),
        ui::bold(&title),
        b(&format!(
            "{}╮",
            "─".repeat(width.saturating_sub(5 + title.chars().count()))
        ))
    );
    let bottom = b(&format!("╰{}╯", "─".repeat(width - 2)));
    let row = |inner: String| format!("{}{inner}{}", b("│"), b("│"));

    let docs_rel = info
        .docs_dir
        .strip_prefix(info.project_root)
        .map(|p| p.display().to_string().replace('\\', "/"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".into());
    let two_col = width >= 76;
    let lw = if two_col { 40 } else { width - 2 };

    let mut left: Vec<Cell> = vec![
        vec![],
        vec![seg("Welcome to apiscribe!", ui::bold)],
        vec![seg("API docs for frontend & mobile teams", ui::dim)],
        vec![],
    ];
    left.extend(LOGO.iter().map(|l| vec![seg(*l, ui::accent)]));
    left.push(vec![]);
    left.push(vec![seg(
        format!("{} · effort {}", info.model, info.effort),
        ui::dim,
    )]);
    left.push(vec![seg(short_path(info.project_root, lw - 4), ui::dim)]);

    let right: Vec<Cell> = vec![
        vec![],
        vec![seg("Tips for getting started", heading)],
        vec![
            seg("/scan    ", ui::accent),
            seg("document every endpoint", plain),
        ],
        vec![
            seg("/image   ", ui::accent),
            seg("map a screen to its APIs", plain),
        ],
        vec![
            seg("/export  ", ui::accent),
            seg("build md · html · pdf", plain),
        ],
        vec![seg("/help    ", ui::accent), seg("all commands", plain)],
        vec![],
        vec![seg("Docs", heading), seg(format!("  {docs_rel}/"), ui::dim)],
        docs_summary(info.docs_dir),
    ];

    let inner = width - 2;
    let prefixed = |pad: &str, c: &Cell| {
        let mut v: Cell = vec![seg(pad, plain)];
        v.extend(c.iter().cloned());
        v
    };
    let mut lines = vec![top];
    if two_col {
        let rw = inner - lw - 1;
        let empty = Cell::new();
        for i in 0..left.len().max(right.len()) {
            let lc = left.get(i).unwrap_or(&empty);
            let rc = right.get(i).unwrap_or(&empty);
            lines.push(row(format!(
                " {} {}{}",
                fit(lc, lw - 2, true),
                b("│"),
                fit(&prefixed(" ", rc), rw, false)
            )));
        }
    } else {
        for lc in &left {
            lines.push(row(format!(" {} ", fit(lc, inner - 2, true))));
        }
        for rc in &right {
            lines.push(row(fit(&prefixed("  ", rc), inner, false)));
        }
    }
    lines.push(bottom);
    lines
}

/// Prints the welcome box sized to the terminal.
pub fn print(info: &BannerInfo) {
    let columns = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);
    for l in render(info, columns) {
        println!("{l}");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_have_equal_width() {
        let root = format!("/very/long/path/{}", "x".repeat(80));
        let info = BannerInfo {
            version: "0.1.0",
            model: "claude-opus-5-5",
            effort: "high",
            project_root: Path::new(&root),
            docs_dir: Path::new("/tmp/none"),
        };
        for cols in [30, 60, 80, 120] {
            let lines = render(&info, cols);
            let want = lines[0].chars().count();
            for l in &lines {
                assert_eq!(l.chars().count(), want, "cols={cols}: {l:?}");
            }
        }
    }
}
