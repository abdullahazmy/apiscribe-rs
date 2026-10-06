//! Loads screenshots from files or the system clipboard.

use std::path::Path;
use std::process::Command;

use base64::Engine;
use serde_json::{Value, json};

const MAX_BYTES: usize = 5 << 20; // API limit per image

fn media_type(p: &str) -> Option<&'static str> {
    match Path::new(p)
        .extension()?
        .to_string_lossy()
        .to_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// An image ready to attach to a message.
pub struct Loaded {
    pub name: String,
    pub block: Value,
}

pub fn is_image_path(p: &str) -> bool {
    media_type(p).is_some()
}

/// Splits typed or pasted text into paths, handling terminal drag-and-drop
/// forms: 'quoted', "double quoted", backslash-escaped spaces, file:// URLs and ~/.
pub fn parse_path_args(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '\\' && !cfg!(windows) && chars.peek().is_some() => {
                cur.push(chars.next().unwrap())
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok();
    out.into_iter()
        .map(|p| {
            let p = match p.strip_prefix("file://") {
                Some(rest) => percent_decode(rest),
                None => p,
            };
            match (p.strip_prefix("~/"), &home) {
                (Some(rest), Some(h)) => format!("{h}/{rest}"),
                _ => p,
            }
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn load_file(file: &str) -> Result<Loaded, String> {
    let mt = media_type(file)
        .ok_or_else(|| format!("{file}: unsupported image type (use png, jpg, gif or webp)"))?;
    let data = std::fs::read(file).map_err(|_| format!("{file}: file not found"))?;
    let name = Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file.into());
    build(name, &data, mt)
}

/// Reads a PNG from the system clipboard (Wayland, X11, macOS, Windows).
pub fn load_clipboard() -> Result<Loaded, String> {
    match read_clipboard() {
        Some(d) if !d.is_empty() => build("clipboard.png".into(), &d, "image/png"),
        _ => Err("no image found on the clipboard. Copy a screenshot first, or pass a file path: /image ./screen.png".into()),
    }
}

fn build(name: String, data: &[u8], mt: &str) -> Result<Loaded, String> {
    if data.len() > MAX_BYTES {
        return Err(format!(
            "{name} is {:.1}MB; images must be under 5MB. Resize or crop it first",
            data.len() as f64 / (1 << 20) as f64
        ));
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    Ok(Loaded {
        name,
        block: json!({"type": "image", "source": {"type": "base64", "media_type": mt, "data": b64}}),
    })
}

fn output(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    let o = Command::new(cmd).args(args).output().ok()?;
    if o.status.success() && !o.stdout.is_empty() {
        Some(o.stdout)
    } else {
        None
    }
}

fn read_clipboard() -> Option<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!("apiscribe-clip-{}.png", std::process::id()));
    let tmp_s = tmp.to_string_lossy().to_string();
    let from_tmp = || {
        let b = std::fs::read(&tmp).ok();
        let _ = std::fs::remove_file(&tmp);
        b
    };
    if cfg!(target_os = "macos") {
        if let Some(b) = output("pngpaste", &["-"]) {
            return Some(b);
        }
        let _ = Command::new("osascript")
            .args([
                "-e",
                &format!("set f to open for access POSIX file \"{tmp_s}\" with write permission"),
                "-e",
                "write (the clipboard as «class PNGf») to f",
                "-e",
                "close access f",
            ])
            .status();
        return from_tmp();
    }
    if cfg!(windows) {
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; $i=[System.Windows.Forms.Clipboard]::GetImage(); if ($i) {{ $i.Save('{tmp_s}') }}"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .status();
        return from_tmp();
    }
    // Linux: Wayland first, then X11.
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && let Some(types) = output("wl-paste", &["--list-types"])
        && String::from_utf8_lossy(&types)
            .lines()
            .any(|l| l == "image/png")
        && let Some(b) = output("wl-paste", &["--type", "image/png"])
    {
        return Some(b);
    }
    output(
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
    )
    .filter(|b| b.starts_with(b"\x89PNG"))
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn parses_dragged_paths() {
        let cases: [(&str, &[&str]); 5] = [
            ("a.png b.png", &["a.png", "b.png"]),
            ("'/tmp/my shot.png'", &["/tmp/my shot.png"]),
            (r"/tmp/my\ shot.png", &["/tmp/my shot.png"]),
            ("file:///tmp/a%20b.png", &["/tmp/a b.png"]),
            ("\"x y.jpg\"  z.webp", &["x y.jpg", "z.webp"]),
        ];
        for (input, want) in cases {
            assert_eq!(parse_path_args(input), want, "{input}");
        }
    }
}
