//! The file tools Claude uses to read the backend and write documentation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{Config, confine};

const MAX_LIST: usize = 1500;
const MAX_READ_LINES: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
const MAX_MATCHES: usize = 250;

const IGNORED_DIRS: [&str; 25] = [
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    "dist",
    "build",
    "out",
    "coverage",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "target",
    "bin",
    "obj",
    ".next",
    ".nuxt",
    ".idea",
    ".vscode",
    ".gradle",
    ".dart_tool",
    "storage",
    "tmp",
];
const DOC_EXTENSIONS: [&str; 4] = ["md", "json", "yaml", "yml"];

/// Tool schemas sent to Claude, in a stable order so the prompt cache keeps hitting.
pub fn definitions() -> Value {
    json!([
        {
            "name": "list_files",
            "description": "List files under a directory of the backend project (relative to the project root). Common build/vendor folders are skipped. Use `glob` (e.g. \"**/*.controller.ts\", \"routes/**\") to filter.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory relative to the project root. Defaults to the root."},
                    "glob": {"type": "string", "description": "Optional glob filter matched against the path relative to `path`."},
                    "max_depth": {"type": "integer", "description": "Maximum directory depth (default 12)."}
                },
                "additionalProperties": false
            }
        },
        {
            "name": "read_file",
            "description": format!("Read a text file from the backend project with line numbers. Returns at most {MAX_READ_LINES} lines per call; use offset/limit for long files."),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the project root."},
                    "offset": {"type": "integer", "description": "1-based line to start from."},
                    "limit": {"type": "integer", "description": "Number of lines to read."}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "search",
            "description": "Regex search across the backend project's files (ripgrep syntax). Returns file:line:match. Great for finding route registrations, decorators, DTOs, validators, middleware, and error handlers.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for."},
                    "path": {"type": "string", "description": "Directory or file to search, relative to the project root."},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. \"*.py\"."},
                    "ignore_case": {"type": "boolean"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        },
        {
            "name": "write_doc",
            "description": "Create or overwrite a documentation file inside the docs directory. `path` is relative to the docs directory (e.g. \"README.md\", \"endpoints/users.md\", \"screens/login.md\"). Only .md, .json, .yaml and .yml files are allowed. Write one complete file per call.",
            "eager_input_streaming": true,
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the docs directory."},
                    "content": {"type": "string", "description": "Full file content."}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }
        }
    ])
}

/// The result of one tool call.
pub struct Outcome {
    /// Sent back to Claude.
    pub content: String,
    pub is_error: bool,
    /// Short line for the terminal.
    pub summary: String,
}

fn ok(content: String, summary: String) -> Outcome {
    Outcome {
        content,
        is_error: false,
        summary,
    }
}

fn fail(msg: impl Into<String>) -> Outcome {
    let msg = msg.into();
    Outcome {
        content: msg.clone(),
        is_error: true,
        summary: msg,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {
    path: Option<String>,
    glob: Option<String>,
    max_depth: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    ignore_case: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    path: String,
    content: String,
}

/// One-line description of a call for the terminal.
pub fn describe(name: &str, input: &Value) -> String {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match name {
        "list_files" => {
            let mut parts = vec![if s("path").is_empty() {
                ".".into()
            } else {
                s("path")
            }];
            if !s("glob").is_empty() {
                parts.push(s("glob"));
            }
            parts.join(", ")
        }
        "read_file" | "write_doc" => s("path"),
        "search" => {
            let mut d = format!("\"{}\"", s("pattern"));
            if !s("path").is_empty() {
                d += &format!(" in {}", s("path"));
            }
            if !s("glob").is_empty() {
                d += &format!(" ({})", s("glob"));
            }
            d
        }
        _ => String::new(),
    }
}

/// Executes a tool call. Input is validated strictly because eager input
/// streaming means the server no longer validates it.
pub fn run(cfg: &Config, name: &str, input: &Value) -> Outcome {
    fn parse<T: for<'de> Deserialize<'de>>(name: &str, input: &Value) -> Result<T, Outcome> {
        serde_json::from_value(input.clone()).map_err(|e| {
            fail(format!(
                "Invalid input for {name}: {e}. Re-issue the call with complete, valid JSON."
            ))
        })
    }
    let result = match name {
        "list_files" => parse(name, input).map(|i| list_files(cfg, i)),
        "read_file" => parse(name, input).map(|i| read_file(cfg, i)),
        "search" => parse(name, input).map(|i| search(cfg, i)),
        "write_doc" => parse(name, input).map(|i| write_doc(cfg, i)),
        _ => Err(fail(format!("Unknown tool: {name}"))),
    };
    result.unwrap_or_else(|e| e)
}

fn rel_to_root(cfg: &Config, abs: &Path) -> String {
    abs.strip_prefix(&cfg.project_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".into())
}

fn list_files(cfg: &Config, i: ListInput) -> Outcome {
    let base = match confine(&cfg.project_root, i.path.as_deref().unwrap_or(".")) {
        Ok(b) => b,
        Err(e) => return fail(e),
    };
    let matcher = i.glob.as_deref().map(glob_to_regex);
    let depth = i.max_depth.filter(|d| (1..=30).contains(d)).unwrap_or(12);
    let (files, truncated) = walk_files(cfg, &base, matcher.as_ref(), depth);
    let mut content = if files.is_empty() {
        "(no files)".to_string()
    } else {
        files.join("\n")
    };
    let mut summary = format!("{} files", files.len());
    if truncated {
        content += &format!("\n… truncated at {MAX_LIST} entries; narrow with path/glob.");
        summary = format!("{}+ files", files.len());
    }
    ok(content, summary)
}

fn walk_files(
    cfg: &Config,
    base: &Path,
    matcher: Option<&Regex>,
    max_depth: usize,
) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    #[allow(clippy::too_many_arguments)]
    fn walk(
        cfg: &Config,
        base: &Path,
        dir: &Path,
        depth: usize,
        max_depth: usize,
        matcher: Option<&Regex>,
        out: &mut Vec<String>,
        truncated: &mut bool,
    ) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if *truncated {
                return;
            }
            let path = e.path();
            if path == cfg.docs_dir {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if IGNORED_DIRS.contains(&name.as_str())
                    || (name.starts_with('.') && name != ".github")
                {
                    continue;
                }
                if depth < max_depth {
                    walk(
                        cfg,
                        base,
                        &path,
                        depth + 1,
                        max_depth,
                        matcher,
                        out,
                        truncated,
                    );
                }
            } else if ft.is_file() {
                if let Some(m) = matcher {
                    let rel = path
                        .strip_prefix(base)
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    if !m.is_match(&rel) && !m.is_match(&name) {
                        continue;
                    }
                }
                out.push(rel_to_root(cfg, &path));
                if out.len() >= MAX_LIST {
                    *truncated = true;
                    return;
                }
            }
        }
    }
    walk(
        cfg,
        base,
        base,
        1,
        max_depth,
        matcher,
        &mut out,
        &mut truncated,
    );
    (out, truncated)
}

fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8000)].contains(&0)
}

fn read_file(cfg: &Config, i: ReadInput) -> Outcome {
    let abs = match confine(&cfg.project_root, &i.path) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let Ok(meta) = fs::metadata(&abs) else {
        return fail(format!("{}: file not found", i.path));
    };
    if !meta.is_file() {
        return fail(format!("{} is not a file", i.path));
    }
    if meta.len() > 5 << 20 {
        return fail(format!("{} is larger than 5MB; use search instead", i.path));
    }
    let data = match fs::read(&abs) {
        Ok(d) => d,
        Err(e) => return fail(e.to_string()),
    };
    if is_binary(&data) {
        return fail(format!("{} looks like a binary file", i.path));
    }
    let text = String::from_utf8_lossy(&data);
    let lines: Vec<&str> = text.split('\n').collect();
    let start = i.offset.unwrap_or(1).max(1) - 1;
    let limit = i
        .limit
        .filter(|l| (1..=MAX_READ_LINES).contains(l))
        .unwrap_or(MAX_READ_LINES);
    let end = lines.len().min(start + limit);
    let start = start.min(end);
    let mut body: Vec<String> = Vec::with_capacity(end - start);
    for (n, l) in lines[start..end].iter().enumerate() {
        let l = if l.len() > MAX_LINE_LEN {
            let cut = (0..=MAX_LINE_LEN)
                .rev()
                .find(|&c| l.is_char_boundary(c))
                .unwrap_or(0);
            format!("{}…", &l[..cut])
        } else {
            l.to_string()
        };
        body.push(format!("{:>6}\t{l}", start + n + 1));
    }
    let mut content = body.join("\n");
    if end < lines.len() {
        content += &format!(
            "\n… {} more lines (continue with offset={})",
            lines.len() - end,
            end + 1
        );
    }
    ok(content, format!("{} lines", end - start))
}

fn search(cfg: &Config, i: SearchInput) -> Outcome {
    let target = match confine(&cfg.project_root, i.path.as_deref().unwrap_or(".")) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let lines = match ripgrep(cfg, &target, &i) {
        Some(Ok(l)) => l,
        Some(Err(e)) => return fail(format!("search failed: {e}")),
        None => match native_search(cfg, &target, &i) {
            Ok(l) => l,
            Err(e) => return fail(format!("search failed: {e}")),
        },
    };
    let total = lines.len();
    let mut content = if total == 0 {
        "(no matches)".into()
    } else {
        lines[..total.min(MAX_MATCHES)].join("\n")
    };
    if total > MAX_MATCHES {
        content += &format!(
            "\n… {} more matches; narrow the pattern or path.",
            total - MAX_MATCHES
        );
    }
    ok(content, format!("{total} matches"))
}

