//! The system prompt and task prompts. The texts live in prompts/ so they
//! are easy to edit and identical across implementations.

use crate::config::Config;

const SYSTEM: &str = include_str!("../prompts/system.md");
const SCAN: &str = include_str!("../prompts/scan.md");
const ENDPOINT: &str = include_str!("../prompts/endpoint.md");
const IMAGE: &str = include_str!("../prompts/image.md");

/// Byte-stable for a session (no timestamps or per-request data) so the
/// prompt cache keeps hitting across turns.
pub fn system(cfg: &Config) -> String {
    let rel = cfg
        .docs_dir
        .strip_prefix(&cfg.project_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".into());
    SYSTEM
        .replace("{{PROJECT_ROOT}}", &cfg.project_root.display().to_string())
        .replace("{{DOCS_DIR}}", &cfg.docs_dir.display().to_string())
        .replace("{{DOCS_REL}}", &rel)
}

pub fn scan(focus: &str) -> String {
    let focus = if focus.is_empty() {
        String::new()
    } else {
        format!(", focusing on: {focus}")
    };
    SCAN.replace("{{FOCUS}}", &focus)
}

pub fn endpoint(target: &str) -> String {
    ENDPOINT.replace("{{TARGET}}", target)
}

pub fn image(names: &[String], note: &str) -> String {
    let list = names.join(", ");
    let screens = if names.len() > 1 {
        format!(
            "These {} images are screens from the frontend/mobile app ({list}).",
            names.len()
        )
    } else {
        format!("This image is a screen from the frontend/mobile app ({list}).")
    };
    let note = if note.is_empty() {
        String::new()
    } else {
        format!("\nContext from the developer: {note}")
    };
    IMAGE
        .replace("{{SCREENS}}", &screens)
        .replace("{{NOTE}}", &note)
}
