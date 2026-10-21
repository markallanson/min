//! min-llm — the LLM client plugin.
//!
//! Registers an [`Llm`] service that streams one completion at a time and
//! normalizes everything to Anthropic-style content blocks:
//!
//! ```json
//! { "stop_reason": "end_turn" | "tool_use" | ...,
//!   "content": [ {"type":"text","text":...},
//!                {"type":"tool_use","id":...,"name":...,"input":{...}} ] }
//! ```
//!
//! so the rest of min never has to care which wire format is in use.
//!
//! Failures are classified: `429` with a balance/resource-package error is
//! an endpoint/plan mismatch, while genuine rate limits and 5xx/network
//! errors are retried with exponential backoff (1→32s, honouring
//! `Retry-After`), reporting progress via a status callback.
//!
//! The z.ai coding plan is served from two regional portals (`api.z.ai` and
//! `open.bigmodel.cn`); a key provisioned on one gets "no resource package"
//! (a 429) on the other. On that error min automatically retries the mirror
//! portal for the rest of the session.
//!
//! Environment:
//! - `MIN_API`            dialect or endpoint alias: `"anthropic"` (default),
//!                        `"openai"`, `"zai"` (Z.ai coding plan), `"openrouter"`,
//!                        `"vertex"` (Gemini on Vertex AI, Google ADC auth),
//!                        `"ollama"` (native `/api/chat`, no key needed)
//! - `MIN_BASE_URL`       overrides the endpoint an alias picks (default
//!                        `https://api.anthropic.com`)
//! - `MIN_MODEL`          overrides the model an alias picks
//!                        (default `claude-sonnet-4-5`)
//! - `MIN_MAX_TOKENS`     default `16384`
//! - `ANTHROPIC_API_KEY`  key for the anthropic dialect
//! - `MIN_API_KEY` / `OPENAI_API_KEY` / `ZAI_API_KEY`  keys for openai-style endpoints
//! - `GOOGLE_CLOUD_PROJECT` / `GOOGLE_CLOUD_LOCATION`  Vertex project and
//!                        region (default `global`); credentials come from
//!                        Application Default Credentials (see [`google`])
//! - `OLLAMA_HOST`        Ollama server (default `http://localhost:11434`)
//! - `OLLAMA_API_KEY`     optional bearer token for a remote Ollama
//! - `MIN_NUM_CTX`        Ollama context window (default `32768`)
//! - `HTTPS_PROXY`        optional outbound proxy

use min_core::{events, Hub, Plugin};
use min_core::usage::TokenUsage;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

mod google;

use min_core::llm::{ChatProvider, LlmRouter};

pub struct RouterPlugin;

impl Plugin for RouterPlugin {
    fn name(&self) -> &'static str { "llm-router" }
    fn start(&mut self, hub: &Hub) {
        hub.set_service(LlmRouter::default());
    }
}

pub struct AnthropicPlugin;
impl Plugin for AnthropicPlugin {
    fn name(&self) -> &'static str { "llm-anthropic" }
    fn start(&mut self, hub: &Hub) {
        if std::env::var("MIN_API").unwrap_or_default() == "anthropic" || std::env::var("MIN_API").is_err() {
            if let Some(router) = hub.service::<LlmRouter>() {
                let llm = Llm::from_env();
                router.register(Box::new(llm));
            }
        }
    }
}

pub struct OpenAiPlugin;
impl Plugin for OpenAiPlugin {
    fn name(&self) -> &'static str { "llm-openai" }
    fn start(&mut self, hub: &Hub) {
        if std::env::var("MIN_API").unwrap_or_default() == "openai" {
            if let Some(router) = hub.service::<LlmRouter>() {
                let llm = Llm::from_env();
                router.register(Box::new(llm));
            }
        }
    }
}

pub struct VertexPlugin;
impl Plugin for VertexPlugin {
    fn name(&self) -> &'static str { "llm-vertex" }
    fn start(&mut self, hub: &Hub) {
        if std::env::var("MIN_API").unwrap_or_default() == "vertex" {
            if let Some(router) = hub.service::<LlmRouter>() {
                let llm = Llm::from_env();
                router.register(Box::new(llm));
            }
        }
    }
}

pub struct ZaiPlugin;
impl Plugin for ZaiPlugin {
    fn name(&self) -> &'static str { "llm-zai" }
    fn start(&mut self, hub: &Hub) {
        if std::env::var("MIN_API").unwrap_or_default() == "zai" {
            if let Some(router) = hub.service::<LlmRouter>() {
                let llm = Llm::from_env();
                router.register(Box::new(llm));
            }
        }
    }
}

pub struct OllamaPlugin;
impl Plugin for OllamaPlugin {
    fn name(&self) -> &'static str { "llm-ollama" }
    fn start(&mut self, hub: &Hub) {
        if std::env::var("MIN_API").unwrap_or_default() == "ollama" {
            if let Some(router) = hub.service::<LlmRouter>() {
                let llm = Llm::from_env();
                router.register(Box::new(llm));
            }
        }
    }
}

impl ChatProvider for Llm {
    fn can_handle(&self, _model: &str) -> bool {
        true
    }

    fn complete(
        &self,
        _model: &str,
        system: &str,
        messages_json: &str,
        tools_json: &str,
        on_delta: &mut dyn FnMut(&str),
        on_thinking: &mut dyn FnMut(&str, &str),
        on_status: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        let messages: Vec<Value> = serde_json::from_str(messages_json).unwrap_or_default();
        let tools: Vec<Value> = serde_json::from_str(tools_json).unwrap_or_default();
        
        let result = self.complete_inner(system, &messages, &tools, on_delta, on_thinking, on_status)?;
        Ok(serde_json::to_string(&result).unwrap_or_default())
    }
}



/// A streaming chat client for exactly one configured endpoint.
pub struct Llm {
    http: ureq::Agent,
    api: &'static str,
    base: RefCell<String>,
    /// The other regional portal for the z.ai coding plan, if `base` is one.
    mirror: Option<&'static str>,
    /// Set once the mirror has been tried (successfully or not).
    mirrored: Cell<bool>,
    auth: Auth,
    pub model: String,
    max_tokens: u64,
    /// Ollama's `options.num_ctx` — its server default silently truncates long prompts.
    num_ctx: u64,
}

