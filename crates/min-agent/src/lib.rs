//! min-agent — the agent loop.
//!
//! Owns the conversation. On [`UserInput`] it appends to the history, then
//! loops: call the LLM service → display text as it streams → run any tool
//! calls through the core's tool registry → feed results back — until the
//! model stops asking for tools. There is deliberately nothing clever here:
//! no subagents, no planning, no context management. A plugin can add that
//! later; this is the simplest loop that can edit code.

use min_core::{events, Hub, Plugin};
use serde_json::{json, Value};
use std::any::Any;
use std::time::Instant;

/// Safety valve: max LLM↔tool round-trips in a single user turn.
const MAX_ROUNDS: usize = 1000;

pub struct AgentPlugin {
    messages: Vec<Value>,
    system: String,
    /// Extra prompt sections handed over via `SystemPromptSection` events
    /// (e.g. the project's AGENTS.md, via min-context) and dropped again on
    /// `ClearPromptSections`: the emitter re-reads the files whenever the
    /// conversation is cleared, so `/clear` picks up on-disk edits too.
    sections: Vec<(String, String)>,
}

impl AgentPlugin {
    pub fn new() -> Self {
        AgentPlugin {
            messages: Vec::new(),
            system: String::new(),
            sections: Vec::new(),
        }
    }
}

impl Plugin for AgentPlugin {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn start(&mut self, _hub: &Hub) {
        self.system = build_system_prompt();
    }

    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if let Some(e) = event.downcast_ref::<events::SystemPromptSection>() {
            self.sections.push((e.title.clone(), e.body.clone()));
        } else if event.downcast_ref::<events::ClearPromptSections>().is_some() {
            self.sections.clear();
        } else if let Some(e) = event.downcast_ref::<events::UserInput>() {
            let text = e.text.clone();
            self.turn(hub, &text);
        } else if event.downcast_ref::<events::ResetConversation>().is_some() {
            self.messages.clear();
            hub.emit(events::Info {
                text: "conversation cleared".into(),
            });
        } else if let Some(e) = event.downcast_ref::<events::ReplayMessage>() {
            if let Ok(msg) = serde_json::from_str::<Value>(&e.json) {
                self.messages.push(msg);
            }
        }
    }
}

impl AgentPlugin {
    fn push_message(&mut self, hub: &Hub, msg: Value) {
        hub.emit(events::ConversationMessage {
            json: msg.to_string(),
        });
        self.messages.push(msg);
    }

    fn turn(&mut self, hub: &Hub, user_text: &str) {
        // Base prompt plus any sections plugins handed us (refreshed on /clear).
        let mut system = self.system.clone();
        for (title, body) in &self.sections {
            system.push_str(&format!("\n\n# {title}\n\n{body}"));
        }
        self.push_message(hub, json!({"role": "user", "content": user_text}));
        for round in 1..=MAX_ROUNDS {
            let Some(router) = hub.service::<min_core::llm::LlmRouter>() else {
                hub.emit(events::Error {
                    text: "no LLM router loaded — is the llm plugin loaded?".into(),
                });
                return;
            };
            // The provider owns the model: it resolved MIN_MODEL against
            // the endpoint's default. Ask it, rather than naming a model
            // here that may never be requested.
            let model = router.model().unwrap_or_default();
            let tools: Vec<Value> = hub
                .tools()
                .specs()
                .iter()
                .map(|(name, spec)| {
                    let mut v: Value = serde_json::from_str(spec).unwrap_or_else(|_| json!({}));
                    v["name"] = json!(name);
                    v
                })
                .collect();
            hub.emit(events::LlmStart {
                round,
                model: model.clone(),
                messages: self.messages.len(),
                tools: tools.len(),
            });
            let mut on_delta = |t: &str| {
                hub.emit(events::AssistantDelta { text: t.to_string() });
            };
            let mut on_thinking = |text: &str, tool: &str| {
                hub.emit(events::Thinking {
                    text: text.to_string(),
                    tool: tool.to_string(),
                });
            };
            let mut on_status = |s: &str| {
                hub.emit(events::Info { text: s.to_string() });
            };
            let messages_json = serde_json::to_string(&self.messages).unwrap_or_default();
            let tools_json = serde_json::to_string(&tools).unwrap_or_default();

            let message_str = match router.complete(
                &model,
                &system,
                &messages_json,
                &tools_json,
                min_core::llm::Callbacks {
                    on_delta: &mut on_delta,
                    on_thinking: &mut on_thinking,
                    on_status: &mut on_status,
                },
            ) {
                Ok(m) => m,
                Err(e) => {
                    // A cancelled turn is the user's own doing, not a
                    // failure — say so with the event every plugin
                    // already handles, instead of a red error.
                    if min_core::is_cancelled() {
                        hub.emit(events::TurnCancelled {});
                    } else {
                        hub.emit(events::Error { text: e });
                    }
                    return;
                }
            };
            
            let message: Value = serde_json::from_str(&message_str).unwrap_or_else(|_| json!({}));
            hub.emit(events::MessageEnd {});
            // Token accounting for the round that just finished. The
            // provider's own report, normalized by min-llm; endpoints that
            // do not report usage yield an empty (not zero-cost) record.
            hub.emit(events::Usage {
                usage: usage_of(&message),
                model: model.clone(),
                round,
            });
            self.push_message(
                hub,
                json!({"role": "assistant", "content": message["content"].clone()}),
            );

            let calls: Vec<Value> = message["content"]
                .as_array()
                .map(|a| a.iter().filter(|b| b["type"] == "tool_use").cloned().collect())
                .unwrap_or_default();
            if calls.is_empty() {
                return;
            }

            let mut results: Vec<Value> = Vec::new();
            for call in calls {
                results.push(self.run_tool(hub, &call));
            }
            self.push_message(hub, json!({"role": "user", "content": results}));
        }
        hub.emit(events::Error {
            text: format!("gave up: more than {MAX_ROUNDS} tool rounds in one turn"),
        });
    }

