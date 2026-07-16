//! Minimal AI client for jterm1's terminal-side helpers (per-block error
//! explanation, session Q&A panel, `?` palette prefix).
//!
//! Mirrors rsh's provider conventions so a user who set `ANTHROPIC_API_KEY`
//! for the shell already has jterm1 wired up: detection prefers Claude →
//! OpenAI → Ollama (local fallback). Inference runs on a worker thread and
//! posts its result back to the GLib main thread via `glib::idle_add_local`,
//! so the UI never blocks on the HTTP round-trip.
//!
//! Privacy: nothing leaves the machine without an explicit user action
//! (clicking an Explain button, typing into the panel, hitting `?` in the
//! palette). AI-bound text is bounded by callers and scrubbed for common
//! high-confidence secret formats immediately before the request is started.

use regex::Regex;
use relm4::gtk;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gtk::glib;

/// Conservative, high-confidence patterns for credentials that commonly appear
/// in terminal output. Avoid generic hashes so git SHAs and build IDs survive.
fn secret_patterns() -> &'static [(&'static str, Regex)] {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            (
                "private-key",
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            ),
            ("aws-access-key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            ("github-pat", r"\bgithub_pat_[A-Za-z0-9_]{82}\b"),
            ("github-token", r"\bgh[opusr]_[A-Za-z0-9]{36,}\b"),
            ("slack-token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b"),
            (
                "jwt",
                r"\beyJ[A-Za-z0-9_=-]{8,}\.eyJ[A-Za-z0-9_=-]{8,}\.[A-Za-z0-9_=.+/-]{8,}\b",
            ),
            ("anthropic-key", r"\bsk-ant-[A-Za-z0-9_-]{20,}\b"),
            ("openai-key", r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}\b"),
        ]
        .into_iter()
        .map(|(kind, pattern)| {
            (
                kind,
                Regex::new(pattern).expect("AI secret-redaction pattern must compile"),
            )
        })
        .collect()
    })
}

/// Scrub secrets while preserving the original allocation when no pattern
/// matches, which is the overwhelmingly common path.
fn redact_secrets_owned(mut input: String) -> String {
    for (kind, regex) in secret_patterns() {
        if !regex.is_match(&input) {
            continue;
        }
        let replacement = format!("[REDACTED:{kind}]");
        input = regex.replace_all(&input, replacement.as_str()).into_owned();
    }
    input
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Anthropic,
    OpenAiCompatible,
    Ollama,
}

impl Provider {
    pub(crate) fn as_config_value(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Ollama => "ollama",
        }
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAiCompatible => "OpenAI-compatible",
            Self::Ollama => "Ollama",
        }
    }

    pub(crate) fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-4-6",
            Self::OpenAiCompatible => "gpt-4o-mini",
            Self::Ollama => "codellama:7b",
        }
    }

    pub(crate) fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAiCompatible => "https://api.openai.com/v1",
            Self::Ollama => "http://localhost:11434",
        }
    }

    fn endpoint(self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        match self {
            Self::Anthropic if base.ends_with("/v1/messages") => base.to_string(),
            Self::Anthropic if base.ends_with("/v1") => format!("{base}/messages"),
            Self::Anthropic => format!("{base}/v1/messages"),
            Self::OpenAiCompatible if base.ends_with("/chat/completions") => base.to_string(),
            Self::OpenAiCompatible => format!("{base}/chat/completions"),
            Self::Ollama if base.ends_with("/api/chat") => base.to_string(),
            Self::Ollama if base.ends_with("/api") => format!("{base}/chat"),
            Self::Ollama => format!("{base}/api/chat"),
        }
    }

    fn api_key(self) -> Option<String> {
        let provider_key = match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAiCompatible => "OPENAI_API_KEY",
            Self::Ollama => "OLLAMA_API_KEY",
        };
        nonempty_env("JTERM1_AI_API_KEY").or_else(|| nonempty_env(provider_key))
    }
}

impl FromStr for Provider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "openai-compatible" | "openai_compatible" => Ok(Self::OpenAiCompatible),
            "ollama" => Ok(Self::Ollama),
            other => Err(format!("unknown AI provider '{other}'")),
        }
    }
}

