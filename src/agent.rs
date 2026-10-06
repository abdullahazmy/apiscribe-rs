//! The streaming tool-use loop.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::api::{Api, ApiError, Event, Usage};
use crate::config::Config;
use crate::{prompts, tools, ui};

const MAX_TOOL_ROUNDS: usize = 200;

const BETAS: [&str; 2] = [
    // Re-run a safety-classifier refusal on Anthropic's recommended fallback model.
    "server-side-fallback-2026-07-01",
    // Surface Claude's short progress notes between tool calls.
    "thinking-display-updates-2026-08-18",
];

/// Set by the ctrl+c handler while a turn is running.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// True while a turn is running, so ctrl+c interrupts instead of exiting.
pub static BUSY: AtomicBool = AtomicBool::new(false);

/// $ per million tokens: input, output, cache read, cache write.
fn prices(model: &str) -> Option<[f64; 4]> {
    match model {
        "claude-opus-5-5" => Some([4.0, 20.0, 0.2, 5.0]),
        "claude-sonnet-5-5" => Some([2.0, 10.0, 0.2, 2.5]),
        "claude-fable-5-1" => Some([10.0, 50.0, 0.25, 12.5]),
        "claude-haiku-4-5" => Some([1.0, 5.0, 0.1, 1.25]),
        _ => None,
    }
}

/// A conversation with an append-only history. History is never rewritten,
/// which keeps the prompt cache warm and thinking blocks valid.
pub struct Agent {
    pub cfg: Config,
    api: Option<Api>,
    system: String,
    tools: Value,
    messages: Vec<Value>,
    pub usage: Usage,
}

/// Prints streamed thinking and text.
struct Printer {
    at_line_start: bool,
    in_thinking: bool,
}

impl Printer {
    fn write(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        print!("{s}");
        let _ = std::io::stdout().flush();
        self.at_line_start = s.ends_with('\n');
    }
}

impl Agent {
    pub fn new(cfg: Config) -> Agent {
        let system = prompts::system(&cfg);
        Agent {
            api: Api::from_env().ok(),
            system,
            tools: tools::definitions(),
            messages: Vec::new(),
            usage: Usage::default(),
            cfg,
        }
    }

    pub fn reset(&mut self) {
        self.messages.clear();
    }

    /// Estimated session cost, or None for models without a known price.
    pub fn cost_usd(&self) -> Option<f64> {
        let p = prices(&self.cfg.model)?;
        let u = self.usage;
        Some(
            (u.input as f64 * p[0]
                + u.output as f64 * p[1]
                + u.cache_read as f64 * p[2]
                + u.cache_write as f64 * p[3])
                / 1e6,
        )
    }

    /// Adds one user turn and runs tools until Claude is done. ctrl+c
    /// interrupts the turn; the history stays a valid prefix.
    pub fn send(&mut self, content: Vec<Value>) -> Result<(), ApiError> {
        if self.api.is_none() {
            self.api = Some(Api::from_env()?);
        }
        let api = self.api.as_ref().expect("api initialised above");
        self.messages
            .push(json!({"role": "user", "content": content}));
        INTERRUPTED.store(false, Ordering::Relaxed);
        BUSY.store(true, Ordering::Relaxed);
        let result = run_turn(
            api,
            &self.cfg,
            &self.system,
            &self.tools,
            &mut self.messages,
            &mut self.usage,
        );
        BUSY.store(false, Ordering::Relaxed);
        match result {
            Err(ApiError::Interrupted) => {
                println!("{}", ui::yellow("\n⏹  Interrupted."));
                Ok(())
            }
            other => other,
        }
    }
}

