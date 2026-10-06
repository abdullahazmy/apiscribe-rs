//! Runtime settings and path confinement.

use std::env;
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone)]
pub struct Config {
    /// Root of the backend being documented. All reads are confined here.
    pub project_root: PathBuf,
    /// Where generated Markdown lives. All writes are confined here.
    pub docs_dir: PathBuf,
    pub model: String,
    pub effort: String,
}

#[derive(Debug, Default, Clone)]
pub struct Options {
    pub project: Option<String>,
    pub docs_dir: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

fn pick(flag: &Option<String>, var: &str, default: &str) -> String {
    flag.clone()
        .filter(|s| !s.is_empty())
        .or_else(|| env::var(var).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| default.to_string())
}

impl Config {
    /// Applies flags, then environment variables, then defaults.
    pub fn resolve(o: &Options) -> Result<Config, String> {
        let cwd = env::current_dir().map_err(|e| e.to_string())?;
        let root = match &o.project {
            Some(p) if !p.is_empty() => normalize(&cwd.join(p)),
            _ => cwd,
        };
        let docs = pick(&o.docs_dir, "APISCRIBE_DOCS_DIR", "api-docs");
        let effort = pick(&o.effort, "APISCRIBE_EFFORT", "high");
        if !EFFORTS.contains(&effort.as_str()) {
            return Err(format!(
                "invalid effort \"{effort}\"; use one of: {}",
                EFFORTS.join(", ")
            ));
        }
        Ok(Config {
            docs_dir: normalize(&root.join(docs)),
            project_root: root,
            model: pick(&o.model, "APISCRIBE_MODEL", DEFAULT_MODEL),
            effort,
        })
    }
}

/// Lexically cleans a path: removes `.` and resolves `..` without touching the filesystem.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolves `p` against `root` and refuses anything that escapes it.
pub fn confine(root: &Path, p: &str) -> Result<PathBuf, String> {
    let target = normalize(&root.join(p));
    if target.starts_with(root) {
        Ok(target)
    } else {
        Err(format!("path \"{p}\" is outside {}", root.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confine_blocks_escapes() {
        let root = Path::new("/srv/app");
        for ok in ["src/main.rs", ".", "a/../b"] {
            assert!(confine(root, ok).is_ok(), "{ok} should be allowed");
        }
        for bad in ["..", "../etc/passwd", "a/../../x", "/etc/passwd"] {
            assert!(confine(root, bad).is_err(), "{bad} should be refused");
        }
    }
}