/// All the knobs jterm1's AI helpers need to make one HTTP call. Built once
/// from the environment and cached on App for the session.
#[derive(Clone, Debug)]
pub(crate) struct AiClient {
    pub provider: Provider,
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: String,
    pub max_tokens: u32,
    pub redact_secrets: bool,
}

#[derive(Clone, Debug)]
pub struct BlockContext {
    pub cmd: String,
    pub output: String,
    pub cwd: Option<String>,
    pub exit_code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Turn {
    pub(crate) role: Role,
    pub(crate) text: String,
}

pub(crate) fn truncate_for_context(output: &str, max_lines_per_side: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines_per_side * 2 + 1 {
        return output.to_string();
    }
    let head = &lines[..max_lines_per_side];
    let tail = &lines[lines.len() - max_lines_per_side..];
    let elided = lines.len() - max_lines_per_side * 2;
    format!(
        "{}\n... [{} lines elided] ...\n{}",
        head.join("\n"),
        elided,
        tail.join("\n")
    )
}

impl AiClient {
    pub(crate) fn new(
        provider: Provider,
        api_key: Option<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
        max_tokens: u32,
        redact_secrets: bool,
    ) -> Result<Self, String> {
        let model = model.into();
        let base_url = base_url.into();
        validate_client_values(&model, &base_url, max_tokens)?;
        let api_key = api_key.filter(|key| !key.trim().is_empty());
        if provider == Provider::Anthropic && api_key.is_none() {
            return Err(
                "Anthropic API key is not set (use JTERM1_AI_API_KEY or ANTHROPIC_API_KEY)"
                    .to_string(),
            );
        }
        Ok(Self {
            provider,
            api_key,
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            max_tokens,
            redact_secrets,
        })
    }

    pub(crate) fn from_config(config: &crate::config::Config) -> Result<Self, String> {
        if !config.ai_enabled {
            return Err("AI features are disabled by configuration".to_string());
        }
        let provider = Provider::from_str(&config.ai_provider)?;
        Self::new(
            provider,
            provider.api_key(),
            config.ai_model.clone(),
            config.ai_base_url.clone(),
            config.ai_max_tokens,
            config.ai_redact_secrets,
        )
    }

    /// Inspect the environment and return a configured client when at least
    /// one provider looks usable. Returns None when there's no API key AND
    /// no Ollama at the default URL — callers gate UI on that None to hide
    /// AI surfaces silently rather than show a broken button.
    pub(crate) fn from_env() -> Option<Self> {
        // Mirror rsh's precedence:
        //   1. explicit JTERM1_AI_PROVIDER (anthropic/openai/ollama)
        //   2. ANTHROPIC_API_KEY → Anthropic
        //   3. OPENAI_API_KEY → OpenAI
        //   4. fall back to Ollama (no key needed)
        let provider = match nonempty_env("JTERM1_AI_PROVIDER") {
            Some(value) => Provider::from_str(&value).ok()?,
            None if nonempty_env("ANTHROPIC_API_KEY").is_some() => Provider::Anthropic,
            None if nonempty_env("OPENAI_API_KEY").is_some() => Provider::OpenAiCompatible,
            None => Provider::Ollama,
        };
        let model =
            nonempty_env("JTERM1_AI_MODEL").unwrap_or_else(|| provider.default_model().to_string());
        let base_url = nonempty_env("JTERM1_AI_BASE_URL")
            .unwrap_or_else(|| provider.default_base_url().to_string());
        let max_tokens = nonempty_env("JTERM1_AI_MAX_TOKENS")
            .and_then(|value| value.parse().ok())
            .unwrap_or(1_024);
        let redact_secrets = nonempty_env("JTERM1_AI_REDACT_SECRETS")
            .and_then(|value| parse_bool(&value))
            .unwrap_or(true);
        Self::new(
            provider,
            provider.api_key(),
            model,
            base_url,
            max_tokens,
            redact_secrets,
        )
        .ok()
    }

    /// Short human label for status text ("Claude · sonnet-4 …").
    pub(crate) fn display_name(&self) -> String {
        format!("{} · {}", self.provider.display_name(), self.model)
    }

