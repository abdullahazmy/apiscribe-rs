//! The interactive session and command handlers shared with one-shot mode.

use std::path::Path;

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};
use serde_json::Value;

use crate::agent::{self, Agent};
use crate::api::ApiError;
use crate::config::EFFORTS;
use crate::{export, image, prompts, ui};

const SLASH_COMMANDS: [(&str, &str); 10] = [
    (
        "/scan [focus]",
        "Scan the backend and write/refresh docs for every endpoint",
    ),
    (
        "/endpoint <what>",
        "Document or update a single endpoint, e.g. /endpoint POST /api/orders",
    ),
    (
        "/image [paths…] [-- note]",
        "Map app screen(s) to the APIs to call. No path = paste from clipboard",
    ),
    (
        "/export [md|html|pdf|all]",
        "Build API_DOCUMENTATION.{md,html,pdf} in <docs>/dist",
    ),
    ("/docs", "List the documentation files"),
    (
        "/effort [level]",
        "Show or set reasoning effort (low, medium, high, xhigh, max)",
    ),
    (
        "/cost",
        "Show token usage and estimated cost for this session",
    ),
    (
        "/clear",
        "Start a fresh conversation (docs on disk are kept)",
    ),
    ("/help", "Show this help"),
    ("/exit", "Quit"),
];

fn print_help() {
    println!("{}", ui::bold("\nCommands"));
    for (cmd, desc) in SLASH_COMMANDS {
        println!("  {} {desc}", ui::accent(&format!("{cmd:<28}")));
    }
    println!("{}", ui::bold("\nTips"));
    println!(
        "{}",
        ui::dim(
            "  • Anything else you type is a chat message — ask questions or request changes to the docs."
        )
    );
    println!(
        "{}",
        ui::dim("  • Drag an image file into the terminal (or paste its path) to map that screen.")
    );
    println!(
        "{}",
        ui::dim("  • ctrl+c interrupts Claude; ctrl+c at an empty prompt (or ctrl+d) exits.\n")
    );
}

/// Parses "md,html" / "all" into export formats.
pub fn parse_formats(arg: &str) -> Result<Vec<String>, String> {
    let arg = arg.trim();
    if arg.is_empty() || arg == "all" {
        return Ok(export::FORMATS.iter().map(|s| s.to_string()).collect());
    }
    let (good, bad): (Vec<&str>, Vec<&str>) = arg
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .partition(|f| export::FORMATS.contains(f));
    if !bad.is_empty() {
        return Err(format!(
            "unknown format(s): {}. Use md, html, pdf or all",
            bad.join(", ")
        ));
    }
    Ok(good.into_iter().map(String::from).collect())
}

/// Renders the docs and prints the result.
pub fn run_export(cfg: &crate::config::Config, formats: &[String]) -> Result<(), String> {
    println!("{}", ui::dim(&format!("Exporting {}…", formats.join(", "))));
    let res = export::run(cfg, formats)?;
    let cwd = std::env::current_dir().unwrap_or_default();
    for f in res.files {
        let shown = f.strip_prefix(&cwd).map(Path::to_path_buf).unwrap_or(f);
        ui::ok(&shown.display().to_string());
    }
    for w in res.warnings {
        ui::error(&w);
    }
    Ok(())
}

/// Splits "a.png b.png -- this is checkout" into images and a note. With no
/// paths it reads the clipboard.
pub fn collect_images(args: &str) -> Result<(Vec<image::Loaded>, String), String> {
    let (path_part, note) = match args
        .find(" -- ")
        .map(|i| (i, 4))
        .or_else(|| args.starts_with("-- ").then_some((0, 3)))
    {
        Some((i, len)) => (&args[..i], args[i + len..].trim().to_string()),
        None if args.trim() == "--" => ("", String::new()),
        None => (args, String::new()),
    };
    let paths = image::parse_path_args(path_part);
    if paths.is_empty() {
        return Ok((vec![image::load_clipboard()?], note));
    }
    let imgs = paths
        .iter()
        .map(|p| image::load_file(p))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((imgs, note))
}

/// Attaches screenshots and asks Claude to map them to APIs.
pub fn send_images(a: &mut Agent, imgs: Vec<image::Loaded>, note: &str) -> Result<(), ApiError> {
    let names: Vec<String> = imgs.iter().map(|i| i.name.clone()).collect();
    println!("{}", ui::dim(&format!("Attached {}", names.join(", "))));
    let mut blocks: Vec<Value> = imgs.into_iter().map(|i| i.block).collect();
    blocks.push(agent::text(&prompts::image(&names, note)));
    a.send(blocks)
}

