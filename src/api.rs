//! A minimal streaming client for the Claude Messages API over raw HTTP.
//! (There is no official Anthropic SDK for Rust.)

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{Value, json};

const API_VERSION: &str = "2023-06-01";
const MAX_RETRIES: u32 = 2;

#[derive(Debug)]
pub enum ApiError {
    /// No credentials in the environment.
    NoCredentials,
    /// Non-2xx HTTP response.
    Status { code: u16, message: String },
    /// An `error` event in the middle of the stream (e.g. overloaded).
    Stream(String),
    /// Network failure.
    Network(String),
    /// The user pressed ctrl+c.
    Interrupted,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NoCredentials => write!(
                f,
                "No Anthropic credentials found. Set ANTHROPIC_API_KEY (get one at https://console.anthropic.com)."
            ),
            ApiError::Status { code: 401, .. } => {
                write!(f, "Not authenticated. Check ANTHROPIC_API_KEY.")
            }
            ApiError::Status { code: 403, message } => write!(f, "Permission denied: {message}"),
            ApiError::Status { code: 404, message } => {
                write!(f, "Not found (check the model name): {message}")
            }
            ApiError::Status { code: 429, .. } => {
                write!(f, "Rate limited — wait a moment and try again.")
            }
            ApiError::Status { code: 400, message } => write!(f, "Bad request: {message}"),
            ApiError::Status { code, message } => write!(f, "API error {code}: {message}"),
            ApiError::Stream(m) => write!(f, "API stream error: {m}"),
            ApiError::Network(m) => write!(f, "Network error: {m}"),
            ApiError::Interrupted => write!(f, "Interrupted"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Token counts from one response.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// A fully accumulated assistant message.
#[derive(Debug, Default)]
pub struct Message {
    /// Content blocks exactly as the API produced them, ready to append to history.
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub stop_details: Value,
    pub usage: Usage,
    /// Indices of tool_use blocks whose streamed input was not valid JSON.
    pub invalid_tool_inputs: Vec<usize>,
}

/// Display events for the terminal.
pub enum Event<'a> {
    BlockStart(&'a str),
    Thinking(&'a str),
    Text(&'a str),
    BlockStop,
}

pub struct Api {
    http: Client,
    base_url: String,
    auth: (String, String),
}

impl Api {
    /// Reads ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN) and ANTHROPIC_BASE_URL.
    pub fn from_env() -> Result<Api, ApiError> {
        let auth = match (
            std::env::var("ANTHROPIC_API_KEY"),
            std::env::var("ANTHROPIC_AUTH_TOKEN"),
        ) {
            (Ok(k), _) if !k.is_empty() => ("x-api-key".to_string(), k),
            (_, Ok(t)) if !t.is_empty() => ("authorization".to_string(), format!("Bearer {t}")),
            _ => return Err(ApiError::NoCredentials),
        };
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(None::<Duration>)
            .build()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        Ok(Api {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
        })
    }

    /// Streams POST /v1/messages, calling `on_event` for display and
    /// returning the accumulated message. Retries 429/5xx/connection errors
    /// before the stream starts. `interrupted` aborts promptly.
    pub fn stream(
        &self,
        body: &Value,
        betas: &[&str],
        interrupted: &AtomicBool,
        mut on_event: impl FnMut(Event),
    ) -> Result<Message, ApiError> {
        let rx = self.open(body, betas, interrupted)?;
        let mut msg = Message::default();
        let mut partial: HashMap<usize, String> = HashMap::new();

        loop {
            if interrupted.load(Ordering::Relaxed) {
                return Err(ApiError::Interrupted);
            }
            let item = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(item) => item,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ApiError::Network("stream ended before message_stop".into()));
                }
            };
            let data = item?;
            let idx = data["index"].as_u64().unwrap_or(0) as usize;
            match data["type"].as_str().unwrap_or("") {
                "message_start" => merge_usage(&mut msg.usage, &data["message"]["usage"]),
                "content_block_start" => {
                    let block = data["content_block"].clone();
                    let kind = block["type"].as_str().unwrap_or("").to_string();
                    if kind == "tool_use" {
                        partial.insert(idx, String::new());
                    }
                    msg.content.push(block);
                    on_event(Event::BlockStart(&kind));
                }
                "content_block_delta" => {
                    let Some(block) = msg.content.get_mut(idx) else {
                        continue;
                    };
                    let d = &data["delta"];
                    match d["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let t = d["text"].as_str().unwrap_or("");
                            append_str(block, "text", t);
                            on_event(Event::Text(t));
                        }
                        "thinking_delta" => {
                            let t = d["thinking"].as_str().unwrap_or("");
                            append_str(block, "thinking", t);
                            on_event(Event::Thinking(t));
                        }
                        "signature_delta" => {
                            append_str(block, "signature", d["signature"].as_str().unwrap_or(""))
                        }
                        "input_json_delta" => {
                            partial
                                .entry(idx)
                                .or_default()
                                .push_str(d["partial_json"].as_str().unwrap_or(""));
                        }
                        "citations_delta" => {
                            if !block["citations"].is_array() {
                                block["citations"] = json!([]);
                            }
                            if let Some(arr) = block["citations"].as_array_mut() {
                                arr.push(d["citation"].clone());
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if let Some(raw) = partial.remove(&idx) {
                        let input = if raw.trim().is_empty() {
                            Ok(json!({}))
                        } else {
                            serde_json::from_str(&raw)
                        };
                        match input {
                            Ok(v) => msg.content[idx]["input"] = v,
                            Err(_) => msg.invalid_tool_inputs.push(idx),
                        }
                    }
                    on_event(Event::BlockStop);
                }
                "message_delta" => {
                    if let Some(r) = data["delta"]["stop_reason"].as_str() {
                        msg.stop_reason = r.to_string();
                    }
                    msg.stop_details = data["delta"]["stop_details"].clone();
                    merge_usage(&mut msg.usage, &data["usage"]);
                }
                "message_stop" => {
                    // Blocks still open (cut off by max_tokens) keep their partial input marked invalid.
                    for (idx, raw) in partial.drain() {
                        if serde_json::from_str::<Value>(&raw).is_err() {
                            msg.invalid_tool_inputs.push(idx);
                        }
                    }
                    return Ok(msg);
                }
                "error" => {
                    return Err(ApiError::Stream(
                        data["error"]["message"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_string(),
                    ));
                }
                _ => {} // ping and future event types
            }
        }
    }

    /// Opens the stream (with retries) and parses SSE on a reader thread, so
    /// the caller can stay responsive to ctrl+c.
    fn open(
        &self,
        body: &Value,
        betas: &[&str],
        interrupted: &AtomicBool,
    ) -> Result<mpsc::Receiver<Result<Value, ApiError>>, ApiError> {
        let mut attempt = 0;
        let resp = loop {
            let req = self
                .http
                .post(format!("{}/v1/messages", self.base_url))
                .header(&self.auth.0, &self.auth.1)
                .header("anthropic-version", API_VERSION)
                .header("anthropic-beta", betas.join(","))
                .header("content-type", "application/json")
                .json(body);
            let retry_delay = |attempt: u32, retry_after: Option<u64>| {
                Duration::from_millis(
                    retry_after
                        .map(|s| s * 1000)
                        .unwrap_or(500 * 2u64.pow(attempt))
                        .min(30_000),
                )
            };
            match req.send() {
                Ok(r) if r.status().is_success() => break r,
                Ok(r) => {
                    let code = r.status().as_u16();
                    let retry_after = r
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                    let text = r.text().unwrap_or_default();
                    let message = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|v| v["error"]["message"].as_str().map(String::from))
                        .unwrap_or(text);
                    let retryable = code == 408 || code == 409 || code == 429 || code >= 500;
                    if retryable && attempt < MAX_RETRIES && !interrupted.load(Ordering::Relaxed) {
                        thread::sleep(retry_delay(attempt, retry_after));
                        attempt += 1;
                        continue;
                    }
                    return Err(ApiError::Status { code, message });
                }
                Err(e) => {
                    if attempt < MAX_RETRIES && !interrupted.load(Ordering::Relaxed) {
                        thread::sleep(retry_delay(attempt, None));
                        attempt += 1;
                        continue;
                    }
                    return Err(ApiError::Network(e.to_string()));
                }
            }
        };

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(resp);
            let mut data = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(e) => {
                        let _ = tx.send(Err(ApiError::Network(e.to_string())));
                        return;
                    }
                }
                let l = line.trim_end_matches(['\r', '\n']);
                if l.is_empty() {
                    if !data.is_empty() {
                        let parsed = serde_json::from_str::<Value>(&data)
                            .map_err(|e| ApiError::Stream(format!("bad event JSON: {e}")));
                        data.clear();
                        if tx.send(parsed).is_err() {
                            return; // receiver gone (interrupted)
                        }
                    }
                } else if let Some(d) = l.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(d.trim_start());
                }
            }
        });
        Ok(rx)
    }
}

fn append_str(block: &mut Value, key: &str, s: &str) {
    let cur = block[key].as_str().unwrap_or("").to_string();
    block[key] = Value::String(cur + s);
}

fn merge_usage(u: &mut Usage, v: &Value) {
    if let Some(n) = v["input_tokens"].as_u64() {
        u.input = n;
    }
    if let Some(n) = v["output_tokens"].as_u64() {
        u.output = n;
    }
    if let Some(n) = v["cache_read_input_tokens"].as_u64() {
        u.cache_read = n;
    }
    if let Some(n) = v["cache_creation_input_tokens"].as_u64() {
        u.cache_write = n;
    }
}
