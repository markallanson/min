//! min-context — project context for the system prompt.
//!
//! Reads project instruction files from the current directory (currently
//! `AGENTS.md`) at startup and emits them as [`events::SystemPromptSection`]
//! events for the agent to append to its base system prompt. No file, no
//! sections, nothing happens — the agent is unaffected.

use min_core::{events, Hub, Plugin};

/// Files to look for, in order; all matches are appended (later files first
/// would overwrite nothing — every hit becomes its own section).
const FILES: &[&str] = &["AGENTS.md"];

pub struct ContextPlugin;

impl Plugin for ContextPlugin {
    fn name(&self) -> &'static str {
        "context"
    }

    fn start(&mut self, hub: &Hub) {
        for file in FILES {
            if let Ok(md) = std::fs::read_to_string(file) {
                hub.emit(events::SystemPromptSection {
                    title: format!("Project instructions ({file})"),
                    body: md,
                });
            }
        }
    }
}