/// Runs ripgrep; `None` means it is not installed.
fn ripgrep(cfg: &Config, target: &Path, i: &SearchInput) -> Option<Result<Vec<String>, String>> {
    let mut cmd = Command::new("rg");
    cmd.current_dir(&cfg.project_root).args([
        "-n",
        "--no-heading",
        "--color=never",
        "--max-columns=400",
        "--max-count=50",
    ]);
    if i.ignore_case.unwrap_or(false) {
        cmd.arg("-i");
    }
    if let Some(g) = &i.glob {
        cmd.args(["-g", g]);
    }
    for d in IGNORED_DIRS {
        cmd.args(["-g", &format!("!{d}/")]);
    }
    if let Ok(rel) = cfg.docs_dir.strip_prefix(&cfg.project_root) {
        cmd.args([
            "-g",
            &format!("!{}/", rel.to_string_lossy().replace('\\', "/")),
        ]);
    }
    cmd.args(["-e", &i.pattern, &rel_to_root(cfg, target)]);
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err(e.to_string())),
    };
    Some(match out.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()),
        Some(1) => Ok(Vec::new()), // no matches
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    })
}

/// Fallback when ripgrep is not installed.
fn native_search(cfg: &Config, target: &Path, i: &SearchInput) -> Result<Vec<String>, String> {
    let pat = if i.ignore_case.unwrap_or(false) {
        format!("(?i){}", i.pattern)
    } else {
        i.pattern.clone()
    };
    let re = Regex::new(&pat).map_err(|e| e.to_string())?;
    let files = if target.is_file() {
        vec![rel_to_root(cfg, target)]
    } else {
        let m = i.glob.as_deref().map(glob_to_regex);
        walk_files(cfg, target, m.as_ref(), 30).0
    };
    let mut out = Vec::new();
    for f in files {
        let Ok(data) = fs::read(cfg.project_root.join(&f)) else {
            continue;
        };
        if is_binary(&data) {
            continue;
        }
        for (n, line) in String::from_utf8_lossy(&data).lines().enumerate() {
            if re.is_match(line) {
                let cut = (0..=line.len().min(400))
                    .rev()
                    .find(|&c| line.is_char_boundary(c))
                    .unwrap_or(0);
                out.push(format!("{f}:{}:{}", n + 1, &line[..cut]));
            }
        }
        if out.len() > MAX_MATCHES * 4 {
            break;
        }
    }
    Ok(out)
}

fn write_doc(cfg: &Config, i: WriteInput) -> Outcome {
    if i.path.is_empty() {
        return fail("path is required");
    }
    let abs: PathBuf = match confine(&cfg.docs_dir, &i.path) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let ext = abs
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !DOC_EXTENSIONS.contains(&ext.as_str()) {
        return fail("Only .md, .json, .yaml and .yml files can be written");
    }
    if let Some(parent) = abs.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return fail(e.to_string());
    }
    let existed = abs.exists();
    let mut content = i.content;
    if !content.ends_with('\n') {
        content.push('\n');
    }
    if let Err(e) = fs::write(&abs, &content) {
        return fail(e.to_string());
    }
    let n = content.matches('\n').count();
    let verb = if existed { "updated" } else { "created" };
    let rel = abs
        .strip_prefix(&cfg.docs_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let mut cap = verb.to_string();
    cap[..1].make_ascii_uppercase();
    ok(
        format!("{cap} {rel} ({n} lines)"),
        format!("{verb} · {n} lines"),
    )
}

/// Converts a glob supporting **, *, ? and {a,b} to a regex.
pub fn glob_to_regex(glob: &str) -> Regex {
    let chars: Vec<char> = glob.chars().collect();
    let mut re = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                i += 1;
                if chars.get(i + 1) == Some(&'/') {
                    i += 1;
                    re += "(?:.*/)?";
                } else {
                    re += ".*";
                }
            }
            '*' => re += "[^/]*",
            '?' => re += "[^/]",
            '{' => match chars[i..].iter().position(|&c| c == '}') {
                Some(end) => {
                    let inner: String = chars[i + 1..i + end].iter().collect();
                    let alts: Vec<String> = inner.split(',').map(regex::escape).collect();
                    re += &format!("(?:{})", alts.join("|"));
                    i += end;
                }
                None => re += r"\{",
            },
            c => re += &regex::escape(&c.to_string()),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching() {
        let cases = [
            ("**/*.ts", "src/routes/users.ts", true),
            ("**/*.ts", "users.ts", true),
            ("*.ts", "src/users.ts", false),
            ("routes/**", "routes/a/b.js", true),
            ("*.{js,ts}", "app.js", true),
            ("*.{js,ts}", "app.py", false),
            ("user?.go", "users.go", true),
        ];
        for (glob, path, want) in cases {
            assert_eq!(glob_to_regex(glob).is_match(path), want, "{glob} on {path}");
        }
    }
}