    fn run_tool(&mut self, hub: &Hub, call: &Value) -> Value {
        let id = call["id"].as_str().unwrap_or_default().to_string();
        let name = call["name"].as_str().unwrap_or_default().to_string();
        let input = call["input"].to_string();
        hub.emit(events::ToolStart {
            brief: brief(&call["input"]),
            name: name.clone(),
        });
        let t0 = Instant::now();
        let out = hub.tools().run(&name, &input);
        let secs = t0.elapsed().as_secs_f64();
        let ok = out.is_ok();
        if !ok && min_core::is_cancelled() {
            // The tool was killed because the user cancelled the turn.
            // The turn ends here; there is no result to feed back.
            hub.emit(events::TurnCancelled {});
            return json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": "cancelled",
                "is_error": true,
            });
        }
        let payload = out.unwrap_or_else(|e| format!("error: {e}"));
        let note: String = payload
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(100)
            .collect();
        hub.emit(events::ToolEnd { ok, secs, note, preview: preview(&payload) });
        json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": payload,
            "is_error": !ok,
        })
    }
}

/// The token usage a normalized LLM response carried, if any.
fn usage_of(message: &Value) -> min_core::usage::TokenUsage {
    let u = &message["usage"];
    if u.is_null() {
        return min_core::usage::TokenUsage::default();
    }
    min_core::usage::TokenUsage {
        input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: u["cache_read_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: u["cache_write_tokens"].as_u64().unwrap_or(0),
    }
}

/// One glanceable line describing a tool call, for the UI.
fn brief(input: &Value) -> String {
    if let Some(c) = input["command"].as_str() {
        c.lines().next().unwrap_or("").chars().take(80).collect()
    } else if let Some(p) = input["path"].as_str() {
        p.chars().take(80).collect()
    } else {
        String::new()
    }
}

fn build_system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let s = format!(
        r#"You are min — a minimal agentic coding assistant running in a terminal.

Environment:
- cwd: {cwd}
- platform: {os}/{arch}
- date: {date}

Working style:
- Look before you edit: read files (or grep/find via bash) before changing them.
- The edit tool replaces exact text that must match exactly one place — include enough context to make each old_text unique. Edits apply in order.
- Prefer small, surgical changes.
- Be concise. No preamble."#,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        date = utc_date(),
    );
    s
}

/// First lines of a tool result, for the terminal preview after `✓`.
/// Skips blank lines so the first lines are never blank, caps at 5 lines /
/// 120 columns each (a wrapped line floods the transcript, not helps), and
/// cuts on a character boundary — a byte-cut would tear a multi-byte char.
fn preview(payload: &str) -> String {
    const LINES: usize = 5;
    const COLS: usize = 120;
    let mut out: Vec<&str> = Vec::new();
    for line in payload.lines().filter(|l| !l.trim().is_empty()) {
        let end = line
            .char_indices()
            .nth(COLS)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        out.push(&line[..end]);
        if out.len() == LINES {
            break;
        }
    }
    out.join("\n")
}