impl Llm {
    pub fn from_env() -> Self {
        let mut builder = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout(Duration::from_secs(600));
        if let Ok(p) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")) {
            if let Ok(proxy) = ureq::Proxy::new(&p) {
                builder = builder.proxy(proxy);
            }
        }
        let spec = std::env::var("MIN_API").unwrap_or_default().trim().to_lowercase();
        let (api, base_default, model_default) = resolve_api(&spec);
        let mut key_names: Vec<&str> = match api {
            "openai" => vec!["MIN_API_KEY", "OPENAI_API_KEY"],
            "ollama" => vec!["MIN_API_KEY", "OLLAMA_API_KEY"],
            _ => vec!["ANTHROPIC_API_KEY"],
        };
        if spec == "zai" {
            key_names.push("ZAI_API_KEY");
        }
        let mut auth = Auth::Key(first_env(&key_names));
        let mut base_default = base_default.to_string();
        if api == "vertex" {
            (auth, base_default) = vertex_setup();
        }
        if api == "ollama" {
            if matches!(&auth, Auth::Key(k) if k.is_empty()) {
                auth = Auth::None;
            }
            base_default = ollama_base(&std::env::var("OLLAMA_HOST").unwrap_or_default());
        }
        let base = std::env::var("MIN_BASE_URL").unwrap_or_default();
        let base = {
            let b = base.trim_end_matches('/');
            if b.is_empty() { base_default } else { b.to_string() }
        };
        let mirror = mirror_base(&base);
        let model = std::env::var("MIN_MODEL").unwrap_or_default();
        let model = if model.is_empty() { model_default.to_string() } else { model };
        Llm {
            http: builder.build(),
            api,
            base: RefCell::new(base),
            mirror,
            mirrored: Cell::new(false),
            auth,
            model,
            max_tokens: std::env::var("MIN_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16384),
            num_ctx: std::env::var("MIN_NUM_CTX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(32768),
        }
    }

    /// Stream one completion. `messages` are Anthropic-style; `tools` are
    /// `[{"name","description","input_schema"}]`; `on_delta` receives text as
    /// it arrives; `on_thinking` receives `(reasoning_text, tool_name)` when
    /// the model streams non-text work (reasoning tokens or tool-call JSON);
    /// `on_status` receives human-readable retry progress.
    /// Returns the normalized message (see crate docs).
    pub fn complete_inner(
        &self,
        system: &str,
        messages: &[Value],
        tools: &[Value],
        on_delta: &mut dyn FnMut(&str),
        on_thinking: &mut dyn FnMut(&str, &str),
        on_status: &mut dyn FnMut(&str),
    ) -> Result<Value, String> {
        if min_core::is_cancelled() {
            return Err("cancelled".into());
        }
        if let Some(problem) = self.auth.problem() {
            return Err(problem);
        }
        let body = self.build_body(system, messages, tools);
        const SCHEDULE: [u64; 6] = [1, 2, 4, 8, 16, 32];
        let attempts = SCHEDULE.len();
        for attempt in 1..=attempts + 1 {
            if min_core::is_cancelled() {
                return Err("cancelled".into());
            }
            let mut streamed = false;
            let mut delta = |t: &str| {
                streamed = true;
                on_delta(t);
            };
            let fail = match self.try_once(&body, &mut delta, on_thinking) {
                Ok(v) => return Ok(v),
                Err(f) => f,
            };
            if min_core::is_cancelled() {
                return Err("cancelled".into());
            }
            // A plan-mismatch 429 is not a rate limit — retrying won't help,
            // but the key may simply be provisioned on the mirror portal.
            if let Fail::Http { status: 429, body, .. } = &fail {
                if is_plan_mismatch(body) {
                    if !self.mirrored.get() {
                        if let Some(alt) = self.mirror {
                            let from = self.base();
                            self.mirrored.set(true);
                            *self.base.borrow_mut() = alt.to_string();
                            on_status(&format!(
                                "key has no resource package on {from} — switching to {alt} for this session"
                            ));
                            continue;
                        }
                    }
                    return Err(format!(
                        "HTTP 429: {} — the key ({}) has no coding-plan resource package on {}.\nhint: {PLAN_HINT}",
                        body.trim(),
                        self.key_hint(),
                        self.base()
                    ));
                }
            }
            // Never retry once text has reached the user — a retry would duplicate it.
            if streamed || !fail.is_retryable() {
                return Err(fail.message());
            }
            if attempt > attempts {
                return Err(format!(
                    "{} — gave up after {attempts} retries (~{}s total)",
                    fail.message(),
                    SCHEDULE.iter().sum::<u64>()
                ));
            }
            let wait = match &fail {
                Fail::Http { retry_after: Some(ra), .. } => *ra,
                _ => SCHEDULE[attempt - 1],
            };
            on_status(&format!(
                "{} — retrying in {wait}s (attempt {attempt}/{attempts})",
                fail.brief()
            ));
            let deadline = std::time::Instant::now() + Duration::from_secs(wait);
            while std::time::Instant::now() < deadline {
                if min_core::is_cancelled() {
                    return Err("cancelled".into());
                }
                std::thread::sleep(
                    Duration::from_millis(50).min(deadline.saturating_duration_since(std::time::Instant::now())),
                );
            }
        }
        unreachable!()
    }

    fn build_body(&self, system: &str, messages: &[Value], tools: &[Value]) -> Value {
        match self.api {
            "openai" | "vertex" => {
                let mut wire: Vec<Value> = vec![json!({"role": "system", "content": system})];
                for m in messages {
                    wire.extend(to_openai_messages(m));
                }
                let fns: Vec<Value> = tools.iter().map(to_function).collect();
                let mut body = json!({"model": self.model, "max_tokens": self.max_tokens, "messages": wire, "stream": true});
                // The OpenAI family only reports usage when the request asks
                // for it; the count then arrives in a trailing chunk.
                body["stream_options"] = json!({"include_usage": true});
                if !fns.is_empty() {
                    body["tools"] = Value::Array(fns);
                }
                if self.api == "vertex" {
                    let mut thinking = json!({"include_thoughts": true});
                    if !self.model.contains("gemini-2.") {
                        thinking["thinking_level"] = json!("HIGH");
                    }
                    body["extra_body"] = json!({"google": {"thinking_config": thinking}});
                }
                body
            }
            "ollama" => {
                let mut wire: Vec<Value> = vec![json!({"role": "system", "content": system})];
                let mut names = std::collections::HashMap::new();
                for m in messages {
                    wire.extend(to_ollama_messages(m, &mut names));
                }
                let mut body = json!({
                    "model": self.model, "messages": wire, "stream": true,
                    "options": {"num_ctx": self.num_ctx, "num_predict": self.max_tokens},
                });
                if !tools.is_empty() {
                    body["tools"] = Value::Array(tools.iter().map(to_function).collect());
                }
                body
            }
            _ => json!({
                "model": self.model, "max_tokens": self.max_tokens,
                "system": system, "messages": messages, "tools": tools, "stream": true,
            }),
        }
    }

    /// The currently active endpoint (may have switched to the mirror portal).
    pub fn base(&self) -> String {
        self.base.borrow().clone()
    }

    /// First/last few chars of the key, so errors show *which* key was used.
    fn key_hint(&self) -> String {
        match &self.auth {
            Auth::Key(k) if k.len() > 12 && k.is_ascii() => format!("{}…{}", &k[..6], &k[k.len() - 4..]),
            _ => "redacted".into(),
        }
    }

    /// Whether this endpoint reports usage in a chunk that arrives after
    /// the one carrying the finish reason (the OpenAI family does). The
    /// stream must be read to its end for the count to be seen.
    fn usage_after_finish(&self) -> bool {
        matches!(self.api, "openai" | "vertex")
    }

    fn try_once(
        &self,
        body: &Value,
        on_delta: &mut dyn FnMut(&str),
        on_thinking: &mut dyn FnMut(&str, &str),
    ) -> Result<Value, Fail> {
        let base = self.base();
        let url = match self.api {
            "openai" | "vertex" => format!("{base}/chat/completions"),
            "ollama" => format!("{base}/api/chat"),
            _ => format!("{base}/v1/messages"),
        };
        let mut req = self.http.post(&url);
        req = match (&self.auth, self.api) {
            (Auth::Google(adc), _) => {
                let token = adc.token(&self.http).map_err(Fail::Auth)?;
                let req = req.set("Authorization", &format!("Bearer {token}"));
                match &adc.quota_project {
                    Some(p) => req.set("x-goog-user-project", p),
                    None => req,
                }
            }
            (Auth::Key(k), "openai" | "ollama") => req.set("Authorization", &format!("Bearer {k}")),
            (Auth::None, _) => req,
            (Auth::Key(k), _) => req.set("x-api-key", k).set("anthropic-version", "2023-06-01"),
            (Auth::Unusable(e), _) => return Err(Fail::Auth(e.clone())),
        };
        let resp = match req.send_json(body.clone()) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let retry_after = r.header("retry-after").and_then(|v| v.parse().ok());
                return Err(Fail::Http { status: code, body: error_body(r), retry_after });
            }
            Err(e) => {
                if min_core::is_cancelled() {
                    return Err(Fail::Stream("cancelled".into()));
                }
                return Err(Fail::Transport(e.to_string()));
            }
        };
        let mut blocks = Blocks::default();
        let cancel_reader = CancelReader::new(resp.into_reader());
        for line in BufReader::new(cancel_reader).lines() {
            let line = line.map_err(|e| Fail::Stream(format!("stream failed: {e}")))?;
            // Ollama streams bare NDJSON; the others use SSE `data:` lines.
            let data = match (self.api, line.strip_prefix("data:")) {
                ("ollama", _) => line.as_str(),
                (_, Some(d)) => d,
                (_, None) => continue,
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            match self.api {
                "openai" | "vertex" => parse_openai_chunk(&v, &mut blocks, on_delta, on_thinking).map_err(Fail::Stream)?,
                "ollama" => parse_ollama_chunk(&v, &mut blocks, on_delta, on_thinking).map_err(Fail::Stream)?,
                _ => parse_anthropic_chunk(&v, &mut blocks, on_delta, on_thinking).map_err(Fail::Stream)?,
            }
            // The OpenAI family reports usage in a trailing chunk that
            // arrives *after* the one carrying `finish_reason`, so the
            // stream must not be cut short there — it ends at `[DONE]`.
            if blocks.done && !self.usage_after_finish() {
                break;
            }
        }
        if min_core::is_cancelled() {
            return Err(Fail::Stream("cancelled".into()));
        }
        Ok(blocks.finish())
    }
}