pub fn print_cost(a: &Agent) {
    let u = a.usage;
    let mut line = format!(
        "{} {}  {} {}  {} {}  {} {}",
        ui::dim("input"),
        u.input,
        ui::dim("output"),
        u.output,
        ui::dim("cache read"),
        u.cache_read,
        ui::dim("cache write"),
        u.cache_write
    );
    if let Some(c) = a.cost_usd() {
        line += &format!("  {} {}", ui::dim("≈"), ui::bold(&format!("${c:.4}")));
    }
    println!("{line}");
}

fn list_docs(dir: &Path) {
    let mut out = Vec::new();
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(format!("  {}", rel.to_string_lossy().replace('\\', "/")));
            }
        }
    }
    walk(dir, dir, &mut out);
    if out.is_empty() {
        println!("{}", ui::dim("No docs yet. Run /scan."));
    } else {
        out.sort();
        println!("{}", out.join("\n"));
    }
}

enum Flow {
    Continue,
    Exit,
}

fn handle(a: &mut Agent, input: &str) -> Result<Flow, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(Flow::Continue);
    }
    let api_err = |e: ApiError| e.to_string();

    // A bare dragged-in image path counts as /image.
    let paths = image::parse_path_args(input);
    if !paths.is_empty()
        && paths
            .iter()
            .all(|p| image::is_image_path(p) && Path::new(p).is_file())
    {
        let (imgs, _) = collect_images(input)?;
        send_images(a, imgs, "").map_err(api_err)?;
        return Ok(Flow::Continue);
    }
    if !input.starts_with('/') {
        a.send(vec![agent::text(input)]).map_err(api_err)?;
        return Ok(Flow::Continue);
    }

    let (cmd, args) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    let args = args.trim();
    match cmd {
        "/exit" | "/quit" => return Ok(Flow::Exit),
        "/help" => print_help(),
        "/clear" => {
            a.reset();
            ui::ok("Conversation cleared.");
        }
        "/cost" => print_cost(a),
        "/docs" => list_docs(&a.cfg.docs_dir),
        "/effort" if args.is_empty() => println!("effort: {}", a.cfg.effort),
        "/effort" if EFFORTS.contains(&args) => {
            a.cfg.effort = args.to_string();
            ui::ok(&format!("effort set to {args}"));
        }
        "/effort" => ui::error(&format!("Use one of: {}", EFFORTS.join(", "))),
        "/scan" => a
            .send(vec![agent::text(&prompts::scan(args))])
            .map_err(api_err)?,
        "/endpoint" if args.is_empty() => ui::error("Usage: /endpoint POST /api/orders"),
        "/endpoint" => a
            .send(vec![agent::text(&prompts::endpoint(args))])
            .map_err(api_err)?,
        "/image" => {
            let (imgs, note) = collect_images(args)?;
            send_images(a, imgs, &note).map_err(api_err)?;
        }
        "/export" => run_export(&a.cfg, &parse_formats(args)?)?,
        other => ui::error(&format!("Unknown command {other}. Type /help.")),
    }
    Ok(Flow::Continue)
}

/// Tab-completes slash commands.
struct SlashHelper;

impl Completer for SlashHelper {
    type Candidate = String;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let typed = &line[..pos];
        if !typed.starts_with('/') || typed.contains(' ') {
            return Ok((pos, Vec::new()));
        }
        let hits = SLASH_COMMANDS
            .iter()
            .map(|(c, _)| c.split(' ').next().unwrap().to_string())
            .filter(|c| c.starts_with(typed))
            .collect();
        Ok((0, hits))
    }
}
impl Hinter for SlashHelper {
    type Hint = String;
}
impl Highlighter for SlashHelper {}
impl Validator for SlashHelper {}
impl Helper for SlashHelper {}

/// Starts the interactive session.
pub fn run(mut a: Agent) -> Result<(), String> {
    ui::banner(
        &a.cfg.project_root.display().to_string(),
        &a.cfg.docs_dir.display().to_string(),
        &a.cfg.model,
    );
    let mut rl: Editor<SlashHelper, rustyline::history::DefaultHistory> =
        Editor::new().map_err(|e| e.to_string())?;
    rl.set_helper(Some(SlashHelper));

    loop {
        match rl.readline("› ") {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = rl.add_history_entry(line.as_str());
                }
                match handle(&mut a, &line) {
                    Ok(Flow::Exit) => break,
                    Ok(Flow::Continue) => {}
                    Err(e) => ui::error(&e),
                }
                println!();
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    print_cost(&a);
    Ok(())
}
