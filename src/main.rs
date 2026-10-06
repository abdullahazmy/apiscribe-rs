//! apiscribe: an AI CLI built on Claude that documents backend APIs for
//! frontend and mobile developers.

mod agent;
mod api;
mod config;
mod export;
mod image;
mod prompts;
mod repl;
mod tools;
mod ui;

use std::process::ExitCode;
use std::sync::atomic::Ordering;

use clap::{Parser, Subcommand};

use agent::Agent;
use config::{Config, Options};

#[derive(Parser)]
#[command(
    name = "apiscribe",
    version,
    about = "AI CLI built on Claude that writes API documentation for frontend & mobile developers"
)]
struct Cli {
    /// Backend project root (default: current directory)
    #[arg(short = 'C', long, global = true)]
    project: Option<String>,
    /// Docs output directory, relative to the project (default: api-docs)
    #[arg(short = 'o', long, global = true)]
    docs_dir: Option<String>,
    /// Claude model (default: claude-opus-5-5, or $APISCRIBE_MODEL)
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Reasoning effort: low, medium, high, xhigh, max (default: high)
    #[arg(short, long, global = true)]
    effort: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan the backend, write docs, then export md/html/pdf
    #[command(alias = "scan")]
    Generate {
        /// Optional focus, e.g. "only the payments module"
        focus: Vec<String>,
        /// Skip the md/html/pdf export step
        #[arg(long)]
        no_export: bool,
        /// Export formats: md,html,pdf or all
        #[arg(short, long, default_value = "all")]
        format: String,
    },
    /// Map app screen image(s) to the APIs each screen should call (no args = clipboard)
    Image {
        images: Vec<String>,
        /// Extra context, e.g. "this is the checkout screen"
        #[arg(short, long, default_value = "")]
        note: String,
    },
    /// Document or update a single endpoint, e.g. apiscribe endpoint POST /api/orders
    Endpoint {
        #[arg(required = true)]
        what: Vec<String>,
    },
    /// Render existing docs to API_DOCUMENTATION.{md,html,pdf} (no AI calls)
    Export {
        #[arg(short, long, default_value = "all")]
        format: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let opts = Options {
        project: cli.project,
        docs_dir: cli.docs_dir,
        model: cli.model,
        effort: cli.effort,
    };
    let cfg = match Config::resolve(&opts) {
        Ok(c) => c,
        Err(e) => {
            ui::error(&e);
            return ExitCode::from(2);
        }
    };

    // ctrl+c interrupts a running turn; otherwise it exits. (At the
    // interactive prompt the line editor handles ctrl+c itself.)
    let _ = ctrlc::set_handler(|| {
        if agent::BUSY.load(Ordering::Relaxed) {
            agent::INTERRUPTED.store(true, Ordering::Relaxed);
        } else {
            std::process::exit(130);
        }
    });

    let result: Result<(), String> = match cli.command {
        None => repl::run(Agent::new(cfg)),
        Some(Cmd::Export { format }) => {
            repl::parse_formats(&format).and_then(|f| repl::run_export(&cfg, &f))
        }
        Some(Cmd::Generate {
            focus,
            no_export,
            format,
        }) => match repl::parse_formats(&format) {
            Err(e) => Err(e),
            Ok(formats) => one_shot(cfg, |a| {
                a.send(vec![agent::text(&prompts::scan(&focus.join(" ")))])
                    .map_err(|e| e.to_string())?;
                if no_export || agent::INTERRUPTED.load(Ordering::Relaxed) {
                    return Ok(());
                }
                repl::run_export(&a.cfg, &formats)
            }),
        },
        Some(Cmd::Image { images, note }) => {
            let loaded = if images.is_empty() {
                image::load_clipboard().map(|i| vec![i])
            } else {
                images.iter().map(|p| image::load_file(p)).collect()
            };
            match loaded {
                Err(e) => Err(e),
                Ok(imgs) => one_shot(cfg, |a| {
                    repl::send_images(a, imgs, &note).map_err(|e| e.to_string())
                }),
            }
        }
        Some(Cmd::Endpoint { what }) => one_shot(cfg, |a| {
            a.send(vec![agent::text(&prompts::endpoint(&what.join(" ")))])
                .map_err(|e| e.to_string())
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            ui::error(&e);
            ExitCode::FAILURE
        }
    }
}

/// Runs a single agent task and prints the cost.
fn one_shot(cfg: Config, f: impl FnOnce(&mut Agent) -> Result<(), String>) -> Result<(), String> {
    let mut a = Agent::new(cfg);
    let r = f(&mut a);
    repl::print_cost(&a);
    r
}