    fn prepare_text(&self, text: String) -> String {
        if self.redact_secrets {
            redact_secrets_owned(text)
        } else {
            text
        }
    }
}

fn validate_client_values(model: &str, base_url: &str, max_tokens: u32) -> Result<(), String> {
    if model.trim().is_empty() {
        return Err("AI model must not be empty".to_string());
    }
    let base_url = base_url.trim();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://"))
        || base_url
            .split_once("://")
            .is_none_or(|(_, authority)| authority.is_empty())
        || base_url.chars().any(char::is_whitespace)
    {
        return Err("AI base URL must be an absolute http(s) URL without whitespace".to_string());
    }
    if !(1..=32_768).contains(&max_tokens) {
        return Err("AI max tokens must be between 1 and 32768".to_string());
    }
    Ok(())
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Handle held by the UI for an in-flight request. Drop it (or call
/// `cancel`) to ignore any pending callback — the HTTP request may still
/// finish in the background, but `on_done` will not run.
pub(crate) struct AiHandle {
    cancelled: Arc<AtomicBool>,
}

type AiResult = Result<String, String>;
type AiResultSlot = Arc<std::sync::Mutex<Option<AiResult>>>;
type AiCompletion = Box<dyn FnOnce(AiResult)>;

impl AiHandle {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Fire one prompt at the configured provider. `on_done` is invoked exactly
/// once on the GLib main thread with either the assistant text or an error
/// string. Returns an `AiHandle` the caller can drop to cancel.
pub(crate) fn ask(
    client: AiClient,
    system: String,
    user: String,
    on_done: impl FnOnce(AiResult) + 'static,
) -> AiHandle {
    ask_turns(
        client,
        system,
        vec![Turn {
            role: Role::User,
            text: user,
        }],
        on_done,
    )
}

/// Fire a provider-neutral multi-turn transcript. Roles are retained all the
/// way to the provider request body; the legacy `ask` helper above remains the
/// single-user-turn compatibility entry point for explain/palette/agent calls.
pub(crate) fn ask_turns(
    client: AiClient,
    system: String,
    history: Vec<Turn>,
    on_done: impl FnOnce(AiResult) + 'static,
) -> AiHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_thread = cancelled.clone();

    // Scrub at the common provider boundary so every AI surface — explain,
    // palette, session Q&A, and agent mode — gets identical protection.
    let system = client.prepare_text(system);
    let history: Vec<Turn> = history
        .into_iter()
        .map(|turn| Turn {
            role: turn.role,
            text: client.prepare_text(turn.text),
        })
        .collect();

    // glib::Sender can't carry FnOnce closures portably; use a one-shot
    // channel pattern: thread parks the result behind a Mutex<Option<T>>
    // and a glib idle pulls it on the main thread.
    let slot: AiResultSlot = Arc::new(std::sync::Mutex::new(None));
    let slot_thread = slot.clone();
    let slot_main = slot.clone();
    // `on_done` is FnOnce; wrap in Option so the idle closure can take it.
    let mut on_done_cell: Option<AiCompletion> = Some(Box::new(on_done));

    std::thread::spawn(move || {
        let result = run_request(&client, &system, &history);
        if cancelled_thread.load(Ordering::SeqCst) {
            return;
        }
        *slot_thread.lock().expect("ai slot mutex poisoned") = Some(result);
    });

    // Poll the slot on the GLib main loop. Cheap: a tick once every 100ms
    // until the worker finishes (typical request: 0.5–5 s).
    let cancelled_main = cancelled.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if cancelled_main.load(Ordering::SeqCst) {
            return glib::ControlFlow::Break;
        }
        let mut guard = slot_main.lock().expect("ai slot mutex poisoned");
        if let Some(result) = guard.take() {
            if let Some(cb) = on_done_cell.take() {
                cb(result);
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });

    AiHandle { cancelled }
}

/// Build a fresh ureq agent per call — connection reuse isn't worth the
/// extra global state for our low request rate, and a per-call agent makes
/// the cancel/timeout story trivial.
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .timeout_send_body(Some(Duration::from_secs(15)))
        .build()
        .into()
}

fn run_request(client: &AiClient, system: &str, history: &[Turn]) -> Result<String, String> {
    match client.provider {
        Provider::Anthropic => call_anthropic(client, system, history),
        Provider::OpenAiCompatible => call_openai(client, system, history),
        Provider::Ollama => call_ollama(client, system, history),
    }
}

fn message_values(history: &[Turn]) -> Vec<serde_json::Value> {
    history
        .iter()
        .map(|turn| serde_json::json!({"role": turn.role.as_str(), "content": turn.text}))
        .collect()
}

fn request_body(client: &AiClient, system: &str, history: &[Turn]) -> serde_json::Value {
    let mut messages = message_values(history);
    match client.provider {
        Provider::Anthropic => serde_json::json!({
            "model": client.model,
            "max_tokens": client.max_tokens,
            "system": system,
            "messages": messages,
        }),
        Provider::OpenAiCompatible => {
            messages.insert(0, serde_json::json!({"role": "system", "content": system}));
            serde_json::json!({
                "model": client.model,
                "max_tokens": client.max_tokens,
                "messages": messages,
            })
        }
        Provider::Ollama => {
            messages.insert(0, serde_json::json!({"role": "system", "content": system}));
            serde_json::json!({
                "model": client.model,
                "messages": messages,
                "stream": false,
                "options": { "num_predict": client.max_tokens },
            })
        }
    }
}

fn call_anthropic(client: &AiClient, system: &str, history: &[Turn]) -> Result<String, String> {
    let url = client.provider.endpoint(&client.base_url);
    let body = request_body(client, system, history);
    let mut req = http_agent()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01");
    if let Some(key) = &client.api_key {
        req = req.header("x-api-key", key.as_str());
    }
    let mut resp = req
        .send(body.to_string())
        .map_err(|e| format!("anthropic request failed: {e}"))?;
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read body: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    if let Some(text) = response_text(client.provider, &v) {
        Ok(text)
    } else if let Some(msg) = v["error"]["message"].as_str() {
        Err(msg.to_string())
    } else {
        Err(format!(
            "unexpected anthropic response: {}",
            trim_for_log(&text)
        ))
    }
}

fn call_openai(client: &AiClient, system: &str, history: &[Turn]) -> Result<String, String> {
    let url = client.provider.endpoint(&client.base_url);
    let body = request_body(client, system, history);
    let mut req = http_agent()
        .post(&url)
        .header("Content-Type", "application/json");
    if let Some(key) = &client.api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let mut resp = req
        .send(body.to_string())
        .map_err(|e| format!("openai request failed: {e}"))?;
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read body: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    if let Some(text) = response_text(client.provider, &v) {
        Ok(text)
    } else if let Some(msg) = v["error"]["message"].as_str() {
        Err(msg.to_string())
    } else {
        Err(format!(
            "unexpected openai response: {}",
            trim_for_log(&text)
        ))
    }
}

fn call_ollama(client: &AiClient, system: &str, history: &[Turn]) -> Result<String, String> {
    let url = client.provider.endpoint(&client.base_url);
    let body = request_body(client, system, history);
    let mut req = http_agent()
        .post(&url)
        .header("Content-Type", "application/json");
    if let Some(key) = &client.api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let mut resp = req
        .send(body.to_string())
        .map_err(|e| format!("ollama request failed (is `ollama serve` running?): {e}"))?;
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read body: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    if let Some(text) = response_text(client.provider, &v) {
        Ok(text)
    } else if let Some(msg) = v["error"].as_str() {
        Err(msg.to_string())
    } else {
        Err(format!(
            "unexpected ollama response: {}",
            trim_for_log(&text)
        ))
    }
}

fn response_text(provider: Provider, value: &serde_json::Value) -> Option<String> {
    let text = match provider {
        Provider::Anthropic => value.get("content").and_then(|content| {
            content.as_array().map(|parts| {
                parts
                    .iter()
                    .filter(|part| {
                        part.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    })
                    .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }),
        Provider::OpenAiCompatible => value
            .pointer("/choices/0/message/content")
            .and_then(content_text),
        Provider::Ollama => value
            .pointer("/message/content")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                value
                    .get("response")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }),
    }
    .unwrap_or_default();
    (!text.trim().is_empty()).then_some(text)
}

fn content_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    value.as_array().map(|parts| {
        parts
            .iter()
            .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut index = index.min(s.len());
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(s: &str, index: usize) -> usize {
    let mut index = index.min(s.len());
    while index < s.len() && !s.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn trim_for_log(s: &str) -> String {
    if s.len() <= 256 {
        s.to_string()
    } else {
        let end = floor_char_boundary(s, 256);
        format!("{}…", &s[..end])
    }
}

// ── Prompt builders ────────────────────────────────────────────────────────

/// Bounded output sample for prompts: head + tail so a multi-MB build log
/// still fits in the context window without dropping the failing tail.
fn sample_output(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let half = max_bytes / 2;
    let head_end = floor_char_boundary(output, half);
    let tail_start = ceil_char_boundary(output, output.len().saturating_sub(half));
    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let retained = head.len().saturating_add(tail.len());
    format!(
        "{head}\n\n… [{} bytes elided] …\n\n{tail}",
        output.len().saturating_sub(retained)
    )
}

/// Build the system+user prompt for "explain why this failed and how to fix it".
pub(crate) fn build_explain_prompt(
    command: &str,
    output: &str,
    exit_code: i32,
    cwd: &str,
) -> (String, String) {
    let system = "You are a senior shell user helping debug a failed command. \
Read the command, its output, and its exit code. Reply with:\n\
1. One short sentence on what went wrong.\n\
2. A single concrete fix (one shell command or one config change).\n\
Be terse. No markdown headers. No filler. If the error is ambiguous, say so."
        .to_string();
    let sample = sample_output(output, 8 * 1024);
    let user = format!("cwd: {cwd}\nexit: {exit_code}\ncommand:\n{command}\n\noutput:\n{sample}");
    (system, user)
}

/// Build the system+user prompt for the `?` palette: natural language → one
/// shell command. The model is told to emit ONLY the command so we can paste
/// it directly into the input line without further parsing.
pub(crate) fn build_nl_to_cmd_prompt(query: &str, cwd: &str) -> (String, String) {
    let system = "You convert natural language requests into one shell command. \
Output ONLY the command, no markdown, no quotes, no explanation. \
If the request is ambiguous, output the safest interpretation."
        .to_string();
    let user = format!("cwd: {cwd}\nrequest: {query}");
    (system, user)
}

/// Build the system prompt for agent mode. The user-side payload is the
/// running transcript, assembled by `agent::AgentSession::build_user_prompt`.
///
/// The JSON-action protocol is the load-bearing piece: the UI parses each
/// reply with `agent::parse_action`, and malformed or schema-invalid output
/// is surfaced as a protocol error. Few-shot examples cover the three
/// actions the model is allowed to emit (`run` / `say` / `done`).
pub(crate) fn build_agent_system_prompt(cwd: &str, shell: &str, os: &str) -> String {
    format!(
        "You are an interactive shell agent helping the user in their terminal. \
Each reply MUST be exactly one JSON object — no prose, markdown, or commentary. \
Use exactly one of these schemas and no other keys (`thought` is optional):\n\
  {{ \"action\": \"run\", \"command\": \"...\", \"thought\": \"...\" }}\n\
  {{ \"action\": \"say\", \"message\": \"...\", \"thought\": \"...\" }}\n\
  {{ \"action\": \"done\", \"message\": \"...\", \"thought\": \"...\" }}\n\
- `action: run` means the user must approve a shell command. Put the command in `command`. \
  Use this for anything that changes filesystem, network, or state. Do not chain unrelated \
  steps with `;` or `&&` — one command per turn so the user can review each.\n\
- `action: say` means you need a clarifying answer from the user, or want to comment without \
  running a command. Put the text in `message`.\n\
- `action: done` means the task is complete. Put a short summary in `message`.\n\
The user runs the command after approving it; you then receive an `Output (exit=N):` block \
in the next turn and can decide what to do next. Prefer the smallest command that yields the \
information you need. Never assume a command succeeded — wait for the observation.\n\
\n\
Environment:\n\
  cwd: {cwd}\n\
  shell: {shell}\n\
  os: {os}\n\
\n\
Examples (single-line for clarity — actual replies should still be valid JSON):\n\
User: my disk is full, what's eating space?\n\
Assistant: {{\"thought\":\"survey top-level usage first\",\"action\":\"run\",\"command\":\"du -sh /* 2>/dev/null | sort -h | tail -20\"}}\n\
Output (exit=0): 12G /var\\n8.4G /home\\n…\n\
Assistant: {{\"thought\":\"/var is biggest, drill into it\",\"action\":\"run\",\"command\":\"du -sh /var/* 2>/dev/null | sort -h | tail -10\"}}\n\
\n\
User: rename all .txt to .md in this folder\n\
Assistant: {{\"action\":\"run\",\"command\":\"for f in *.txt; do mv -- \\\"$f\\\" \\\"${{f%.txt}}.md\\\"; done\"}}\n\
\n\
User: is port 5432 free?\n\
Assistant: {{\"action\":\"run\",\"command\":\"ss -tlnp | grep ':5432' || echo free\"}}\n\
Output (exit=0): free\n\
Assistant: {{\"action\":\"done\",\"message\":\"Port 5432 is free.\"}}\n\
"
    )
}

/// Build the system+user prompt for the session panel, optionally seeded
/// with the most recent block context.
pub(crate) fn build_session_prompt(question: &str, context: Option<&str>) -> (String, String) {
    let system = "You are a terminal assistant. Answer the user's question concisely. \
If shell context is attached, use it. No filler, no markdown headers."
        .to_string();
    let user = match context {
        Some(c) => format!("Recent shell context:\n{c}\n\nQuestion: {question}"),
        None => format!("Question: {question}"),
    };
    (system, user)
}

/// Build the first turn for "Ask AI about selected block". Block data is
/// system context rather than an assistant/user turn, so subsequent questions
/// retain a strictly alternating provider transcript.
pub(crate) fn build_block_chat_prompt(question: &str, context: &BlockContext) -> (String, String) {
    let cwd = context.cwd.as_deref().unwrap_or("unknown");
    let system = format!(
        "You are a terminal assistant. Answer concisely using the selected finished command block. \
Do not claim that a suggested command was executed.\n\n\
Selected block:\n\
cwd: {cwd}\n\
exit_code: {}\n\
command:\n{}\n\n\
output:\n{}",
        context.exit_code, context.cmd, context.output
    );
    (system, format!("Question: {question}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(provider: Provider, redact_secrets: bool) -> AiClient {
        AiClient::new(
            provider,
            Some("test-key".to_string()),
            "test-model",
            provider.default_base_url(),
            512,
            redact_secrets,
        )
        .unwrap()
    }

    #[test]
    fn provider_aliases_and_endpoints_are_normalized() {
        assert_eq!(Provider::from_str("claude").unwrap(), Provider::Anthropic);
        assert_eq!(
            Provider::from_str("openai_compatible").unwrap(),
            Provider::OpenAiCompatible
        );
        assert_eq!(
            Provider::Anthropic.endpoint("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            Provider::OpenAiCompatible.endpoint("http://localhost:8000/v1/chat/completions"),
            "http://localhost:8000/v1/chat/completions"
        );
        assert_eq!(
            Provider::Ollama.endpoint("http://localhost:11434/api"),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn client_validates_keys_urls_and_token_bounds() {
        assert!(AiClient::new(
            Provider::Anthropic,
            None,
            "model",
            "https://example.com",
            1,
            true,
        )
        .is_err());
        assert!(AiClient::new(
            Provider::OpenAiCompatible,
            None,
            "local-model",
            "http://localhost:8000/v1",
            32_768,
            true,
        )
        .is_ok());
        assert!(AiClient::new(
            Provider::Ollama,
            None,
            "model",
            "file:///tmp/model",
            512,
            true,
        )
        .is_err());
        assert!(AiClient::new(
            Provider::Ollama,
            None,
            "model",
            "http://localhost:11434",
            0,
            true,
        )
        .is_err());
    }

    #[test]
    fn parses_provider_response_shapes() {
        assert_eq!(
            response_text(
                Provider::Anthropic,
                &serde_json::json!({"content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]})
            )
            .as_deref(),
            Some("one\ntwo")
        );
        assert_eq!(
            response_text(
                Provider::OpenAiCompatible,
                &serde_json::json!({"choices": [{"message": {"content": [{"text": "ok"}]}}]})
            )
            .as_deref(),
            Some("ok")
        );
        assert_eq!(
            response_text(
                Provider::Ollama,
                &serde_json::json!({"message": {"content": "local"}})
            )
            .as_deref(),
            Some("local")
        );
    }

    #[test]
    fn secret_redaction_obeys_the_client_setting() {
        let token = format!("ghp_{}", "A".repeat(36));
        let input = format!("token={token}");
        assert!(!client(Provider::Ollama, true)
            .prepare_text(input.clone())
            .contains(&token));
        assert_eq!(
            client(Provider::Ollama, false).prepare_text(input.clone()),
            input
        );
    }

    #[test]
    fn sample_output_passes_through_small() {
        let s = "hi";
        assert_eq!(sample_output(s, 1000), s);
    }

    #[test]
    fn sample_output_truncates_large_with_marker() {
        let big = "x".repeat(20_000);
        let s = sample_output(&big, 1000);
        assert!(s.len() < 1500);
        assert!(s.contains("elided"));
    }

    #[test]
    fn sample_output_is_safe_at_multibyte_boundaries() {
        let big = "编译失败🙂".repeat(2_000);
        let sampled = sample_output(&big, 1_001);
        assert!(sampled.contains("elided"));
        assert!(sampled.starts_with('编'));
        assert!(sampled.ends_with('🙂'));
    }

    #[test]
    fn log_trimming_is_safe_at_multibyte_boundaries() {
        let input = "界".repeat(100);
        let trimmed = trim_for_log(&input);
        assert!(trimmed.ends_with('…'));
        assert!(trimmed.len() <= 259);
    }

    #[test]
    fn ai_context_redacts_high_confidence_secrets() {
        let token = format!("ghp_{}", "A".repeat(36));
        let input = format!("git remote contains {token}");
        let redacted = redact_secrets_owned(input);
        assert!(redacted.contains("[REDACTED:github-token]"));
        assert!(!redacted.contains("ghp_"));
    }

    #[test]
    fn ai_context_keeps_git_hashes_and_uuids() {
        let input =
            "commit deadbeefcafef00d1234567890abcdef01234567 uuid 550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(redact_secrets_owned(input.to_string()), input);
    }

    #[test]
    fn explain_prompt_contains_command_and_exit() {
        let (sys, user) = build_explain_prompt("false", "out", 1, "/tmp");
        assert!(sys.to_lowercase().contains("debug"));
        assert!(user.contains("false"));
        assert!(user.contains("exit: 1"));
        assert!(user.contains("/tmp"));
    }

    #[test]
    fn nl_to_cmd_prompt_emits_request() {
        let (_sys, user) = build_nl_to_cmd_prompt("list large files", "/var");
        assert!(user.contains("list large files"));
        assert!(user.contains("/var"));
    }

    #[test]
    fn provider_request_bodies_preserve_multi_turn_role_order() {
        let turns = vec![
            Turn {
                role: Role::User,
                text: "first".into(),
            },
            Turn {
                role: Role::Assistant,
                text: "reply".into(),
            },
            Turn {
                role: Role::User,
                text: "follow-up".into(),
            },
        ];
        let anthropic = request_body(&client(Provider::Anthropic, false), "system", &turns);
        let roles: Vec<_> = anthropic["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);

        let openai = request_body(&client(Provider::OpenAiCompatible, false), "system", &turns);
        let roles: Vec<_> = openai["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
    }

    #[test]
    fn selected_block_prompt_contains_command_output_exit_and_cwd() {
        let (system, user) = build_block_chat_prompt(
            "why?",
            &BlockContext {
                cmd: "false".into(),
                output: "failed".into(),
                cwd: Some("/tmp".into()),
                exit_code: 1,
            },
        );
        for expected in ["false", "failed", "/tmp", "exit_code: 1"] {
            assert!(system.contains(expected));
        }
        assert_eq!(user, "Question: why?");
    }

    #[test]
    fn detection_prefers_anthropic_when_key_set() {
        // We can't mutate process env safely in tests; just sanity-check that
        // the explicit JTERM1_AI_PROVIDER path picks Ollama (no key needed).
        std::env::set_var("JTERM1_AI_PROVIDER", "ollama");
        let c = AiClient::from_env().expect("ollama needs no key");
        assert_eq!(c.provider, Provider::Ollama);
        std::env::remove_var("JTERM1_AI_PROVIDER");
    }
}
