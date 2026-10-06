//! Terminal styling, the spinner, and status lines.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn wrap(code: &str, s: &str) -> String {
    if color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn accent(s: &str) -> String {
    wrap("38;2;217;119;87", s)
}
pub fn dim(s: &str) -> String {
    wrap("2", s)
}
pub fn bold(s: &str) -> String {
    wrap("1", s)
}
pub fn red(s: &str) -> String {
    wrap("31", s)
}
pub fn green(s: &str) -> String {
    wrap("32", s)
}
pub fn yellow(s: &str) -> String {
    wrap("33", s)
}

const FRAMES: [&str; 10] = ["✻", "✼", "✽", "✾", "✿", "❀", "✿", "✾", "✽", "✼"];

/// A one-line status on stderr while Claude is working.
#[derive(Default)]
pub struct Spinner {
    inner: Mutex<Option<(Arc<AtomicBool>, JoinHandle<()>)>>,
}

impl Spinner {
    pub fn start(&self, label: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.is_some() || !std::io::stderr().is_terminal() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let label = label.to_string();
        let handle = thread::spawn(move || {
            let start = Instant::now();
            let mut i = 0;
            while !flag.load(Ordering::Relaxed) {
                let secs = start.elapsed().as_secs();
                eprint!(
                    "\r{} {} {}\x1b[K",
                    accent(FRAMES[i % FRAMES.len()]),
                    accent(&format!("{label}…")),
                    dim(&format!("({secs}s · ctrl+c to interrupt)"))
                );
                let _ = std::io::stderr().flush();
                i += 1;
                thread::sleep(Duration::from_millis(120));
            }
            eprint!("\r\x1b[K");
            let _ = std::io::stderr().flush();
        });
        *g = Some((stop, handle));
    }

    pub fn stop(&self) {
        if let Some((stop, handle)) = self.inner.lock().unwrap().take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn tool_line(name: &str, detail: &str) {
    println!(
        "{} {}{}{}{}",
        accent("●"),
        bold(name),
        dim("("),
        detail,
        dim(")")
    );
}

pub fn tool_result_line(text: &str, is_err: bool) {
    let t = if is_err { red(text) } else { dim(text) };
    println!("  {}  {t}", dim("⎿"));
}

pub fn error(text: &str) {
    eprintln!("{}", red(&format!("✗ {text}")));
}

pub fn ok(text: &str) {
    println!("{} {text}", green("✓"));
}