struct CancelReader<R> {
    inner: R,
}

impl<R> CancelReader<R> {
    fn new(inner: R) -> Self {
        CancelReader { inner }
    }
}

impl<R: Read> Read for CancelReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if min_core::is_cancelled() {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "cancelled"));
        }
        match self.inner.read(buf) {
            Ok(n) => {
                if min_core::is_cancelled() {
                    Err(std::io::Error::new(std::io::ErrorKind::Other, "cancelled"))
                } else {
                    Ok(n)
                }
            }
            Err(e) if min_core::is_cancelled() || e.kind() == std::io::ErrorKind::Interrupted => {
                Err(std::io::Error::new(std::io::ErrorKind::Other, "cancelled"))
            }
            Err(e) => Err(e),
        }
    }
}

fn parse_anthropic_chunk(
    v: &Value,
    b: &mut Blocks,
    on_delta: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str, &str),
) -> Result<(), String> {
    match v["type"].as_str().unwrap_or_default() {
        "message_start" => {
            // Anthropic reports the prompt tokens here, and counts cache
            // reads and cache writes apart from them.
            let u = &v["message"]["usage"];
            b.usage.input_tokens = num(&u["input_tokens"]);
            b.usage.cache_read_tokens = num(&u["cache_read_input_tokens"]);
            b.usage.cache_write_tokens = num(&u["cache_creation_input_tokens"]);
        }
        "content_block_start" => match v["content_block"]["type"].as_str().unwrap_or_default() {
            "text" => b.push_text(),
            "thinking" | "redacted_thinking" => on_thinking("", ""),
            "tool_use" => {
                let id = v["content_block"]["id"].as_str().unwrap_or_default();
                let name = v["content_block"]["name"].as_str().unwrap_or_default();
                b.push_tool(id, name);
                on_thinking("", name);
            }
            _ => {}
        },
        "content_block_delta" => match v["delta"]["type"].as_str().unwrap_or_default() {
            "text_delta" => {
                let t = v["delta"]["text"].as_str().unwrap_or_default();
                b.append_text(t);
                on_delta(t);
            }
            "thinking_delta" => {
                let t = v["delta"]["thinking"].as_str().unwrap_or_default();
                on_thinking(t, "");
            }
            "signature_delta" => {
                on_thinking("", "");
            }
            "input_json_delta" => {
                b.append_json(v["delta"]["partial_json"].as_str().unwrap_or_default());
                let tool = b.tools.last().map(|t| t.1.as_str()).unwrap_or_default();
                on_thinking("", tool);
            }
            _ => {}
        },
        "message_delta" => {
            b.stop_reason = v["delta"]["stop_reason"].as_str().unwrap_or_default().to_string();
            // The generated tokens are only known once the message ends.
            b.usage.output_tokens = num(&v["usage"]["output_tokens"]);
        }
        "message_stop" => b.done = true,
        "error" => {
            return Err(v["error"]["message"].as_str().unwrap_or("stream error").to_string())
        }
        _ => {}
    }
    Ok(())
}

