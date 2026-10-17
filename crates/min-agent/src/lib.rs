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
const MAX_ROUNDS: usize = 50;

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

impl Default for AgentPlugin {
    fn default() -> Self {
        Self::new()
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
        }
    }
}

impl AgentPlugin {
    fn turn(&mut self, hub: &Hub, user_text: &str) {
        // Base prompt plus any sections plugins handed us (refreshed on /clear).
        let mut system = self.system.clone();
        for (title, body) in &self.sections {
            system.push_str(&format!("\n\n# {title}\n\n{body}"));
        }
        self.messages.push(json!({"role": "user", "content": user_text}));
        for _ in 0..MAX_ROUNDS {
            let Some(llm) = hub.service::<min_llm::Llm>() else {
                hub.emit(events::Error {
                    text: "no LLM service — is the llm plugin loaded?".into(),
                });
                return;
            };
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
            let mut on_delta = |t: &str| {
                hub.emit(events::AssistantDelta { text: t.to_string() });
            };
            let mut on_status = |s: &str| {
                hub.emit(events::Info { text: s.to_string() });
            };
            let message = match llm.complete(&system, &self.messages, &tools, &mut on_delta, &mut on_status) {
                Ok(m) => m,
                Err(e) => {
                    hub.emit(events::Error { text: e });
                    return;
                }
            };
            hub.emit(events::MessageEnd {});
            self.messages
                .push(json!({"role": "assistant", "content": message["content"].clone()}));

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
            self.messages
                .push(json!({"role": "user", "content": results}));
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
            brief: brief(&name, &call["input"]),
            name: name.clone(),
        });
        let t0 = Instant::now();
        let out = hub.tools().run(&name, &input);
        let secs = t0.elapsed().as_secs_f64();
        let ok = out.is_ok();
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

/// One glanceable line describing a tool call, for the UI.
fn brief(_name: &str, input: &Value) -> String {
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
        r#"You are min — a minimal agentic coding assistant running in a terminal, built as plugins on a tiny core. You may be asked to work on min's own source code.

Environment:
- cwd: {cwd}
- platform: {os}/{arch}
- date: {date}

Working style:
- Look before you edit: read files (or grep/find via bash) before changing them.
- The edit tool replaces exact text that must match exactly one place — include enough context to make each old_text unique. Edits apply in order.
- Prefer small, surgical changes. After changing Rust code, verify with `cargo build` and `cargo test` (bash tool).
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
}
