//! min-session-log — an append-only log of session activity.
//!
//! One file per session, named `<session id>.log`, under the XDG state
//! directory — `$XDG_STATE_HOME/min/sessions`, default
//! `~/.local/state/min/sessions`. Created the moment a session is announced
//! ([`events::SessionStart`]); appended to as the session unfolds; never
//! rewritten — a later release can resume a session by reading it back.
//!
//! ## Why a listener
//!
//! This is the third plugin shape from `docs/plugins.md`: a pure listener.
//! It introduces no events, offers no tools, publishes no services — every
//! fact it records is already broadcast by another plugin. The event →
//! entry mapping, kept deliberately exhaustive (anything that helps a future
//! resume), mirrors the repl's own display:
//!
//! | event | log entry |
//! |---|---|
//! | `SessionStart` | session header (id, parent, cwd, wire format) |
//! | `UserInput` | `prompt:` |
//! | `AssistantDelta` | buffered; flushed as `assistant:` on `MessageEnd` |
//! | `ToolStart` / `ToolEnd` | `tool:` lines (the full model→tool input) |
//! | `Info` | `info:` |
//! | `Error` | `error:` |
//! | `SystemPromptSection` | `instructions:` |
//! | `SessionEnd` | session footer (closing timestamp) |
//!
//! Prompt, assistant and tool text are JSON-encoded on the way in, so the
//! log stays one-entry-per-line no matter what newlines run through it.
//!
//! ## Robustness
//!
//! Logging must never disturb a session. If the state directory cannot be
//! created or the file cannot be opened, this plugin degrades to a no-op
//! (the failure is announced once, via `Error`) and drops everything after
//! it. Each `SessionStart` retries the setup, so a full disk that frees up
//! mid-run gets picked up at the next `/clear`.

use min_core::{events, Hub, Plugin};
use std::any::Any;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SessionLogPlugin {
    /// The open log file; `None` means "not logging" (before the first
    /// session, or after a setup failure — retried on the next one).
    file: Option<File>,
    /// Buffered assistant deltas — flushed to the log when the message ends.
    assistant: String,
    /// Where to log, instead of the XDG state dir. A test seam: tests point
    /// this at a scratch directory so they never touch the environment
    /// (which is process-global, and this crate's tests run in parallel).
    dir: Option<std::path::PathBuf>,
}

impl SessionLogPlugin {
    pub fn new() -> Self {
        SessionLogPlugin {
            file: None,
            assistant: String::new(),
            dir: None,
        }
    }

    /// Begin logging a session: open the file and announce the header.
    /// Any earlier failure is forgiven — every new session gets a fresh
    /// attempt at the state directory.
    fn start_session(&mut self, hub: &Hub, id: u64, parent: Option<u64>) {
        let result = self
            .dir
            .clone()
            .or_else(state_dir)
            .ok_or_else(|| "no state directory (set XDG_STATE_HOME or HOME)".to_string())
            .and_then(|dir| {
                let path = dir.join(format!("{id}.log"));
                let parent_dir = path.parent().expect("log path has a parent").to_path_buf();
                std::fs::create_dir_all(&parent_dir)
                    .map(|_| path)
                    .map_err(|e| format!("create {}: {e}", parent_dir.display()))
            });
        let path = match result {
            Ok(p) => p,
            Err(e) => {
                self.file = None;
                announce_failure(hub, &e);
                return;
            }
        };
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                self.file = Some(file);
                self.assistant.clear();
                let _ = writeln!(
                    self.file.as_mut().expect("just set"),
                    "{}",
                    header(id, parent)
                );
            }
            Err(e) => {
                self.file = None;
                announce_failure(hub, &format!("open {}: {e}", path.display()));
            }
        }
    }

    /// Write one log line. Silent on I/O failure — a broken log must not
    /// break the session it is recording.
    fn write_line(&mut self, line: &str) {
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }
}

