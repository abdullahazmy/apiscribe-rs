//! Merges the Markdown docs and renders them to a single Markdown file, a
//! self-contained HTML page, and a PDF.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};
use regex::Regex;
use syntect::html::{ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::config::Config;

const CSS: &str = include_str!("../assets/style.css");
const SCRIPT: &str = include_str!("../assets/script.js");

pub const FORMATS: [&str; 3] = ["md", "html", "pdf"];

static METHOD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS|WS|SSE)\s+(\S.*)$").unwrap()
});
static H1_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#\s+(.+)$").unwrap());
static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\]\(([^)\s]+?\.md)(#[^)\s]*)?\)").unwrap());
static STATUS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([1-5])\d\d\b").unwrap());
static SCHEME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z]+://").unwrap());
static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

struct DocFile {
    /// Relative to the docs dir, slash-separated.
    rel: String,
    anchor: String,
    title: String,
    body: String,
}

fn collect(cfg: &Config) -> Result<Vec<DocFile>, String> {
    let dist = cfg.docs_dir.join("dist");
    let mut files = Vec::new();
    fn walk(dir: &Path, root: &Path, dist: &Path, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p != dist {
                    walk(&p, root, dist, out);
                }
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md"))
                && let Ok(rel) = p.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    walk(&cfg.docs_dir, &cfg.docs_dir, &dist, &mut files);
    let rank = |f: &str| {
        if f.eq_ignore_ascii_case("README.md") {
            0
        } else if f.starts_with("endpoints/") {
            1
        } else if f.starts_with("screens/") {
            2
        } else {
            3
        }
    };
    files.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    files
        .into_iter()
        .map(|rel| {
            let body =
                fs::read_to_string(cfg.docs_dir.join(&rel)).map_err(|e| format!("{rel}: {e}"))?;
            let title = H1_RE
                .captures(&body)
                .map(|c| c[1].trim().to_string())
                .unwrap_or_else(|| title_from_path(&rel));
            Ok(DocFile {
                anchor: file_anchor(&rel),
                rel,
                title,
                body,
            })
        })
        .collect()
}

fn file_anchor(rel: &str) -> String {
    let base = rel.to_lowercase();
    let base = base.strip_suffix(".md").unwrap_or(&base);
    format!("doc-{}", NON_ALNUM.replace_all(base, "-").trim_matches('-'))
}

fn title_from_path(rel: &str) -> String {
    let base = Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().replace(['-', '_'], " "))
        .unwrap_or_default();
    let mut c = base.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => rel.to_string(),
    }
}

/// Matches GitHub's heading anchors, so links Claude writes (#post-apiorders) resolve.
fn slugify(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                Some(c)
            } else if c.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// Lexically joins a relative link onto a doc's directory.