fn parse_openai_chunk(
    v: &Value,
    b: &mut Blocks,
    on_delta: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str, &str),
) -> Result<(), String> {
    if let Some(err) = v["error"]["message"].as_str() {
        return Err(err.to_string());
    }
    // The usage chunk carries `"choices": []` — it must be read before the
    // choices are looked at, or the count is silently lost.
    if let Some(u) = v.get("usage") {
        if !u.is_null() {
            b.usage.input_tokens = num(&u["prompt_tokens"]);
            b.usage.output_tokens = num(&u["completion_tokens"]);
            // OpenAI-style endpoints that support caching report the cached
            // prompt tokens apart from the prompt total.
            b.usage.cache_read_tokens = num(&u["prompt_tokens_details"]["cached_tokens"]);
        }
    }
    let Some(choice) = v["choices"].get(0) else {
        return Ok(());
    };
    let delta = &choice["delta"];
    if let Some(t) = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
    {
        if !t.is_empty() {
            on_thinking(t, "");
        }
    }
    if let Some(t) = delta["content"].as_str() {
        if !t.is_empty() {
            if delta["extra_content"]["google"]["thought"].as_bool() == Some(true) {
                on_thinking(t, "");
            } else {
                b.append_text(t);
                on_delta(t);
            }
        }
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            let idx = call["index"].as_u64().unwrap_or(0) as usize;
            let id = call["id"].as_str().unwrap_or_default();
            let name = call["function"]["name"].as_str().unwrap_or_default();
            b.tool_entry(idx, id, name);
            // Gemini's thought signature — must be echoed back with the call.
            if call.get("extra_content").is_some() {
                b.tools[idx].3 = call["extra_content"].clone();
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                b.append_json_at(idx, args);
            }
            let tool = b.tools[idx].1.as_str();
            on_thinking("", tool);
        }
    }
    match choice["finish_reason"].as_str() {
        Some("stop") => {
            b.stop_reason = "end_turn".into();
            b.done = true;
        }
        Some("tool_calls") | Some("function_call") => {
            b.stop_reason = "tool_use".into();
            b.done = true;
        }
        Some("length") => {
            b.stop_reason = "max_tokens".into();
            b.done = true;
        }
        _ => {}
    }
    Ok(())
}

/// One Ollama `/api/chat` NDJSON line. Unlike the OpenAI dialect, tool
/// calls arrive whole (arguments already an object) rather than as fragments.
fn parse_ollama_chunk(
    v: &Value,
    b: &mut Blocks,
    on_delta: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str, &str),
) -> Result<(), String> {
    if let Some(err) = v["error"].as_str() {
        return Err(err.to_string());
    }
    let msg = &v["message"];
    if let Some(t) = msg["thinking"].as_str() {
        if !t.is_empty() {
            on_thinking(t, "");
        }
    }
    if let Some(t) = msg["content"].as_str() {
        if !t.is_empty() {
            b.append_text(t);
            on_delta(t);
        }
    }
    if let Some(calls) = msg["tool_calls"].as_array() {
        for call in calls {
            let name = call["function"]["name"].as_str().unwrap_or_default();
            b.push_tool(call["id"].as_str().unwrap_or_default(), name);
            b.append_json(&call["function"]["arguments"].to_string());
            on_thinking("", name);
        }
    }
    if v["done"].as_bool() == Some(true) {
        // Ollama reports `stop` even when the turn ended in tool calls.
        b.stop_reason = match v["done_reason"].as_str() {
            _ if !b.tools.is_empty() => "tool_use",
            Some("length") => "max_tokens",
            _ => "end_turn",
        }
        .into();
        // The counts only arrive on the final line of the stream.
        b.usage.input_tokens = num(&v["prompt_eval_count"]);
        b.usage.output_tokens = num(&v["eval_count"]);
        b.done = true;
    }
    Ok(())
}