impl Default for SessionLogPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SessionLogPlugin {
    fn name(&self) -> &'static str {
        "session-log"
    }

    fn start(&mut self, _hub: &Hub) {}

    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if let Some(e) = event.downcast_ref::<events::SessionStart>() {
            // A new session begins. Whether or not this plugin ever saw the
            // matching end — min killed hard, say — the new id means a new
            // file; nothing of the old session can be clobbered.
            self.start_session(hub, e.id, e.parent);
        } else if let Some(e) = event.downcast_ref::<events::UserInput>() {
            self.write_line(&format!("prompt: {}", json_escape(&e.text)));
        } else if let Some(e) = event.downcast_ref::<events::AssistantDelta>() {
            // Streamed in pieces; a complete message is logged, not chunks.
            self.assistant.push_str(&e.text);
        } else if event.downcast_ref::<events::MessageEnd>().is_some() {
            let text = std::mem::take(&mut self.assistant);
            self.write_line(&format!("assistant: {}", json_escape(&text)));
        } else if let Some(e) = event.downcast_ref::<events::ToolStart>() {
            self.write_line(&format!("tool: {} {}", e.name, json_escape(&e.brief)));
        } else if let Some(e) = event.downcast_ref::<events::ToolEnd>() {
            self.write_line(&format!(
                "tool_end: {} {}s {}",
                if e.ok { "ok" } else { "fail" },
                e.secs,
                json_escape(&e.note)
            ));
        } else if let Some(e) = event.downcast_ref::<events::Info>() {
            self.write_line(&format!("info: {}", json_escape(&e.text)));
        } else if let Some(e) = event.downcast_ref::<events::Error>() {
            self.write_line(&format!("error: {}", json_escape(&e.text)));
        } else if let Some(e) = event.downcast_ref::<events::SystemPromptSection>() {
            self.write_line(&format!(
                "instructions: {} {}",
                json_escape(&e.title),
                json_escape(&e.body)
            ));
        } else if event.downcast_ref::<events::SessionEnd>().is_some() {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            self.write_line(&format!("session_end: {secs}"));
            self.file = None;
            self.assistant.clear();
        }
    }
}

/// The directory this plugin logs into: `$XDG_STATE_HOME/min/sessions`,
/// or `~/.local/state/min/sessions` when `XDG_STATE_HOME` is unset or
/// empty (the XDG rule), or nothing at all when there is no home.
fn state_dir() -> Option<std::path::PathBuf> {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => Some(std::path::PathBuf::from(v)),
        _ => std::env::home_dir().map(|h| h.join(".local/state")),
    }?;
    Some(base.join("min").join("sessions"))
}

/// One JSON string, quotes and all — the only escaping needed to keep a
/// multi-line message on the one log line it belongs to.
fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The first line of a session's log: everything a future resume needs to
/// rebuild the stage.
fn header(id: u64, parent: Option<u64>) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    format!(
        "session_start: id={id} parent={} started={secs} cwd={}",
        parent.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
        json_escape(&cwd)
    )
}

