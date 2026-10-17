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
//! Environment:
//! - `MIN_API`            dialect or endpoint alias: `"anthropic"` (default),
//!                        `"openai"`, `"zai"` (Z.ai coding plan), `"openrouter"`
//! - `MIN_BASE_URL`       overrides the endpoint an alias picks (default
//!                        `https://api.anthropic.com`)
//! - `MIN_MODEL`          overrides the model an alias picks
//!                        (default `claude-sonnet-4-5`)
//! - `MIN_MAX_TOKENS`     default `16384`
//! - `ANTHROPIC_API_KEY`  key for the anthropic dialect
//! - `MIN_API_KEY` / `OPENAI_API_KEY` / `ZAI_API_KEY`  keys for openai-style endpoints
//! - `HTTPS_PROXY`        optional outbound proxy

use min_core::{events, Hub, Plugin};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

/// The plugin: publishes the [`Llm`] service and greets the user.
pub struct LlmPlugin;

impl Plugin for LlmPlugin {
    fn name(&self) -> &'static str {
        "llm"
    }

    fn start(&mut self, hub: &Hub) {
        let llm = Llm::from_env();
        if llm.api_key.is_empty() {
            hub.emit(events::Info {
                text: "warning: no API key set (ANTHROPIC_API_KEY, MIN_API_KEY/OPENAI_API_KEY, or ZAI_API_KEY for MIN_API=zai)".into(),
            });
        }
        let host = llm.base.split("//").nth(1).unwrap_or(llm.base.as_str());
        hub.emit(events::Info {
            text: format!("{} @ {} — /help for commands", llm.model, host),
        });
        hub.set_service(llm);
    }
}