fn join_rel(from: &str, target: &str) -> String {
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Turns cross-file links (endpoints/orders.md#x) into in-page anchors (#x).
fn rewrite_links(d: &DocFile, known: &HashSet<String>) -> String {
    MD_LINK_RE
        .replace_all(&d.body, |c: &regex::Captures| {
            let target = &c[1];
            if SCHEME_RE.is_match(target) {
                return c[0].to_string();
            }
            let resolved = join_rel(&d.rel, target);
            if !known.contains(&resolved) {
                return c[0].to_string();
            }
            match c.get(2) {
                Some(frag) => format!("]({})", frag.as_str()),
                None => format!("](#{})", file_anchor(&resolved)),
            }
        })
        .into_owned()
}

fn project_name(cfg: &Config) -> String {
    let checks = [
        ("Cargo.toml", r#"(?m)^name\s*=\s*"([^"]+)""#),
        ("package.json", r#""name"\s*:\s*"([^"]+)""#),
        ("go.mod", r"(?m)^module\s+(\S+)"),
        ("pyproject.toml", r#"(?m)^name\s*=\s*"([^"]+)""#),
        ("composer.json", r#""name"\s*:\s*"([^"]+)""#),
    ];
    for (file, re) in checks {
        if let Ok(text) = fs::read_to_string(cfg.project_root.join(file))
            && let Some(c) = Regex::new(re).unwrap().captures(&text)
        {
            return c[1].rsplit('/').next().unwrap_or(&c[1]).to_string();
        }
    }
    cfg.project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "api".into())
}

pub struct ExportResult {
    pub files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Exports the docs in the requested formats to <docs>/dist.
pub fn run(cfg: &Config, formats: &[String]) -> Result<ExportResult, String> {
    let docs = collect(cfg)?;
    if docs.is_empty() {
        return Err(format!(
            "No Markdown docs found in {}. Run /scan first.",
            cfg.docs_dir.display()
        ));
    }
    let out_dir = cfg.docs_dir.join("dist");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let known: HashSet<String> = docs.iter().map(|d| d.rel.clone()).collect();
    let want = |f: &str| formats.iter().any(|x| x == f);
    let name = project_name(cfg);
    let generated = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut res = ExportResult {
        files: Vec::new(),
        warnings: Vec::new(),
    };

    if want("md") {
        let mut md = format!(
            "# {name} — API Documentation\n\n_Generated {generated} by apiscribe._\n\n## Contents\n\n"
        );
        for d in &docs {
            md += &format!("- [{}](#{})\n", d.title, d.anchor);
        }
        md += "\n---\n\n";
        let parts: Vec<String> = docs
            .iter()
            .map(|d| {
                format!(
                    "<a id=\"{}\"></a>\n\n{}\n",
                    d.anchor,
                    rewrite_links(d, &known).trim()
                )
            })
            .collect();
        md += &parts.join("\n---\n\n");
        let out = out_dir.join("API_DOCUMENTATION.md");
        fs::write(&out, md).map_err(|e| e.to_string())?;
        res.files.push(out);
    }

    if want("html") || want("pdf") {
        let page = render_html(&docs, &known, &name, &generated);
        if want("html") {
            let out = out_dir.join("API_DOCUMENTATION.html");
            fs::write(&out, &page).map_err(|e| e.to_string())?;
            res.files.push(out);
        }
        if want("pdf") {
            let out = out_dir.join("API_DOCUMENTATION.pdf");
            match render_pdf(&page, &out, &name) {
                Ok(()) => res.files.push(out),
                Err(e) => res.warnings.push(format!("PDF skipped: {e}")),
            }
        }
    }
    Ok(res)
}

fn esc(s: &str) -> String {
    html_escape::encode_double_quoted_attribute(s).into_owned()
}

fn highlight(code: &str, lang: &str) -> String {
    let lang = match lang {
        "ts" | "typescript" | "tsx" => "js",
        "shell" | "sh" | "zsh" | "console" => "bash",
        "yml" => "yaml",
        l => l,
    };
    let syntax = SYNTAXES
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());
    let mut generator = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        &SYNTAXES,
        ClassStyle::SpacedPrefixed { prefix: "hl-" },
    );
    for line in LinesWithEndings::from(code) {
        if generator
            .parse_html_for_line_which_includes_newline(line)
            .is_err()
        {
            return html_escape::encode_text(code).into_owned();
        }
    }
    generator.finalize()
}

struct NavItem {
    id: String,
    label: String,
    method: Option<String>,
}

/// Renders one doc, giving headings unique GitHub-style ids, badges for
/// "METHOD /path" headings, status dots for "200 OK" headings, and
/// syntax-highlighted code blocks.
fn render_doc(md: &str, used: &mut HashMap<String, usize>, nav: &mut Vec<NavItem>) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut out: Vec<Event> = Vec::new();
    let mut heading: Option<(u8, Vec<Event>, String)> = None;
    let mut code: Option<(String, String)> = None;

    for ev in Parser::new_ext(md, opts) {
        if let Some((_, buf)) = code.as_mut().map(|(l, b)| (l, b)) {
            match ev {
                Event::Text(t) => buf.push_str(&t),
                Event::End(TagEnd::CodeBlock) => {
                    let (lang, src) = code.take().unwrap();
                    let class = if lang.is_empty() {
                        String::new()
                    } else {
                        format!(" class=\"language-{}\"", esc(&lang))
                    };
                    out.push(Event::Html(
                        format!(
                            "<pre><code{class}>{}</code></pre>\n",
                            highlight(&src, &lang)
                        )
                        .into(),
                    ));
                }
                _ => {}
            }
            continue;
        }
        if let Some((level, inner, plain)) = heading.as_mut() {
            match ev {
                Event::End(TagEnd::Heading(_)) => {
                    let level = *level;
                    let plain = plain.trim().to_string();
                    let mut inner_html = String::new();
                    html::push_html(&mut inner_html, std::mem::take(inner).into_iter());
                    heading = None;

                    let base = match slugify(&plain) {
                        s if s.is_empty() => "section".to_string(),
                        s => s,
                    };
                    let n = used.entry(base.clone()).or_insert(0);
                    let id = if *n == 0 {
                        base.clone()
                    } else {
                        format!("{base}-{n}")
                    };
                    *n += 1;

                    if let Some(m) = METHOD_RE.captures(&plain) {
                        let method = m[1].to_string();
                        inner_html = format!(
                            "<span class=\"method m-{}\">{method}</span><code class=\"route\">{}</code>",
                            method.to_lowercase(),
                            html_escape::encode_text(&m[2])
                        );
                        if level <= 2 {
                            nav.push(NavItem {
                                id: id.clone(),
                                label: m[2].to_string(),
                                method: Some(method),
                            });
                        }
                    } else if level == 2 {
                        nav.push(NavItem {
                            id: id.clone(),
                            label: plain.clone(),
                            method: None,
                        });
                    }
                    if level >= 3
                        && let Some(s) = STATUS_RE.captures(&plain)
                    {
                        inner_html =
                            format!("<span class=\"status s{}xx\">{inner_html}</span>", &s[1]);
                    }
                    out.push(Event::Html(
                        format!("<h{level} id=\"{id}\"><a class=\"anchor\" href=\"#{id}\">#</a>{inner_html}</h{level}>\n").into(),
                    ));
                }
                Event::Text(ref t) | Event::Code(ref t) => {
                    plain.push_str(t);
                    inner.push(ev.clone());
                }
                other => inner.push(other),
            }
            continue;
        }
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                heading = Some((level as u8, Vec::new(), String::new()))
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            other => out.push(other),
        }
    }
    let mut body = String::new();
    html::push_html(&mut body, out.into_iter());
    // Wrap tables so wide ones scroll instead of breaking the layout.
    body.replace("<table>", "<div class=\"table-wrap\"><table>")
        .replace("</table>", "</table></div>")
}

fn render_html(docs: &[DocFile], known: &HashSet<String>, name: &str, generated: &str) -> String {
    let mut used = HashMap::new();
    let mut sections = Vec::new();
    let mut groups = Vec::new();
    for d in docs {
        let mut nav = Vec::new();
        let body = render_doc(&rewrite_links(d, known), &mut used, &mut nav);
        sections.push(format!(
            "<section class=\"doc\" id=\"{}\" data-file=\"{}\">\n{body}\n</section>",
            d.anchor,
            esc(&d.rel)
        ));
        let items: String = nav
            .iter()
            .map(|n| {
                let badge = n
                    .method
                    .as_ref()
                    .map(|m| format!("<span class=\"method m-{}\">{m}</span>", m.to_lowercase()))
                    .unwrap_or_default();
                format!(
                    "<li><a href=\"#{}\">{badge}<span class=\"label\">{}</span></a></li>",
                    n.id,
                    html_escape::encode_text(&n.label)
                )
            })
            .collect();
        let list = if items.is_empty() {
            String::new()
        } else {
            format!("<ul>{items}</ul>")
        };
        groups.push(format!(
            "<div class=\"nav-group\"><a class=\"nav-title\" href=\"#{}\">{}</a>{list}</div>",
            d.anchor,
            html_escape::encode_text(&d.title)
        ));
    }
    let n = html_escape::encode_text(name);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{n} — API Documentation</title>
<style>{CSS}</style>
</head>
<body>
<button class="menu" aria-label="Toggle navigation" onclick="document.body.classList.toggle('nav-open')">☰</button>
<aside class="sidebar">
  <div class="brand"><div class="brand-name">{n}</div><div class="brand-sub">API Documentation</div></div>
  <input class="filter" type="search" placeholder="Filter endpoints…" aria-label="Filter endpoints">
  <nav>{groups}</nav>
</aside>
<main>
  <header class="cover">
    <div class="eyebrow">API Reference</div>
    <h1 class="cover-title">{n}</h1>
    <p class="cover-sub">Integration guide for frontend and mobile developers · generated {generated}</p>
  </header>
  {sections}
  <footer>Generated by apiscribe · {generated}</footer>
</main>
<script>{SCRIPT}</script>
</body>
</html>"#,
        groups = groups.join("\n"),
        sections = sections.join("\n"),
    )
}

fn find_chrome() -> Result<PathBuf, String> {
    let mut candidates: Vec<String> = ["APISCRIBE_CHROME", "CHROME_PATH"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .collect();
    let defaults: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ]
    } else if cfg!(windows) {
        &[
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        ]
    } else {
        &[
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/microsoft-edge",
            "/snap/bin/chromium",
        ]
    };
    candidates.extend(defaults.iter().map(|s| s.to_string()));
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "no Chrome/Chromium/Edge found. Install one or set APISCRIBE_CHROME=/path/to/chrome"
                .into()
        })
}