fn run_turn(
    api: &Api,
    cfg: &Config,
    system: &str,
    tool_defs: &Value,
    messages: &mut Vec<Value>,
    usage: &mut Usage,
) -> Result<(), ApiError> {
    let spinner = ui::Spinner::default();
    let mut json_retries = 0;

    for _ in 0..MAX_TOOL_ROUNDS {
        spinner.start("Thinking");
        let mut p = Printer {
            at_line_start: true,
            in_thinking: false,
        };
        let body = json!({
            "model": cfg.model,
            "max_tokens": 64000,
            "stream": true,
            "fallbacks": "default",
            "thinking": {"type": "adaptive", "display": "updates"},
            "output_config": {"effort": cfg.effort},
            "cache_control": {"type": "ephemeral"},
            "system": system,
            "tools": tool_defs,
            "messages": messages,
        });
        let result = api.stream(&body, &BETAS, &INTERRUPTED, |ev| match ev {
            Event::BlockStart(kind) => {
                if kind == "text" && !p.at_line_start {
                    p.write("\n");
                }
            }
            Event::Thinking(t) => {
                if t.is_empty() {
                    return;
                }
                spinner.stop();
                if !p.in_thinking {
                    if !p.at_line_start {
                        p.write("\n");
                    }
                    p.write(&ui::dim("∴ "));
                    p.in_thinking = true;
                }
                p.write(&ui::dim(t));
            }
            Event::Text(t) => {
                spinner.stop();
                if p.in_thinking {
                    p.write("\n\n");
                    p.in_thinking = false;
                }
                p.write(t);
            }
            Event::BlockStop => {
                if p.in_thinking {
                    p.write("\n");
                    p.in_thinking = false;
                }
            }
        });
        spinner.stop();
        let mut msg = result?;
        if !p.at_line_start {
            println!();
        }
        usage.input += msg.usage.input;
        usage.output += msg.usage.output;
        usage.cache_read += msg.usage.cache_read;
        usage.cache_write += msg.usage.cache_write;

        if msg.stop_reason == "refusal" {
            // Discard the partial response; the history stays a valid prefix.
            let cat = msg.stop_details["category"]
                .as_str()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            ui::error(&format!(
                "Claude declined this request{cat}. Try rephrasing it."
            ));
            return Ok(());
        }

        let tool_uses: Vec<(String, String, Value)> = msg
            .content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| {
                (
                    b["id"].as_str().unwrap_or("").to_string(),
                    b["name"].as_str().unwrap_or("").to_string(),
                    b["input"].clone(),
                )
            })
            .collect();

        if msg.stop_reason == "max_tokens" {
            // A tool input cut off at max_tokens is incomplete; never run it.
            for &i in &msg.invalid_tool_inputs {
                msg.content[i]["input"] = json!({});
            }
            messages.push(json!({"role": "assistant", "content": msg.content}));
            if tool_uses.is_empty() {
                ui::error("Response hit the output limit. Ask Claude to continue.");
                return Ok(());
            }
            let results: Vec<Value> = tool_uses
                .iter()
                .map(|(id, _, _)| json!({"type": "tool_result", "tool_use_id": id, "is_error": true,
                    "content": "Output limit reached before this tool input was complete; it was not executed. Split the work into smaller files and retry."}))
                .collect();
            messages.push(json!({"role": "user", "content": results}));
            continue;
        }

        // With eager input streaming the server does not validate tool input;
        // re-issue the turn when a tool input is not even valid JSON.
        if !msg.invalid_tool_inputs.is_empty() {
            json_retries += 1;
            if json_retries > 2 {
                return Err(ApiError::Stream(
                    "Claude produced invalid tool input JSON three times in a row".into(),
                ));
            }
            eprintln!(
                "{}",
                ui::dim("(tool input was not valid JSON — retrying the turn)")
            );
            continue;
        }
        json_retries = 0;

        messages.push(json!({"role": "assistant", "content": msg.content}));
        if msg.stop_reason == "pause_turn" {
            continue;
        }
        if tool_uses.is_empty() {
            return Ok(());
        }

        let mut results = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in &tool_uses {
            ui::tool_line(pretty_name(name), &tools::describe(name, input));
            let out = tools::run(cfg, name, input);
            ui::tool_result_line(&out.summary, out.is_error);
            let mut r = json!({"type": "tool_result", "tool_use_id": id, "content": out.content});
            if out.is_error {
                r["is_error"] = json!(true);
            }
            results.push(r);
        }
        messages.push(json!({"role": "user", "content": results}));
        if INTERRUPTED.load(Ordering::Relaxed) {
            return Err(ApiError::Interrupted);
        }
    }
    ui::error(&format!("Stopped after {MAX_TOOL_ROUNDS} tool rounds."));
    Ok(())
}

fn pretty_name(name: &str) -> &str {
    match name {
        "list_files" => "List",
        "read_file" => "Read",
        "search" => "Search",
        "write_doc" => "Write",
        other => other,
    }
}

/// Wraps a string as a text content block.
pub fn text(s: &str) -> Value {
    json!({"type": "text", "text": s})
}