/// A streaming chat client for exactly one configured endpoint.
pub struct Llm {
    http: ureq::Agent,
    api: &'static str,
    base: String,
    api_key: String,
    pub model: String,
    max_tokens: u64,
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
            _ => vec!["ANTHROPIC_API_KEY"],
        };
        if spec == "zai" {
            key_names.push("ZAI_API_KEY");
        }
        let base = std::env::var("MIN_BASE_URL").unwrap_or_default();
        let base = {
            let b = base.trim_end_matches('/');
            if b.is_empty() { base_default.to_string() } else { b.to_string() }
        };
        let model = std::env::var("MIN_MODEL").unwrap_or_default();
        let model = if model.is_empty() { model_default.to_string() } else { model };
        Llm {
            http: builder.build(),
            api,
            base,
            api_key: first_env(&key_names),
            model,
            max_tokens: std::env::var("MIN_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16384),
        }
    }

    /// Stream one completion. `messages` are Anthropic-style; `tools` are
    /// `[{"name","description","input_schema"}]`; `on_delta` receives text as
    /// it arrives. Returns the normalized message (see crate docs).
    pub fn complete(
        &self,
        system: &str,
        messages: &[Value],
        tools: &[Value],
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<Value, String> {
        if self.api_key.is_empty() {
            return Err("no API key set for the LLM (see MIN_API / ANTHROPIC_API_KEY / MIN_API_KEY)".into());
        }
        let body = self.build_body(system, messages, tools);
        let mut streamed = false;
        let mut last_err = String::new();
        for attempt in 0..4u64 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(1000 * attempt));
            }
            let mut delta = |t: &str| {
                streamed = true;
                on_delta(t);
            };
            match self.try_once(&body, &mut delta) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = e;
                    let retryable = ["429", "overloaded", "500", "502", "503", "529", "timeout", "connection"]
                        .iter().any(|s| last_err.to_lowercase().contains(s));
                    if streamed || !retryable {
                        return Err(last_err);
                    }
                }
            }
        }
        Err(last_err)
    }

    fn build_body(&self, system: &str, messages: &[Value], tools: &[Value]) -> Value {
        match self.api {
            "openai" => {
                let mut wire: Vec<Value> = vec![json!({"role": "system", "content": system})];
                for m in messages {
                    wire.extend(to_openai_messages(m));
                }
                let fns: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        json!({"type": "function", "function": {
                            "name": t["name"],
                            "description": t["description"],
                            "parameters": t["input_schema"],
                        }})
                    })
                    .collect();
                json!({"model": self.model, "max_tokens": self.max_tokens, "messages": wire, "tools": fns, "stream": true})
            }
            _ => json!({
                "model": self.model, "max_tokens": self.max_tokens,
                "system": system, "messages": messages, "tools": tools, "stream": true,
            }),
        }
    }

    fn try_once(&self, body: &Value, on_delta: &mut dyn FnMut(&str)) -> Result<Value, String> {
        let url = match self.api {
            "openai" => format!("{}/chat/completions", self.base),
            _ => format!("{}/v1/messages", self.base),
        };
        let mut req = self.http.post(&url);
        req = match self.api {
            "openai" => req.set("Authorization", &format!("Bearer {}", self.api_key)),
            _ => req
                .set("x-api-key", &self.api_key)
                .set("anthropic-version", "2023-06-01"),
        };
        let resp = match req.send_json(body.clone()) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => return Err(format!("HTTP {code}: {}", error_body(r))),
            Err(e) => return Err(e.to_string()),
        };
        let mut blocks = Blocks::default();
        for line in BufReader::new(resp.into_reader()).lines() {
            let line = line.map_err(|e| format!("stream failed: {e}"))?;
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            match self.api {
                "openai" => self.parse_openai_chunk(&v, &mut blocks, on_delta)?,
                _ => self.parse_anthropic_chunk(&v, &mut blocks, on_delta)?,
            }
            if blocks.done {
                break;
            }
        }
        Ok(blocks.finish())
    }

    fn parse_anthropic_chunk(
        &self,
        v: &Value,
        b: &mut Blocks,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        match v["type"].as_str().unwrap_or_default() {
            "content_block_start" => match v["content_block"]["type"].as_str().unwrap_or_default() {
                "text" => b.push_text(),
                "tool_use" => b.push_tool(
                    v["content_block"]["id"].as_str().unwrap_or_default(),
                    v["content_block"]["name"].as_str().unwrap_or_default(),
                ),
                _ => {}
            },
            "content_block_delta" => match v["delta"]["type"].as_str().unwrap_or_default() {
                "text_delta" => {
                    let t = v["delta"]["text"].as_str().unwrap_or_default();
                    b.append_text(t);
                    on_delta(t);
                }
                "input_json_delta" => {
                    b.append_json(v["delta"]["partial_json"].as_str().unwrap_or_default());
                }
                _ => {}
            },
            "message_delta" => {
                b.stop_reason = v["delta"]["stop_reason"].as_str().unwrap_or_default().to_string();
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
        &self,
        v: &Value,
        b: &mut Blocks,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        if let Some(err) = v["error"]["message"].as_str() {
            return Err(err.to_string());
        }
        let Some(choice) = v["choices"].get(0) else {
            return Ok(());
        };
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str() {
            if !t.is_empty() {
                b.append_text(t);
                on_delta(t);
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let idx = call["index"].as_u64().unwrap_or(0) as usize;
                let id = call["id"].as_str().unwrap_or_default();
                let name = call["function"]["name"].as_str().unwrap_or_default();
                b.tool_entry(idx, id, name);
                if let Some(args) = call["function"]["arguments"].as_str() {
                    b.append_json_at(idx, args);
                }
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
}

/// `MIN_API` selectors: a wire dialect or an alias for a well-known
/// OpenAI-compatible endpoint. Returns `(dialect, base url, default model)` —
/// `MIN_BASE_URL` / `MIN_MODEL` still override the alias's picks.
fn resolve_api(spec: &str) -> (&'static str, &'static str, &'static str) {
    match spec {
        "openai" => ("openai", "https://api.openai.com/v1", "gpt-4o"),
        "zai" => ("openai", "https://api.z.ai/api/coding/paas/v4", "glm-5.3"),
        "openrouter" => ("openai", "https://openrouter.ai/api/v1", "anthropic/claude-sonnet-4.5"),
        _ => ("anthropic", "https://api.anthropic.com", "claude-sonnet-4-5"),
    }
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

/// Normalized stream accumulator shared by both dialects.
#[derive(Default)]
struct Blocks {
    text: String,
    tools: Vec<(String, String, String)>, // (id, name, accumulated json)
    stop_reason: String,
    done: bool,
}

impl Blocks {
    fn push_text(&mut self) {}
    fn push_tool(&mut self, id: &str, name: &str) {
        self.tools.push((id.into(), name.into(), String::new()));
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
            self.tools.push((String::new(), String::new(), String::new()));
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
        for (id, name, args) in &self.tools {
            let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
            content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
        let stop = if self.stop_reason.is_empty() { "end_turn" } else { &self.stop_reason };
        json!({"stop_reason": stop, "content": content})
    }
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
                        json!({"id": b["id"], "type": "function",
                               "function": {"name": b["name"], "arguments": b["input"].to_string()}})
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
                .map(String::from)
        })
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn api_aliases_resolve() {
        let (api, base, model) = resolve_api("");
        assert_eq!((api, base, model), ("anthropic", "https://api.anthropic.com", "claude-sonnet-4-5"));
        let (api, base, model) = resolve_api("zai");
        assert_eq!((api, base, model), ("openai", "https://api.z.ai/api/coding/paas/v4", "glm-5.3"));
        assert_eq!(resolve_api("openai").0, "openai");
        assert_eq!(resolve_api("openrouter").0, "openai");
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
}