fn render_pdf(page: &str, out: &Path, name: &str) -> Result<(), String> {
    use headless_chrome::types::PrintToPdfOptions;
    use headless_chrome::{Browser, LaunchOptions};

    let exe = find_chrome()?;
    let tmp = std::env::temp_dir().join(format!("apiscribe-{}.html", std::process::id()));
    fs::write(&tmp, page).map_err(|e| e.to_string())?;
    let result = (|| -> Result<Vec<u8>, String> {
        let opts = LaunchOptions::default_builder()
            .path(Some(exe))
            .sandbox(false)
            .build()
            .map_err(|e| e.to_string())?;
        let browser = Browser::new(opts).map_err(|e| e.to_string())?;
        let tab = browser.new_tab().map_err(|e| e.to_string())?;
        let url = format!("file://{}", tmp.to_string_lossy().replace('\\', "/"));
        tab.navigate_to(&url)
            .and_then(|t| t.wait_until_navigated())
            .map_err(|e| e.to_string())?;
        const MM: f64 = 1.0 / 25.4; // inches per millimetre
        let esc_name = html_escape::encode_text(name);
        tab.print_to_pdf(Some(PrintToPdfOptions {
            print_background: Some(true),
            paper_width: Some(210.0 * MM),
            paper_height: Some(297.0 * MM),
            margin_top: Some(18.0 * MM),
            margin_bottom: Some(18.0 * MM),
            margin_left: Some(14.0 * MM),
            margin_right: Some(14.0 * MM),
            display_header_footer: Some(true),
            header_template: Some(format!(
                r#"<div style="font-size:8px;color:#888;width:100%;padding:0 14mm;font-family:sans-serif">{esc_name} · API Documentation</div>"#
            )),
            footer_template: Some(
                r#"<div style="font-size:8px;color:#888;width:100%;padding:0 14mm;text-align:right;font-family:sans-serif"><span class="pageNumber"></span> / <span class="totalPages"></span></div>"#.into(),
            ),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())
    })();
    let _ = fs::remove_file(&tmp);
    fs::write(out, result?).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_matches_github() {
        assert_eq!(slugify("POST /api/orders"), "post-apiorders");
        assert_eq!(slugify("GET /users/{id}"), "get-usersid");
        assert_eq!(slugify("200 OK"), "200-ok");
        assert_eq!(slugify("Authentication & Tokens"), "authentication--tokens");
    }

    #[test]
    fn rewrites_cross_file_links() {
        let known: HashSet<String> = ["README.md", "endpoints/auth.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let d = DocFile {
            rel: "endpoints/auth.md".into(),
            anchor: String::new(),
            title: String::new(),
            body: "[home](../README.md) [login](auth.md#post-login) [ext](https://x.dev/a.md)"
                .into(),
        };
        assert_eq!(
            rewrite_links(&d, &known),
            "[home](#doc-readme) [login](#post-login) [ext](https://x.dev/a.md)"
        );
    }
}