/// Tell the user, once per failed session, that logging is off. The repl
/// owns the terminal — plugins announce, they do not print.
fn announce_failure(hub: &Hub, msg: &str) {
    hub.emit(events::Error {
        text: format!("session-log disabled: {msg}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("min-session-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A plugin logging into a scratch directory, via the test seam.
    fn plugin(name: &str) -> (SessionLogPlugin, std::path::PathBuf) {
        let dir = scratch(name);
        let mut p = SessionLogPlugin::new();
        p.dir = Some(dir.clone());
        (p, dir)
    }

    fn log_of(dir: &std::path::Path, id: u64) -> String {
        std::fs::read_to_string(dir.join(format!("{id}.log")))
            .unwrap_or_else(|e| panic!("read log: {e}"))
    }

    #[test]
    fn one_file_per_session_in_the_configured_directory() {
        let (mut p, dir) = plugin("dir");
        let hub = Hub::new(vec![]);
        p.handle(
            &hub,
            &events::SessionStart {
                id: 123,
                parent: Some(7),
            },
        );
        assert!(dir.join("123.log").is_file());
    }

    #[test]
    fn header_records_id_parent_cwd_and_started() {
        let (mut p, dir) = plugin("header");
        let hub = Hub::new(vec![]);
        p.handle(
            &hub,
            &events::SessionStart {
                id: 42,
                parent: Some(41),
            },
        );

        let line = log_of(&dir, 42);
        let line = line.trim_end();
        let cwd = json_escape(&std::env::current_dir().unwrap().display().to_string());
        assert!(
            line.starts_with("session_start: id=42 parent=41 started=")
                && line.ends_with(&format!("cwd={cwd}")),
            "unexpected header: {line}"
        );
        assert_eq!(line.lines().count(), 1, "header is one line: {line}");
    }

    #[test]
    fn events_become_the_documented_lines() {
        let (mut p, dir) = plugin("lines");
        let hub = Hub::new(vec![]);
        p.handle(
            &hub,
            &events::SessionStart {
                id: 5,
                parent: None,
            },
        );
        p.handle(
            &hub,
            &events::SystemPromptSection {
                title: "Project instructions".into(),
                body: "be brief".into(),
            },
        );
        p.handle(
            &hub,
            &events::UserInput {
                text: "hello\nworld".into(),
            },
        );
        p.handle(
            &hub,
            &events::Info {
                text: "thinking".into(),
            },
        );
        p.handle(
            &hub,
            &events::ToolStart {
                name: "read".into(),
                brief: "Cargo.toml".into(),
            },
        );
        p.handle(
            &hub,
            &events::ToolEnd {
                ok: true,
                secs: 0.01,
                note: "[package]".into(),
            },
        );
        p.handle(
            &hub,
            &events::Error {
                text: "boom".into(),
            },
        );
        p.handle(&hub, &events::SessionEnd { id: 5 });

        // The two timestamps and the (escaped) cwd vary; everything between
        // is exact. Header → entries → footer, in the documented order.
        let log = log_of(&dir, 5);
        let mut lines = log.lines();
        let header = lines.next().expect("header line");
        let (head, rest) = header.split_once("started=").expect("started=");
        let (started, cwd_part) = rest.split_once(' ').expect("cwd after started");
        assert_eq!(head, "session_start: id=5 parent=- ");
        assert!(
            started.parse::<u64>().is_ok(),
            "started={started} is a timestamp"
        );
        assert!(cwd_part.starts_with("cwd="), "{cwd_part}");

        for (line, expected) in lines.by_ref().zip([
            "instructions: \"Project instructions\" \"be brief\"",
            "prompt: \"hello\\nworld\"",
            "info: \"thinking\"",
            "tool: read \"Cargo.toml\"",
            "tool_end: ok 0.01s \"[package]\"",
            "error: \"boom\"",
        ]) {
            assert_eq!(line, expected);
        }
        let last = log.lines().last().unwrap();
        let (_pre, secs) = last.split_once("session_end: ").expect("footer");
        assert!(secs.parse::<u64>().is_ok(), "footer={secs} is a timestamp");
        assert_eq!(log.lines().count(), 8, "header + 6 entries + footer");
    }

    #[test]
    fn assistant_deltas_buffer_until_message_end() {
        let (mut p, dir) = plugin("assistant");
        let hub = Hub::new(vec![]);
        p.handle(
            &hub,
            &events::SessionStart {
                id: 9,
                parent: None,
            },
        );
        p.handle(&hub, &events::AssistantDelta { text: "hel".into() });
        p.handle(&hub, &events::AssistantDelta { text: "lo ".into() });
        assert_eq!(
            log_of(&dir, 9).lines().count(),
            1,
            "nothing logged mid-stream"
        );

        p.handle(&hub, &events::MessageEnd {});
        assert!(
            log_of(&dir, 9).contains("\nassistant: \"hello \"\n"),
            "message logged on end: {:?}",
            log_of(&dir, 9)
        );
    }

    #[test]
    fn each_new_session_gets_a_fresh_file_and_nothing_carries_over() {
        let (mut p, dir) = plugin("fresh");
        let hub = Hub::new(vec![]);
        p.handle(
            &hub,
            &events::SessionStart {
                id: 100,
                parent: None,
            },
        );
        p.handle(
            &hub,
            &events::UserInput {
                text: "first".into(),
            },
        );
        p.handle(&hub, &events::SessionEnd { id: 100 });

        p.handle(
            &hub,
            &events::SessionStart {
                id: 101,
                parent: None,
            },
        );
        assert_eq!(
            log_of(&dir, 101).lines().count(),
            1,
            "the new file starts with the header only"
        );

        // A hard kill that skips SessionEnd must not lose the next session:
        // the new id alone opens the new file.
        p.handle(
            &hub,
            &events::SessionStart {
                id: 102,
                parent: None,
            },
        );
        assert!(dir.join("102.log").is_file());
    }

    #[test]
    fn logging_survives_a_session_end_and_start_without_one() {
        let (mut p, dir) = plugin("resume-ready");
        let hub = Hub::new(vec![]);
        p.handle(
            &hub,
            &events::SessionStart {
                id: 200,
                parent: None,
            },
        );
        p.handle(
            &hub,
            &events::SessionStart {
                id: 201,
                parent: None,
            },
        );
        p.handle(
            &hub,
            &events::UserInput {
                text: "written to 201 only".into(),
            },
        );
        p.handle(&hub, &events::SessionEnd { id: 201 });
        assert!(log_of(&dir, 201).contains("written to 201 only"));
        assert_eq!(log_of(&dir, 200).lines().count(), 1);
    }

    /// `state_dir()` — the real resolver, unmocked: XDG_STATE_HOME wins;
    /// without it, `~/.local/state/min/sessions` under HOME.
    #[test]
    fn state_dir_prefers_xdg_and_falls_back_to_home() {
        let root = scratch("statedir");
        let prev_state = std::env::var("XDG_STATE_HOME").ok();
        let prev_home = std::env::var("HOME").ok();

        std::env::set_var("XDG_STATE_HOME", &root);
        assert_eq!(state_dir(), Some(root.join("min/sessions")));

        // XDG base-dir spec: an empty variable means unset.
        std::env::set_var("XDG_STATE_HOME", "");
        std::env::set_var("HOME", &root);
        assert_eq!(state_dir(), Some(root.join(".local/state/min/sessions")));

        match prev_state {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn json_escape_quotes_and_holds_one_line() {
        assert_eq!(json_escape("plain"), "\"plain\"");
        assert_eq!(json_escape("a\nb\"c"), "\"a\\nb\\\"c\"");
        assert!(!json_escape("a\nb").contains('\n'));
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}