/// UTC date from the epoch — avoids a datetime dependency.
fn utc_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (y, m, d) = civil_from_days(secs / 86400);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - 365 * yoe - yoe / 4 + yoe / 100;
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use min_core::Hub;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn preview_takes_first_five_nonblank_lines() {
        let p = preview("a\n\nb\n\nc\nd\ne\nf\ng");
        assert_eq!(p, "a\nb\nc\nd\ne");
        assert_eq!(preview("\n\n  \nonly"), "only");
        assert_eq!(preview(""), "");
    }

    #[test]
    fn preview_caps_line_width_and_is_utf8_safe() {
        let long = "x".repeat(300);
        assert_eq!(preview(&long).chars().count(), 120);
        let mb = "é".repeat(200); // 2 bytes each; a byte-cut would tear one
        assert_eq!(preview(&mb).chars().count(), 120);
    }

    #[test]
    fn prompt_sections_arrive_via_events_not_file_reads() {
        let mut agent = AgentPlugin::new();
        let hub = Hub::new(vec![]);
        assert!(agent.sections.is_empty());
        assert!(!agent.system.contains("Project instructions"));

        // What min-context does in start(): emit, then the core delivers.
        agent.handle(
            &hub,
            &events::SystemPromptSection {
                title: "Project instructions (AGENTS.md)".into(),
                body: "rule one".into(),
            },
        );
        agent.handle(
            &hub,
            &events::UserInput { text: "hi".into() }, // unrelated events ignored
        );
        assert_eq!(agent.sections.len(), 1);
        assert_eq!(agent.sections[0], (
            "Project instructions (AGENTS.md)".into(),
            "rule one".into(),
        ));
    }

    #[test]
    fn clear_prompt_sections_drops_old_ones_so_reemitted_files_replace_them() {
        let mut agent = AgentPlugin::new();
        let hub = Hub::new(vec![]);
        agent.handle(
            &hub,
            &events::SystemPromptSection {
                title: "Project instructions (AGENTS.md)".into(),
                body: "old rules".into(),
            },
        );

        agent.handle(&hub, &events::ClearPromptSections {});
        assert!(agent.sections.is_empty());

        agent.handle(
            &hub,
            &events::SystemPromptSection {
                title: "Project instructions (AGENTS.md)".into(),
                body: "new rules".into(),
            },
        );
        assert_eq!(agent.sections, vec![(
            "Project instructions (AGENTS.md)".into(),
            "new rules".into(),
        )]);
    }

    /// A fake provider that answers with a fixed message and a fixed
    /// usage report — the end-to-end path from the wire to the bus.
    struct FakeProvider {
        message: String,
        usage: Value,
    }

    impl min_core::llm::ChatProvider for FakeProvider {
        fn can_handle(&self, _model: &str) -> bool {
            true
        }
        fn model(&self) -> &str {
            "test-model"
        }
        fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages_json: &str,
            _tools_json: &str,
            _callbacks: min_core::llm::Callbacks<'_>,
        ) -> Result<String, String> {
            Ok(json!({
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": self.message}],
                "usage": self.usage,
            })
            .to_string())
        }
    }

    /// A provider that fails the way a cancelled one does.
    struct CancelledProvider;

    impl min_core::llm::ChatProvider for CancelledProvider {
        fn can_handle(&self, _model: &str) -> bool {
            true
        }
        fn model(&self) -> &str {
            "test-model"
        }
        fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages_json: &str,
            _tools_json: &str,
            _callbacks: min_core::llm::Callbacks<'_>,
        ) -> Result<String, String> {
            // What min-llm returns when the user hits ctrl-c.
            Err("cancelled".into())
        }
    }

    /// Records which of the turn-ending events the agent emitted.
    struct OutcomeRecorder {
        cancelled: Rc<RefCell<bool>>,
        error: Rc<RefCell<bool>>,
    }

    impl Plugin for OutcomeRecorder {
        fn name(&self) -> &'static str {
            "outcome-rec"
        }
        fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
            if event.downcast_ref::<events::TurnCancelled>().is_some() {
                *self.cancelled.borrow_mut() = true;
            } else if let Some(e) = event.downcast_ref::<events::Error>() {
                *self.error.borrow_mut() = true;
                assert_ne!(e.text, "cancelled", "a cancelled turn is not an error");
            }
        }
    }

    /// A cancelled turn must be reported as a cancellation, not as an
    /// error: the repl shows it plainly, and herdr treats min as idle
    /// rather than blocked.
    #[test]
    fn a_cancelled_turn_emits_turn_cancelled_not_an_error() {
        let cancelled = Rc::new(RefCell::new(false));
        let error = Rc::new(RefCell::new(false));
        let hub = Hub::new(vec![Box::new(OutcomeRecorder {
            cancelled: cancelled.clone(),
            error: error.clone(),
        })]);
        let router = min_core::llm::LlmRouter::default();
        router.register(Box::new(CancelledProvider));
        hub.set_service(router);

        // The user hit ctrl-c: the flag is set for the whole process.
        min_core::cancel();

        let mut agent = AgentPlugin::new();
        agent.handle(&hub, &events::UserInput { text: "hi".into() });

        min_core::reset_cancel();

        assert!(*cancelled.borrow(), "the turn was reported as cancelled");
        assert!(!*error.borrow(), "a cancelled turn is not an error");
    }

    /// A plugin that records the usage events the agent emits.
    struct UsageRecorder(Rc<RefCell<Vec<events::Usage>>>);

    impl Plugin for UsageRecorder {
        fn name(&self) -> &'static str {
            "usage-rec"
        }
        fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
            if let Some(e) = event.downcast_ref::<events::Usage>() {
                self.0.borrow_mut().push(events::Usage {
                    usage: e.usage,
                    model: e.model.clone(),
                    round: e.round,
                });
            }
        }
    }

    /// The whole path: the agent calls the LLM, the provider's usage
    /// report comes back, and the agent puts it on the bus.
    #[test]
    fn a_round_reports_its_token_usage() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(UsageRecorder(seen.clone()))]);
        let router = min_core::llm::LlmRouter::default();
        router.register(Box::new(FakeProvider {
            message: "hello".into(),
            usage: json!({
                "input_tokens": 120,
                "output_tokens": 8,
                "cache_read_tokens": 30,
                "cache_write_tokens": 0,
            }),
        }));
        hub.set_service(router);

        let mut agent = AgentPlugin::new();
        agent.handle(&hub, &events::UserInput { text: "hi".into() });

        let seen = seen.borrow();
        assert_eq!(seen.len(), 1, "one round, one usage event");
        assert_eq!(seen[0].usage.input_tokens, 120);
        assert_eq!(seen[0].usage.output_tokens, 8);
        assert_eq!(seen[0].usage.cache_read_tokens, 30);
        assert_eq!(seen[0].usage.prompt_tokens(), 150);
        assert_eq!(seen[0].round, 1);
    }

    /// The model the session log records must be the model that was
    /// actually requested. The provider owns the model name — it resolves
    /// MIN_MODEL against the endpoint's default — so the agent asks the
    /// router rather than naming a model of its own.
    #[test]
    fn the_reported_model_is_the_one_the_provider_will_use() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(UsageRecorder(seen.clone()))]);
        let router = min_core::llm::LlmRouter::default();
        router.register(Box::new(FakeProvider {
            message: "hello".into(),
            usage: json!({}),
        }));
        hub.set_service(router);

        let mut agent = AgentPlugin::new();
        agent.handle(&hub, &events::UserInput { text: "hi".into() });

        let seen = seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].model, "test-model",
            "the recorded model is the provider's, not one the agent made up"
        );
    }

    /// A provider that reports nothing must produce an empty usage event,
    /// not a zero-cost one.
    #[test]
    fn a_round_without_usage_reports_empty_usage() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(UsageRecorder(seen.clone()))]);
        let router = min_core::llm::LlmRouter::default();
        router.register(Box::new(FakeProvider {
            message: "hello".into(),
            usage: json!({}),
        }));
        hub.set_service(router);

        let mut agent = AgentPlugin::new();
        agent.handle(&hub, &events::UserInput { text: "hi".into() });

        let seen = seen.borrow();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].usage.is_empty());
    }

    #[test]
    fn push_message_emits_conversation_message_and_replay_restores_messages() {
        struct MsgRecorder(Rc<RefCell<Vec<String>>>);
        impl Plugin for MsgRecorder {
            fn name(&self) -> &'static str {
                "msg-rec"
            }
            fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
                if let Some(e) = event.downcast_ref::<events::ConversationMessage>() {
                    self.0.borrow_mut().push(e.json.clone());
                }
            }
        }

        let recorded = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(MsgRecorder(recorded.clone()))]);
        let mut agent = AgentPlugin::new();

        // ReplayMessage restores conversation history without re-emitting ConversationMessage.
        agent.handle(
            &hub,
            &events::ReplayMessage {
                json: json!({"role": "user", "content": "first"}).to_string(),
            },
        );
        agent.handle(
            &hub,
            &events::ReplayMessage {
                json: json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}).to_string(),
            },
        );
        assert_eq!(agent.messages.len(), 2);
        assert!(recorded.borrow().is_empty());

        // push_message appends to self.messages AND emits ConversationMessage.
        agent.push_message(&hub, json!({"role": "user", "content": "second"}));
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(recorded.borrow().len(), 1);
        let emitted: Value = serde_json::from_str(&recorded.borrow()[0]).unwrap();
        assert_eq!(emitted, json!({"role": "user", "content": "second"}));
    }
}