/// A number from a JSON value, as a token count. Anything that is not a
/// number counts as zero — a missing usage report is not an error.
fn num(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

/// `MIN_API` selectors: a wire dialect or an alias for a well-known
/// OpenAI-compatible endpoint. Returns `(dialect, base url, default model)` —
/// `MIN_BASE_URL` / `MIN_MODEL` still override the alias's picks.
fn resolve_api(spec: &str) -> (&'static str, &'static str, &'static str) {
    match spec {
        "openai" => ("openai", "https://api.openai.com/v1", "gpt-4o"),
        "zai" => ("openai", "https://api.z.ai/api/coding/paas/v4", "glm-5.3"),
        "openrouter" => ("openai", "https://openrouter.ai/api/v1", "anthropic/claude-sonnet-4.5"),
        // Vertex's OpenAI-compatible endpoint; the base depends on project and
        // region, so `vertex_setup` fills it in.
        "vertex" | "vertexai" => ("vertex", "", "google/gemini-2.5-pro"),
        // The base honours `OLLAMA_HOST`, so `from_env` fills it in.
        "ollama" => ("ollama", "", "qwen3-coder"),
        _ => ("anthropic", "https://api.anthropic.com", "claude-sonnet-4-5"),
    }
}

/// How requests authenticate.
enum Auth {
    /// A static API key (may be empty — reported as a problem).
    Key(String),
    /// No credentials at all (a local Ollama).
    None,
    /// Google Application Default Credentials, for Vertex AI.
    Google(google::Adc),
    /// Configuration that can't work, with the reason.
    Unusable(String),
}

impl Auth {
    /// Why requests can't be made, if they can't.
    fn problem(&self) -> Option<String> {
        match self {
            Auth::Key(k) if k.is_empty() => Some(
                "no API key set (ANTHROPIC_API_KEY, MIN_API_KEY/OPENAI_API_KEY, or ZAI_API_KEY for MIN_API=zai)".into(),
            ),
            Auth::Unusable(e) => Some(e.clone()),
            _ => None,
        }
    }
}

/// Load ADC and work out the Vertex endpoint: `(auth, base url)`.
fn vertex_setup() -> (Auth, String) {
    let adc = match google::Adc::from_env() {
        Ok(adc) => adc,
        Err(e) => return (Auth::Unusable(e), String::new()),
    };
    let project = Some(first_env(&["GOOGLE_CLOUD_PROJECT", "CLOUDSDK_CORE_PROJECT", "GCLOUD_PROJECT"]))
        .filter(|p| !p.is_empty())
        .or_else(|| adc.project.clone())
        .or_else(|| adc.quota_project.clone())
        .or_else(|| adc.on_metadata().then(google::metadata_project).flatten());
    let Some(project) = project else {
        let e = format!("no Google Cloud project for Vertex AI (credentials: {}) — set GOOGLE_CLOUD_PROJECT", adc.origin);
        return (Auth::Unusable(e), String::new());
    };
    let location = first_env(&["GOOGLE_CLOUD_LOCATION", "GOOGLE_CLOUD_REGION"]);
    let location = if location.is_empty() { "global".to_string() } else { location };
    (Auth::Google(adc), vertex_base(&project, &location))
}

/// Ollama's base URL from `OLLAMA_HOST`, which is often a bare `host:port`.
fn ollama_base(host: &str) -> String {
    let host = host.trim().trim_end_matches('/');
    match host {
        "" => "http://localhost:11434".into(),
        h if h.contains("://") => h.into(),
        h if h.contains(':') => format!("http://{h}"),
        h => format!("http://{h}:11434"),
    }
}

/// Vertex AI's OpenAI-compatible endpoint for a project and region.
fn vertex_base(project: &str, location: &str) -> String {
    let host = match location {
        "global" => "aiplatform.googleapis.com".to_string(),
        l => format!("{l}-aiplatform.googleapis.com"),
    };
    format!("https://{host}/v1/projects/{project}/locations/{location}/endpoints/openapi")
}

fn first_env(names: &[&str]) -> String {
    for n in names {
        if let Ok(v) = std::env::var(n) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    String::new()
}

const PLAN_HINT: &str = "check the key (fingerprint above) and which portal its subscription \
     belongs to — coding plans are regional (api.z.ai ↔ open.bigmodel.cn), and \
     only work on .../api/coding/paas/v4 endpoints, not the general \
     .../api/paas/v4. Pin the working endpoint with MIN_BASE_URL.";

/// The z.ai coding plan is served from two regional portals; return the
/// other one if `base` is either, else `None`.
fn mirror_base(base: &str) -> Option<&'static str> {
    if base.starts_with("https://api.z.ai/api/coding") {
        Some("https://open.bigmodel.cn/api/coding/paas/v4")
    } else if base.starts_with("https://open.bigmodel.cn/api/coding") {
        Some("https://api.z.ai/api/coding/paas/v4")
    } else {
        None
    }
}

/// A classified failure from one request attempt.
enum Fail {
    Http { status: u16, body: String, retry_after: Option<u64> },
    Stream(String),
    Transport(String),
    /// Couldn't obtain credentials — not retried.
    Auth(String),
}

impl Fail {
    fn message(&self) -> String {
        match self {
            Fail::Http { status, body, .. } => format!("HTTP {status}: {}", body.trim()),
            Fail::Stream(m) | Fail::Transport(m) | Fail::Auth(m) => m.clone(),
        }
    }
    fn brief(&self) -> String {
        match self {
            Fail::Http { status: 429, .. } => "429 rate-limited".into(),
            Fail::Http { status, .. } => format!("HTTP {status}"),
            Fail::Transport(_) => "network error".into(),
            Fail::Stream(_) => "stream error".into(),
            Fail::Auth(_) => "auth error".into(),
        }
    }
    fn is_retryable(&self) -> bool {
        match self {
            Fail::Http { status: 429, body, .. } => !is_plan_mismatch(body),
            Fail::Http { status: 500..=599, .. } => true,
            Fail::Transport(_) => true,
            _ => false,
        }
    }
}

/// z.ai/bigmodel return 429 (code 1113) when the endpoint doesn't match the
/// key's plan — distinct from an actual rate limit.
fn is_plan_mismatch(body: &str) -> bool {
    let b = body.to_lowercase();
    b.contains("insufficient balance") || b.contains("no resource package") || b.contains("\"1113\"")
}

/// Normalized stream accumulator shared by both dialects.
#[derive(Default)]
struct Blocks {
    text: String,
    tools: Vec<(String, String, String, Value)>, // (id, name, accumulated json, extra_content)
    stop_reason: String,
    done: bool,
    /// Token accounting for the request, as reported by the endpoint.
    usage: TokenUsage,
}

impl Blocks {
    fn push_text(&mut self) {}
    fn push_tool(&mut self, id: &str, name: &str) {
        self.tools.push((id.into(), name.into(), String::new(), Value::Null));
    }
    fn append_text(&mut self, t: &str) {
        self.text.push_str(t);
    }
    fn append_json(&mut self, j: &str) {
        if let Some(last) = self.tools.last_mut() {
            last.2.push_str(j);
        }
    }
    fn tool_entry(&mut self, idx: usize, id: &str, name: &str) {
        while self.tools.len() <= idx {
            self.tools.push((String::new(), String::new(), String::new(), Value::Null));
        }
        if !id.is_empty() {
            self.tools[idx].0 = id.into();
        }
        if !name.is_empty() {
            self.tools[idx].1 = name.into();
        }
    }
    fn append_json_at(&mut self, idx: usize, j: &str) {
        self.tool_entry(idx, "", "");
        self.tools[idx].2.push_str(j);
    }
    fn finish(&self) -> Value {
        let mut content: Vec<Value> = Vec::new();
        if !self.text.is_empty() {
            content.push(json!({"type": "text", "text": self.text}));
        }
        for (i, (id, name, args, extra)) in self.tools.iter().enumerate() {
            let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
            // Some endpoints (Gemini) may omit call ids; results must still pair up.
            let id = if id.is_empty() { format!("call_{i}") } else { id.clone() };
            let mut block = json!({"type": "tool_use", "id": id, "name": name, "input": input});
            if !extra.is_null() {
                block["extra_content"] = extra.clone();
            }
            content.push(block);
        }
        let stop = if self.stop_reason.is_empty() { "end_turn" } else { &self.stop_reason };
        json!({
            "stop_reason": stop,
            "content": content,
            "usage": {
                "input_tokens": self.usage.input_tokens,
                "output_tokens": self.usage.output_tokens,
                "cache_read_tokens": self.usage.cache_read_tokens,
                "cache_write_tokens": self.usage.cache_write_tokens,
            },
        })
    }
}

/// A min tool spec as an OpenAI/Ollama `function` tool.
fn to_function(t: &Value) -> Value {
    json!({"type": "function", "function": {
        "name": t["name"],
        "description": t["description"],
        "parameters": t["input_schema"],
    }})
}

/// Convert one Anthropic-style history message to Ollama wire message(s):
/// the OpenAI shape, but with tool arguments as objects and tool results
/// keyed by `tool_name` (looked up via `names`, tool_use id → name).
fn to_ollama_messages(m: &Value, names: &mut std::collections::HashMap<String, String>) -> Vec<Value> {
    let mut out = to_openai_messages(m);
    for msg in &mut out {
        if let Some(calls) = msg["tool_calls"].as_array_mut() {
            for call in calls {
                let f = &mut call["function"];
                let args = f["arguments"].as_str().and_then(|a| serde_json::from_str(a).ok());
                f["arguments"] = args.unwrap_or_else(|| json!({}));
                if let (Some(id), Some(name)) = (call["id"].as_str(), call["function"]["name"].as_str()) {
                    names.insert(id.into(), name.into());
                }
            }
        }
        if msg["role"] == "tool" {
            let name = msg["tool_call_id"].as_str().and_then(|id| names.get(id)).cloned();
            if let Some(name) = name {
                msg["tool_name"] = json!(name);
            }
        }
    }
    out
}

/// Convert one Anthropic-style history message to OpenAI wire message(s).
fn to_openai_messages(m: &Value) -> Vec<Value> {
    let role = m["role"].as_str().unwrap_or("user");
    match m["content"] {
        Value::String(ref s) => vec![json!({"role": role, "content": s})],
        Value::Array(ref blocks) => {
            let mut out = Vec::new();
            if role == "assistant" {
                let text: String = blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");
                let calls: Vec<Value> = blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_use")
                    .map(|b| {
                        let mut call = json!({"id": b["id"], "type": "function",
                               "function": {"name": b["name"], "arguments": b["input"].to_string()}});
                        if let Some(extra) = b.get("extra_content") {
                            call["extra_content"] = extra.clone();
                        }
                        call
                    })
                    .collect();
                let mut msg = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                out.push(msg);
            } else {
                // user message carrying tool_result blocks (possibly + text)
                for b in blocks {
                    if b["type"] == "tool_result" {
                        out.push(json!({"role": "tool", "tool_call_id": b["tool_use_id"],
                                        "content": b["content"]}));
                    } else if b["type"] == "text" {
                        out.push(json!({"role": "user", "content": b["text"]}));
                    }
                }
            }
            out
        }
        _ => vec![],
    }
}

fn error_body(r: ureq::Response) -> String {
    let mut s = String::new();
    let _ = r.into_reader().take(4000).read_to_string(&mut s);
    serde_json::from_str::<Value>(&s)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
                // Ollama: {"error": "model \"x\" not found"}
                .or_else(|| v["error"].as_str())
                // Vertex wraps errors in an array: [{"error": {…}}]
                .or_else(|| v[0]["error"]["message"].as_str())
                .map(String::from)
        })
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn blocks_finish_normalizes() {
        let mut b = Blocks::default();
        b.push_tool("t1", "bash");
        b.append_json_at(0, "{\"command\"");
        b.append_json_at(0, ":\"ls\"}");
        b.append_text("hi");
        let v = b.finish();
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["content"][0]["text"], "hi");
        assert_eq!(v["content"][1]["input"]["command"], "ls");
    }

    #[test]
    fn plan_mismatch_429_is_not_retryable() {
        let body = "{\"error\":{\"code\":\"1113\",\"message\":\"Insufficient balance or no resource package. Please recharge.\"}}";
        assert!(is_plan_mismatch(body));
        let f = Fail::Http { status: 429, body: body.into(), retry_after: None };
        assert!(!f.is_retryable());
        assert!(f.message().starts_with("HTTP 429: {\"error\""));
        assert_eq!(f.brief(), "429 rate-limited");
    }

    #[test]
    fn genuine_429_is_retryable() {
        let f = Fail::Http { status: 429, body: "Too Many Requests".into(), retry_after: Some(7) };
        assert!(f.is_retryable());
        let f = Fail::Http { status: 503, body: "unavailable".into(), retry_after: None };
        assert!(f.is_retryable());
        let f = Fail::Http { status: 401, body: "bad key".into(), retry_after: None };
        assert!(!f.is_retryable());
        let f = Fail::Stream("mid-stream".into());
        assert!(!f.is_retryable());
    }

    #[test]
    fn mirror_portal_is_bidirectional() {
        assert_eq!(
            mirror_base("https://api.z.ai/api/coding/paas/v4"),
            Some("https://open.bigmodel.cn/api/coding/paas/v4")
        );
        assert_eq!(
            mirror_base("https://open.bigmodel.cn/api/coding/paas/v4"),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
        assert_eq!(mirror_base("https://api.z.ai/api/paas/v4"), None);
        assert_eq!(mirror_base("https://api.openai.com/v1"), None);
    }

    #[test]
    fn api_aliases_resolve() {
        let (api, base, model) = resolve_api("");
        assert_eq!((api, base, model), ("anthropic", "https://api.anthropic.com", "claude-sonnet-4-5"));
        let (api, base, model) = resolve_api("zai");
        assert_eq!((api, base, model), ("openai", "https://api.z.ai/api/coding/paas/v4", "glm-5.3"));
        assert_eq!(resolve_api("openai").0, "openai");
        assert_eq!(resolve_api("openrouter").0, "openai");
        assert_eq!(resolve_api("vertex"), ("vertex", "", "google/gemini-2.5-pro"));
    }

    #[test]
    fn ollama_hosts_resolve() {
        assert_eq!(resolve_api("ollama"), ("ollama", "", "qwen3-coder"));
        assert_eq!(ollama_base(""), "http://localhost:11434");
        assert_eq!(ollama_base("0.0.0.0:9999"), "http://0.0.0.0:9999");
        assert_eq!(ollama_base("gpu-box"), "http://gpu-box:11434");
        assert_eq!(ollama_base("https://ollama.example.com/"), "https://ollama.example.com");
    }

    #[test]
    fn ollama_body_uses_native_shapes() {
        let llm = Llm {
            http: ureq::Agent::new(),
            api: "ollama",
            base: RefCell::new(String::new()),
            mirror: None,
            mirrored: Cell::new(false),
            auth: Auth::None,
            model: "qwen3-coder".into(),
            max_tokens: 1024,
            num_ctx: 65536,
        };
        assert!(llm.auth.problem().is_none());
        let history = vec![
            json!({"role": "user", "content": "list files"}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_0", "name": "bash", "input": {"command": "ls"}}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_0", "content": "a.txt"}]}),
        ];
        let tools = vec![json!({"name": "bash", "description": "run", "input_schema": {"type": "object"}})];
        let b = llm.build_body("sys", &history, &tools);
        assert_eq!(b["options"], json!({"num_ctx": 65536, "num_predict": 1024}));
        assert_eq!(b["messages"][0], json!({"role": "system", "content": "sys"}));
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["arguments"], json!({"command": "ls"}));
        assert_eq!(b["messages"][3]["role"], "tool");
        assert_eq!(b["messages"][3]["tool_name"], "bash");
        assert_eq!(b["tools"][0]["function"]["name"], "bash");
        assert!(b.get("max_tokens").is_none());
    }

    #[test]
    fn ollama_chunks_normalize() {
        let mut b = Blocks::default();
        let mut deltas = Vec::new();
        let mut thoughts: Vec<(String, String)> = Vec::new();
        let mut run = |v: Value, b: &mut Blocks| {
            parse_ollama_chunk(&v, b, &mut |t| deltas.push(t.to_string()), &mut |t, tl| {
                thoughts.push((t.into(), tl.into()))
            })
        };
        run(json!({"message": {"role": "assistant", "content": "", "thinking": "hmm"}, "done": false}), &mut b).unwrap();
        run(json!({"message": {"role": "assistant", "content": "ok"}, "done": false}), &mut b).unwrap();
        run(json!({"message": {"role": "assistant", "content": "", "tool_calls": [
            {"function": {"name": "bash", "arguments": {"command": "ls"}}}]}, "done": false}), &mut b).unwrap();
        run(json!({"message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "stop"}), &mut b).unwrap();
        assert!(b.done);
        let v = b.finish();
        assert_eq!(v["stop_reason"], "tool_use");
        assert_eq!(v["content"][0]["text"], "ok");
        assert_eq!(v["content"][1]["id"], "call_0");
        assert_eq!(v["content"][1]["input"]["command"], "ls");
        assert_eq!(deltas, vec!["ok"]);
        assert_eq!(thoughts, vec![("hmm".into(), "".into()), ("".into(), "bash".into())]);

        let mut b = Blocks::default();
        let quiet = |v: Value, b: &mut Blocks| parse_ollama_chunk(&v, b, &mut |_| {}, &mut |_, _| {});
        quiet(json!({"message": {"content": "x"}, "done": true, "done_reason": "length"}), &mut b).unwrap();
        assert_eq!(b.finish()["stop_reason"], "max_tokens");
        assert_eq!(quiet(json!({"error": "model not found"}), &mut b).unwrap_err(), "model not found");
    }

    /// Anthropic reports the prompt tokens in `message_start` and the
    /// generated ones in `message_delta`; cache tokens are counted apart
    /// from the prompt tokens.
    #[test]
    fn anthropic_usage_is_normalized() {
        let mut b = Blocks::default();
        let quiet = |v: Value, b: &mut Blocks| {
            parse_anthropic_chunk(&v, b, &mut |_| {}, &mut |_, _| {})
        };
        quiet(
            json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 100,
                "cache_read_input_tokens": 4000,
                "cache_creation_input_tokens": 500}}}),
            &mut b,
        )
        .unwrap();
        quiet(
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 42}}),
            &mut b,
        )
        .unwrap();

        let u = &b.finish()["usage"];
        assert_eq!(u["input_tokens"], 100);
        assert_eq!(u["output_tokens"], 42);
        assert_eq!(u["cache_read_tokens"], 4000);
        assert_eq!(u["cache_write_tokens"], 500);
        // The prompt as Anthropic saw it: 100 + 4000 + 500.
        assert_eq!(u["input_tokens"].as_u64().unwrap()
            + u["cache_read_tokens"].as_u64().unwrap()
            + u["cache_write_tokens"].as_u64().unwrap(),
            4600);
    }

    /// The OpenAI family reports usage in a trailing chunk that carries
    /// `"choices": []` — it must be read anyway, or the count is lost.
    #[test]
    fn openai_usage_chunk_without_choices_is_still_read() {
        let mut b = Blocks::default();
        let quiet = |v: Value, b: &mut Blocks| {
            parse_openai_chunk(&v, b, &mut |_| {}, &mut |_, _| {})
        };
        quiet(
            json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}),
            &mut b,
        )
        .unwrap();
        assert!(b.done, "the finish reason ends the message");
        // The usage chunk that follows: no choices at all.
        quiet(
            json!({"choices": [], "usage": {"prompt_tokens": 321, "completion_tokens": 7,
                   "prompt_tokens_details": {"cached_tokens": 64}}}),
            &mut b,
        )
        .unwrap();

        let u = &b.finish()["usage"];
        assert_eq!(u["input_tokens"], 321);
        assert_eq!(u["output_tokens"], 7);
        assert_eq!(u["cache_read_tokens"], 64);
    }

    /// Ollama reports both counts on its final line.
    #[test]
    fn ollama_usage_is_normalized() {
        let mut b = Blocks::default();
        let quiet = |v: Value, b: &mut Blocks| {
            parse_ollama_chunk(&v, b, &mut |_| {}, &mut |_, _| {})
        };
        quiet(
            json!({"message": {"content": "ok"}, "done": true, "done_reason": "stop",
                   "prompt_eval_count": 900, "eval_count": 25}),
            &mut b,
        )
        .unwrap();
        let u = &b.finish()["usage"];
        assert_eq!(u["input_tokens"], 900);
        assert_eq!(u["output_tokens"], 25);
    }

    /// An endpoint that reports nothing must not read as "zero tokens" —
    /// the numbers stay absent, which is what `is_empty` is for.
    #[test]
    fn missing_usage_is_empty_not_zero_cost() {
        let mut b = Blocks::default();
        let quiet = |v: Value, b: &mut Blocks| {
            parse_openai_chunk(&v, b, &mut |_| {}, &mut |_, _| {})
        };
        quiet(
            json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}),
            &mut b,
        )
        .unwrap();
        let u = b.finish()["usage"].clone();
        let usage = TokenUsage {
            input_tokens: u["input_tokens"].as_u64().unwrap(),
            output_tokens: u["output_tokens"].as_u64().unwrap(),
            cache_read_tokens: u["cache_read_tokens"].as_u64().unwrap(),
            cache_write_tokens: u["cache_write_tokens"].as_u64().unwrap(),
        };
        assert!(usage.is_empty());
    }

    /// The OpenAI body must ask for usage — the endpoint only reports it
    /// when the request says so.
    #[test]
    fn openai_body_asks_for_usage() {
        let llm = Llm {
            http: ureq::Agent::new(),
            api: "openai",
            base: RefCell::new(String::new()),
            mirror: None,
            mirrored: Cell::new(false),
            auth: Auth::Key("k".into()),
            model: "gpt-4o".into(),
            max_tokens: 1024,
            num_ctx: 32768,
        };
        let b = llm.build_body("sys", &[], &[]);
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
    }

    /// The stream must not be cut short at the finish reason: the OpenAI
    /// family's usage chunk arrives after it.
    #[test]
    fn openai_stream_reads_past_the_finish_reason() {
        let llm = Llm {
            http: ureq::Agent::new(),
            api: "openai",
            base: RefCell::new(String::new()),
            mirror: None,
            mirrored: Cell::new(false),
            auth: Auth::Key("k".into()),
            model: "gpt-4o".into(),
            max_tokens: 1024,
            num_ctx: 32768,
        };
        assert!(llm.usage_after_finish());

        let anthropic = Llm {
            api: "anthropic",
            ..llm
        };
        assert!(!anthropic.usage_after_finish());
    }

    #[test]
    fn vertex_endpoints() {        assert_eq!(
            vertex_base("p1", "global"),
            "https://aiplatform.googleapis.com/v1/projects/p1/locations/global/endpoints/openapi"
        );
        assert_eq!(
            vertex_base("p1", "europe-west4"),
            "https://europe-west4-aiplatform.googleapis.com/v1/projects/p1/locations/europe-west4/endpoints/openapi"
        );
    }

    #[test]
    fn vertex_body_requests_thoughts() {
        let make = |api: &'static str, model: &str| Llm {
            http: ureq::Agent::new(),
            api,
            base: RefCell::new(String::new()),
            mirror: None,
            mirrored: Cell::new(false),
            auth: Auth::Key("k".into()),
            model: model.into(),
            max_tokens: 1024,
            num_ctx: 32768,
        };
        let b25 = make("vertex", "google/gemini-2.5-pro").build_body("sys", &[], &[]);
        assert_eq!(b25["extra_body"]["google"]["thinking_config"], json!({"include_thoughts": true}));

        let b4 = make("vertex", "google/gemini-4-argon").build_body("sys", &[], &[]);
        assert_eq!(
            b4["extra_body"]["google"]["thinking_config"],
            json!({"include_thoughts": true, "thinking_level": "HIGH"})
        );

        let oai = make("openai", "gpt-4o").build_body("sys", &[], &[]);
        assert!(oai.get("extra_body").is_none());
    }

    #[test]
    fn gemini_thought_signature_round_trips() {
        let mut b = Blocks::default();
        b.tool_entry(0, "", "bash");
        b.tools[0].3 = json!({"google": {"thought_signature": "sig"}});
        b.append_json_at(0, "{}");
        let v = b.finish();
        let block = &v["content"][0];
        assert_eq!(block["id"], "call_0");
        let msg = json!({"role": "assistant", "content": [block.clone()]});
        let wire = to_openai_messages(&msg);
        assert_eq!(wire[0]["tool_calls"][0]["extra_content"]["google"]["thought_signature"], "sig");
    }

    #[test]
    fn openai_history_conversion() {
        let assistant = json!({"role":"assistant","content":[
            {"type":"text","text":"running it"},
            {"type":"tool_use","id":"t9","name":"bash","input":{"command":"ls"}}
        ]});
        let wire = to_openai_messages(&assistant);
        assert_eq!(wire[0]["content"], "running it");
        assert_eq!(wire[0]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(wire[0]["tool_calls"][0]["function"]["arguments"], "{\"command\":\"ls\"}");

        let results = json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t9","content":"file\n"}
        ]});
        let wire = to_openai_messages(&results);
        assert_eq!(wire[0]["role"], "tool");
        assert_eq!(wire[0]["tool_call_id"], "t9");
    }

    #[test]
    fn anthropic_chunks_report_thinking_and_tool_streaming() {
        let mut b = Blocks::default();
        let mut deltas = Vec::new();
        let mut thoughts: Vec<(String, String)> = Vec::new();
        let mut run = |v: Value| {
            parse_anthropic_chunk(
                &v,
                &mut b,
                &mut |t| deltas.push(t.to_string()),
                &mut |text, tool| thoughts.push((text.to_string(), tool.to_string())),
            )
            .unwrap();
        };
        run(json!({"type": "content_block_start", "content_block": {"type": "thinking"}}));
        run(json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "plan"}}));
        run(json!({"type": "content_block_start", "content_block": {"type": "tool_use", "id": "t1", "name": "edit"}}));
        run(json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"a\"}"}}));
        assert!(deltas.is_empty());
        assert_eq!(
            thoughts,
            vec![
                ("".into(), "".into()),
                ("plan".into(), "".into()),
                ("".into(), "edit".into()),
                ("".into(), "edit".into()),
            ]
        );
    }

    #[test]
    fn openai_chunks_report_reasoning_and_tool_streaming() {
        let mut b = Blocks::default();
        let mut deltas = Vec::new();
        let mut thoughts: Vec<(String, String)> = Vec::new();
        let mut run = |v: Value| {
            parse_openai_chunk(
                &v,
                &mut b,
                &mut |t| deltas.push(t.to_string()),
                &mut |text, tool| thoughts.push((text.to_string(), tool.to_string())),
            )
            .unwrap();
        };
        run(json!({"choices": [{"delta": {"reasoning_content": "step 1"}}]}));
        // Vertex/Gemini streams thought summaries in delta.content with extra_content.google.thought = true.
        run(json!({"choices": [{"delta": {"content": "step 2", "extra_content": {"google": {"thought": true}}}}]}));
        run(json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "bash", "arguments": "{\"c"}}]}}]}));
        // Subsequent argument chunks omit the function name, but the callback still carries it.
        run(json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "md\":\"ls\"}"}}]}}]}));
        assert!(deltas.is_empty());
        assert_eq!(b.text, "");
        assert_eq!(
            thoughts,
            vec![
                ("step 1".into(), "".into()),
                ("step 2".into(), "".into()),
                ("".into(), "bash".into()),
                ("".into(), "bash".into()),
            ]
        );
    }

    #[test]
    fn cancel_reader_aborts_on_cancellation_or_interrupted() {
        use std::io::Cursor;
        let _guard = LOCK.lock().unwrap();

        min_core::reset_cancel();
        let data = Cursor::new(b"hello world\n");
        let mut reader = CancelReader::new(data);
        let mut buf = [0u8; 5];
        assert_eq!(reader.read(&mut buf).unwrap(), 5);
        assert_eq!(&buf, b"hello");

        // Cancel mid-stream: next read fails
        min_core::cancel();
        assert!(reader.read(&mut buf).is_err());
        assert_eq!(reader.read(&mut buf).unwrap_err().to_string(), "cancelled");
        min_core::reset_cancel();
    }

    /// The whole path over a real socket: a fake OpenAI endpoint streams
    /// a finish reason and then a usage chunk, and the client must come
    /// back with both the text and the count.
    ///
    /// The server is a thread in this process, bound to an ephemeral port —
    /// no external process, no fixed port.
    #[test]
    fn usage_survives_a_real_stream() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            // Read the request headers and body (headers end at a blank line).
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8_lossy(&body).to_string();
            let asked = body.contains("\"include_usage\":true");

            let mut out = stream;
            out.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n").unwrap();
            // The text, then the finish reason, then the usage chunk.
            out.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n").unwrap();
            out.write_all(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n").unwrap();
            out.write_all(
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1234,\"completion_tokens\":56}}\n\n",
            )
            .unwrap();
            out.write_all(b"data: [DONE]\n\n").unwrap();
            out.flush().unwrap();
            asked
        });

        let llm = Llm {
            http: ureq::Agent::new(),
            api: "openai",
            base: RefCell::new(format!("http://127.0.0.1:{port}/v1")),
            mirror: None,
            mirrored: Cell::new(false),
            auth: Auth::Key("k".into()),
            model: "gpt-4o".into(),
            max_tokens: 1024,
            num_ctx: 32768,
        };
        let mut deltas = Vec::new();
        let res = llm
            .complete_inner(
                "sys",
                &[json!({"role": "user", "content": "hi"})],
                &[],
                &mut |d| deltas.push(d.to_string()),
                &mut |_, _| {},
                &mut |_| {},
            )
            .expect("the request succeeded");

        assert!(server.join().unwrap(), "the request asked for usage");
        assert_eq!(deltas, vec!["hi".to_string()]);
        assert_eq!(res["content"][0]["text"], "hi");
        // The usage chunk arrived *after* the finish reason and was still read.
        assert_eq!(res["usage"]["input_tokens"], 1234);
        assert_eq!(res["usage"]["output_tokens"], 56);
    }

    #[test]
    fn complete_aborts_immediately_if_cancelled() {
        let _guard = LOCK.lock().unwrap();

        min_core::cancel();
        let llm = Llm {
            http: ureq::Agent::new(),
            api: "anthropic",
            base: RefCell::new("https://api.anthropic.com".into()),
            mirror: None,
            mirrored: Cell::new(false),
            auth: Auth::Key("test-key".into()),
            model: "claude".into(),
            max_tokens: 1024,
            num_ctx: 32768,
        };
        let mut deltas = Vec::new();
        let mut thoughts = Vec::new();
        let mut status = Vec::new();
        let res = llm.complete_inner(
            "sys",
            &[],
            &[],
            &mut |d| deltas.push(d.to_string()),
            &mut |t, tl| thoughts.push((t.to_string(), tl.to_string())),
            &mut |s| status.push(s.to_string()),
        );
        assert!(res.is_err());
        assert_eq!(res.unwrap_err(), "cancelled");
        min_core::reset_cancel();
    }
}
