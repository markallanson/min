//! min-session — session identity, JSONL persistence, listing, and resume.
//!
//! Owns the full session lifecycle:
//! 1. **Identity:** mints `<epoch seconds>-<4 random alphanumeric chars>` IDs
//!    (or uses a pinned/resumed ID) and announces [`events::SessionStart`]
//!    from the first `wait` so every plugin receives it.
//! 2. **JSONL Persistence:** writes one append-only `<id>.jsonl` file per
//!    session under `$XDG_STATE_HOME/min/sessions/<project>/<id>.jsonl`
//!    (default `~/.local/state/min/sessions/<project>/<id>.jsonl`).
//! 3. **Resume:** on `min --resume [<id>]` (or `min --session <id>` when
//!    `<id>.jsonl` already exists), reads back the conversation from
//!    `<id>.jsonl`, sanitizes any interrupted trailing state, replays
//!    [`events::ReplayMessage`] and [`events::ReplayExchange`] to the agent
//!    and repl, and continues appending to the same `<id>.jsonl`.
//! 4. **Listing:** [`list_sessions`] scans the project's session directory
//!    and formats root sessions newest-first for `min --sessions`.

use min_core::{events, Hub, Plugin};
use serde_json::{json, Value};
use std::any::Any;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How the initial session is chosen at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMode {
    /// Mint a fresh `<epoch>-<rand>` session (with optional `--parent-session`).
    Fresh { parent: Option<String> },
    /// Pin the first session's ID (`--session <id>`). If `<id>.jsonl` exists, resumes it.
    Pinned(String),
    /// Resume a specific session (`--resume <id>`) or the latest root session (`--resume`).
    Resume(Option<String>),
}

pub struct SessionPlugin {
    /// The open session, announced and not yet ended.
    current: Option<Session>,
    /// Startup mode, consumed on the first `wait` (subsequent `/clear`s mint fresh IDs).
    startup_mode: Option<SessionMode>,
    /// Whether the startup `wait` has already fired (so a failed resume does not spin).
    booted: bool,
    /// The open `.jsonl` file for the current session; `None` before the first session
    /// or if the state directory could not be opened.
    file: Option<File>,
    /// Prompt sections emitted before the first `SessionStart` (from `min-context`'s `start`),
    /// flushed to `<id>.jsonl` when a new session file is opened.
    pending_instructions: Vec<(String, String)>,
    /// Test seam: override the base sessions directory (`$XDG_STATE_HOME/min/sessions`).
    dir: Option<PathBuf>,
    /// Test seam: override the project slug directory name.
    project: Option<String>,
}

#[derive(Clone, Debug)]
struct Session {
    id: String,
    parent: Option<String>,
}

impl SessionPlugin {
    /// Legacy constructor used by existing callers/tests:
    /// `session_id` pins the first session (`--session`), `parent_id` sets
    /// its parent (`--parent-session`). Both `Some` drops both.
    pub fn new(session_id: Option<String>, parent_id: Option<String>) -> Self {
        let mode = match (session_id, parent_id) {
            (Some(_), Some(_)) => SessionMode::Fresh { parent: None },
            (Some(id), None) => SessionMode::Pinned(id),
            (None, parent) => SessionMode::Fresh { parent },
        };
        Self::with_mode(mode)
    }

    /// Construct with an explicit [`SessionMode`].
    pub fn with_mode(mode: SessionMode) -> Self {
        SessionPlugin {
            current: None,
            startup_mode: Some(mode),
            booted: false,
            file: None,
            pending_instructions: Vec::new(),
            dir: None,
            project: None,
        }
    }

    fn fresh_id(&self) -> String {
        format!("{}-{}", secs_since_epoch(), entropy())
    }

    /// Resolve the project directory `<state_dir>/<project>`.
    fn project_dir(&self, cwd: &str) -> Result<PathBuf, String> {
        let project = self
            .project
            .clone()
            .unwrap_or_else(|| slug_project(cwd));
        let base = self
            .dir
            .clone()
            .or_else(state_dir)
            .ok_or_else(|| "no state directory (set XDG_STATE_HOME or HOME)".to_string())?;
        Ok(base.join(project))
    }

    fn open_session(&mut self, id: String, parent: Option<String>) -> events::SessionStart {
        let session = Session { id, parent };
        let event = events::SessionStart {
            id: session.id.clone(),
            parent: session.parent.clone(),
        };
        self.current = Some(session);
        event
    }

    fn close_session(&mut self) -> Option<events::SessionEnd> {
        let session = self.current.take()?;
        self.write_json(&json!({
            "type": "session_end",
            "ts": secs_since_epoch(),
        }));
        self.file = None;
        Some(events::SessionEnd { id: session.id })
    }

    /// Handle `SessionStart`: either resume an existing `<id>.jsonl` or start a new one.
    fn on_session_start(&mut self, hub: &Hub, id: &str, parent: Option<&str>) {
        let cwd = current_cwd();
        let proj_dir = match self.project_dir(&cwd) {
            Ok(d) => d,
            Err(e) => {
                self.file = None;
                announce_failure(hub, &e);
                return;
            }
        };
        if let Err(e) = std::fs::create_dir_all(&proj_dir) {
            self.file = None;
            announce_failure(hub, &format!("create {}: {e}", proj_dir.display()));
            return;
        }
        let path = proj_dir.join(format!("{id}.jsonl"));
        if path.is_file() {
            self.resume_existing(hub, id, &path, &cwd);
        } else {
            self.start_fresh_file(hub, id, parent, &path, &cwd);
        }
    }

    fn start_fresh_file(
        &mut self,
        hub: &Hub,
        id: &str,
        parent: Option<&str>,
        path: &Path,
        cwd: &str,
    ) {
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                self.file = Some(file);
                self.write_json(&json!({
                    "type": "session_start",
                    "id": id,
                    "parent": parent,
                    "ts": secs_since_epoch(),
                    "cwd": cwd,
                }));
                for (title, body) in std::mem::take(&mut self.pending_instructions) {
                    self.write_json(&json!({
                        "type": "instructions",
                        "title": title,
                        "body": body,
                    }));
                }
            }
            Err(e) => {
                self.file = None;
                announce_failure(hub, &format!("open {}: {e}", path.display()));
            }
        }
    }

    fn resume_existing(&mut self, hub: &Hub, id: &str, path: &Path, cwd: &str) {
        self.pending_instructions.clear();
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                self.file = None;
                announce_failure(hub, &format!("read {}: {e}", path.display()));
                return;
            }
        };

        let mut messages: Vec<Value> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if v["type"] == "message" {
                    messages.push(json!({
                        "role": v["role"].clone(),
                        "content": v["content"].clone(),
                    }));
                }
            }
        }

        let synthesized = sanitize_trailing(&mut messages);

        for msg in &messages {
            hub.emit(events::ReplayMessage {
                json: msg.to_string(),
            });
        }

        if let Some((prompt, reply)) = last_exchange(&messages) {
            hub.emit(events::ReplayExchange { prompt, reply });
        }

        hub.emit(events::Info {
            text: format!("resumed {id} ({} messages)", messages.len()),
        });

        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                self.file = Some(file);
                for msg in synthesized {
                    self.write_json(&json!({
                        "type": "message",
                        "role": msg["role"],
                        "content": msg["content"],
                    }));
                }
                self.write_json(&json!({
                    "type": "session_resume",
                    "id": id,
                    "ts": secs_since_epoch(),
                    "cwd": cwd,
                }));
            }
            Err(e) => {
                self.file = None;
                announce_failure(hub, &format!("open {}: {e}", path.display()));
            }
        }
    }

    fn write_json(&mut self, value: &Value) {
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{value}");
        }
    }
}

impl Plugin for SessionPlugin {
    fn name(&self) -> &'static str {
        "session"
    }

    fn start(&mut self, _hub: &Hub) {}

    fn wait(&mut self, hub: &Hub) -> Option<Box<dyn Any>> {
        if self.booted {
            return None;
        }
        self.booted = true;
        let mode = self
            .startup_mode
            .take()
            .unwrap_or(SessionMode::Fresh { parent: None });

        match mode {
            SessionMode::Fresh { parent } => {
                let id = self.fresh_id();
                Some(Box::new(self.open_session(id, parent)))
            }
            SessionMode::Pinned(id) => Some(Box::new(self.open_session(id, None))),
            SessionMode::Resume(target) => {
                let cwd = current_cwd();
                let proj_dir = match self.project_dir(&cwd) {
                    Ok(d) => d,
                    Err(e) => {
                        hub.emit(events::Error { text: e });
                        return Some(Box::new(events::Quit {}));
                    }
                };
                match resolve_resume_target(&proj_dir, target.as_deref()) {
                    Ok((id, parent)) => Some(Box::new(self.open_session(id, parent))),
                    Err(e) => {
                        hub.emit(events::Error { text: e });
                        Some(Box::new(events::Quit {}))
                    }
                }
            }
        }
    }

    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if let Some(e) = event.downcast_ref::<events::SessionStart>() {
            self.on_session_start(hub, &e.id, e.parent.as_deref());
        } else if event.downcast_ref::<events::ClearPromptSections>().is_some() {
            self.pending_instructions.clear();
        } else if let Some(e) = event.downcast_ref::<events::SystemPromptSection>() {
            if self.file.is_some() {
                self.write_json(&json!({
                    "type": "instructions",
                    "title": e.title,
                    "body": e.body,
                }));
            } else {
                self.pending_instructions
                    .push((e.title.clone(), e.body.clone()));
            }
        } else if let Some(e) = event.downcast_ref::<events::ConversationMessage>() {
            if let Ok(msg) = serde_json::from_str::<Value>(&e.json) {
                self.write_json(&json!({
                    "type": "message",
                    "role": msg["role"],
                    "content": msg["content"],
                }));
            }
        } else if let Some(e) = event.downcast_ref::<events::ToolStart>() {
            self.write_json(&json!({
                "type": "tool",
                "name": e.name,
                "brief": e.brief,
            }));
        } else if let Some(e) = event.downcast_ref::<events::ToolEnd>() {
            self.write_json(&json!({
                "type": "tool_end",
                "ok": e.ok,
                "secs": e.secs,
                "note": e.note,
            }));
        } else if let Some(e) = event.downcast_ref::<events::Info>() {
            self.write_json(&json!({
                "type": "info",
                "text": e.text,
            }));
        } else if let Some(e) = event.downcast_ref::<events::Error>() {
            self.write_json(&json!({
                "type": "error",
                "text": e.text,
            }));
        } else if event.downcast_ref::<events::TurnCancelled>().is_some() {
            self.write_json(&json!({
                "type": "info",
                "text": "turn cancelled by user",
            }));
        } else if event.downcast_ref::<events::SessionEnd>().is_some() {
            if self.file.is_some() {
                self.write_json(&json!({
                    "type": "session_end",
                    "ts": secs_since_epoch(),
                }));
                self.file = None;
            }
        } else if event.downcast_ref::<events::ResetConversation>().is_some() {
            let id = self.fresh_id();
            if let Some(end) = self.close_session() {
                hub.emit(end);
            }
            let start = self.open_session(id, None);
            let start_id = start.id.clone();
            hub.emit(start);
            self.on_session_start(hub, &start_id, None);
        } else if event.downcast_ref::<events::Quit>().is_some() {
            if let Some(end) = self.close_session() {
                hub.emit(end);
            }
        }
    }
}

/// Sanitize interrupted trailing state in `messages`. Returns any synthesized
/// messages that were appended so the caller can persist them to `.jsonl`.
fn sanitize_trailing(messages: &mut Vec<Value>) -> Vec<Value> {
    let mut added = Vec::new();
    let Some(last) = messages.last() else {
        return added;
    };
    if last["role"] == "assistant" {
        let tool_ids: Vec<String> = last["content"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|b| b["type"] == "tool_use")
                    .filter_map(|b| b["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !tool_ids.is_empty() {
            let results: Vec<Value> = tool_ids
                .into_iter()
                .map(|id| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": "error: session interrupted before tool completed",
                        "is_error": true,
                    })
                })
                .collect();
            let synth = json!({
                "role": "user",
                "content": results,
            });
            messages.push(synth.clone());
            added.push(synth);
        }
    } else if last["role"] == "user" && last["content"].is_string() {
        let synth = json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "(interrupted)"}],
        });
        messages.push(synth.clone());
        added.push(synth);
    }
    added
}

/// Extract the last user text prompt and the last assistant text reply.
fn last_exchange(messages: &[Value]) -> Option<(String, String)> {
    let last_user_idx = messages
        .iter()
        .rposition(|m| m["role"] == "user" && m["content"].is_string())?;
    let prompt = messages[last_user_idx]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let mut reply = String::new();
    for m in &messages[last_user_idx + 1..] {
        if m["role"] == "assistant" {
            if let Some(arr) = m["content"].as_array() {
                let text: String = arr
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    reply = text;
                }
            } else if let Some(s) = m["content"].as_str() {
                if !s.is_empty() {
                    reply = s.to_string();
                }
            }
        }
    }
    Some((prompt, reply))
}

#[derive(Clone, Debug)]
struct SessionSummary {
    id: String,
    parent: Option<String>,
    ts: u64,
    first_prompt: Option<String>,
}

fn parse_session_summary(path: &Path) -> Option<SessionSummary> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut id: Option<String> = None;
    let mut parent: Option<String> = None;
    let mut ts: u64 = 0;
    let mut first_prompt: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("session_start") if id.is_none() => {
                id = v["id"].as_str().map(String::from);
                parent = v["parent"].as_str().map(String::from);
                ts = v["ts"].as_u64().unwrap_or(0);
            }
            Some("message") if first_prompt.is_none() && v["role"] == "user" => {
                if let Some(s) = v["content"].as_str() {
                    first_prompt = Some(s.to_string());
                }
            }
            _ => {}
        }
        if id.is_some() && first_prompt.is_some() {
            break;
        }
    }

    let id = id.or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(String::from)
    })?;
    Some(SessionSummary {
        id,
        parent,
        ts,
        first_prompt,
    })
}

fn scan_summaries(proj_dir: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(proj_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") && path.is_file() {
            if let Some(sum) = parse_session_summary(&path) {
                out.push(sum);
            }
        }
    }
    out
}

fn resolve_resume_target(
    proj_dir: &Path,
    target: Option<&str>,
) -> Result<(String, Option<String>), String> {
    if let Some(id) = target {
        let path = proj_dir.join(format!("{id}.jsonl"));
        if !path.is_file() {
            return Err(format!("session not found: {id}"));
        }
        let parent = parse_session_summary(&path).and_then(|s| s.parent);
        return Ok((id.to_string(), parent));
    }

    let mut roots: Vec<SessionSummary> = scan_summaries(proj_dir)
        .into_iter()
        .filter(|s| s.parent.is_none())
        .collect();
    if roots.is_empty() {
        return Err("no sessions to resume in this project".into());
    }
    roots.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));
    let chosen = &roots[0];
    Ok((chosen.id.clone(), None))
}

/// List root sessions for the current project directory, newest first.
pub fn list_sessions() -> Result<String, String> {
    let cwd = current_cwd();
    let base =
        state_dir().ok_or_else(|| "no state directory (set XDG_STATE_HOME or HOME)".to_string())?;
    let proj_dir = base.join(slug_project(&cwd));
    Ok(format_sessions_in(&proj_dir, secs_since_epoch()))
}

fn format_sessions_in(proj_dir: &Path, now_secs: u64) -> String {
    let all = scan_summaries(proj_dir);
    let mut child_counts: HashMap<String, usize> = HashMap::new();
    for s in &all {
        if let Some(p) = &s.parent {
            *child_counts.entry(p.clone()).or_insert(0) += 1;
        }
    }

    let mut roots: Vec<SessionSummary> = all.into_iter().filter(|s| s.parent.is_none()).collect();
    if roots.is_empty() {
        return "no sessions".into();
    }
    roots.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));

    let mut lines = Vec::new();
    for s in roots {
        let age = format_age(now_secs.saturating_sub(s.ts));
        let prompt = s
            .first_prompt
            .as_deref()
            .unwrap_or("(empty)")
            .lines()
            .next()
            .unwrap_or("(empty)");
        let prompt_trunc: String = prompt.chars().take(60).collect();
        let prompt_quoted = serde_json::to_string(&prompt_trunc).unwrap_or_else(|_| "\"\"".into());
        let children = child_counts.get(&s.id).copied().unwrap_or(0);
        let suffix = match children {
            0 => String::new(),
            1 => " (+1 subagent)".into(),
            n => format!(" (+{n} subagents)"),
        };
        lines.push(format!("{:<16} {:<8} {}{}", s.id, age, prompt_quoted, suffix));
    }
    lines.join("\n")
}

fn format_age(delta_secs: u64) -> String {
    if delta_secs < 60 {
        format!("{delta_secs}s ago")
    } else if delta_secs < 3600 {
        format!("{}m ago", delta_secs / 60)
    } else if delta_secs < 86_400 {
        format!("{}h ago", delta_secs / 3600)
    } else {
        format!("{}d ago", delta_secs / 86_400)
    }
}

fn current_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into())
}

/// The project directory a session logs under: the slug of the basename of
/// its working directory — lowercased, every character outside
/// `[a-z0-9._-]` collapsed to `-`, capped at 32, `-` trimmed.
fn slug_project(cwd: &str) -> String {
    let name = Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    let mut slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '-',
        })
        .collect();
    slug.truncate(32);
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "root".into()
    } else {
        slug.into()
    }
}

/// The base directory this plugin stores sessions in: `$XDG_STATE_HOME/min/sessions`,
/// or `~/.local/state/min/sessions` when `XDG_STATE_HOME` is unset or empty.
fn state_dir() -> Option<PathBuf> {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => std::env::home_dir().map(|h| h.join(".local/state")),
    }?;
    Some(base.join("min").join("sessions"))
}

fn announce_failure(hub: &Hub, msg: &str) {
    hub.emit(events::Error {
        text: format!("session store disabled: {msg}"),
    });
}

fn secs_since_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Four random lowercase alphanumeric characters.
fn entropy() -> String {
    let mut bytes = [0u8; 4];
    if File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        bytes = nanos.to_le_bytes();
    }
    entropy_from(bytes)
}

fn entropy_from(bytes: [u8; 4]) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("min-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plugin_with_mode(name: &str, mode: SessionMode) -> (SessionPlugin, PathBuf) {
        let dir = scratch(name);
        let mut p = SessionPlugin::with_mode(mode);
        p.dir = Some(dir.clone());
        p.project = Some("proj".into());
        (p, dir)
    }

    fn plugin(name: &str) -> (SessionPlugin, PathBuf) {
        plugin_with_mode(name, SessionMode::Fresh { parent: None })
    }

    fn jsonl_of(dir: &Path, id: &str) -> Vec<Value> {
        let raw = std::fs::read_to_string(dir.join("proj").join(format!("{id}.jsonl")))
            .unwrap_or_else(|e| panic!("read {id}.jsonl: {e}"));
        raw.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap_or_else(|e| panic!("bad json {l}: {e}")))
            .collect()
    }

    struct Recorder(Rc<RefCell<Vec<String>>>);
    impl Plugin for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
            if let Some(e) = event.downcast_ref::<events::SessionStart>() {
                self.0.borrow_mut().push(format!(
                    "start:{}:{}",
                    e.id,
                    e.parent.as_deref().unwrap_or("-")
                ));
            } else if let Some(e) = event.downcast_ref::<events::SessionEnd>() {
                self.0.borrow_mut().push(format!("end:{}", e.id));
            } else if let Some(e) = event.downcast_ref::<events::ReplayMessage>() {
                self.0.borrow_mut().push(format!("replay_msg:{}", e.json));
            } else if let Some(e) = event.downcast_ref::<events::ReplayExchange>() {
                self.0
                    .borrow_mut()
                    .push(format!("replay_ex:{}:{}", e.prompt, e.reply));
            } else if let Some(e) = event.downcast_ref::<events::Info>() {
                self.0.borrow_mut().push(format!("info:{}", e.text));
            } else if let Some(e) = event.downcast_ref::<events::Error>() {
                self.0.borrow_mut().push(format!("error:{}", e.text));
            } else if event.downcast_ref::<events::Quit>().is_some() {
                self.0.borrow_mut().push("quit".into());
            }
        }
    }

    #[test]
    fn first_wait_announces_a_fresh_session_to_everyone() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let (mut p, dir) = plugin("first-wait");
        assert!(log.borrow().is_empty(), "silent through start");

        let event = p.wait(&hub).expect("session announces on first wait");
        let e = event.downcast_ref::<events::SessionStart>().unwrap();
        let start_ev = events::SessionStart {
            id: e.id.clone(),
            parent: e.parent.clone(),
        };
        p.handle(&hub, &start_ev);
        hub.emit(start_ev);

        let logged = log.borrow();
        assert_eq!(logged.len(), 1, "one announcement");
        let id = logged[0]
            .strip_prefix("start:")
            .and_then(|s| s.strip_suffix(":-"))
            .expect("start:<id>:- with no parent");
        assert!(
            id.split_once('-')
                .map(|(secs, suf)| secs.parse::<u64>().is_ok() && suf.len() == 4)
                .unwrap_or(false),
            "id is <epoch second>-<4 char suffix>: {id}"
        );
        assert!(dir.join("proj").join(format!("{id}.jsonl")).is_file());
        assert!(p.wait(&hub).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinned_id_is_used_verbatim_for_the_first_session_only() {
        let (mut p, dir) = plugin_with_mode("pinned", SessionMode::Pinned("1000000-clip".into()));
        let hub = Hub::new(vec![]);
        let ev = p.wait(&hub).unwrap();
        let start = ev.downcast_ref::<events::SessionStart>().unwrap();
        assert_eq!(start.id, "1000000-clip");

        p.handle(&hub, &events::ResetConversation {});
        let second = p.current.as_ref().unwrap().id.clone();
        assert_ne!(second, "1000000-clip");
        assert!(second
            .split_once('-')
            .map(|(secs, _)| secs.parse::<u64>().is_ok())
            .unwrap_or(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_switches_in_new_drops_both() {
        let p = SessionPlugin::new(Some("42-even".into()), Some("7-hole".into()));
        assert_eq!(
            p.startup_mode,
            Some(SessionMode::Fresh { parent: None }),
            "session id and parent together → both cleared"
        );
    }

    #[test]
    fn parent_is_taken_by_the_first_session_only() {
        let (mut p, dir) = plugin_with_mode(
            "parent",
            SessionMode::Fresh {
                parent: Some("7-dads".into()),
            },
        );
        let hub = Hub::new(vec![]);
        let ev = p.wait(&hub).unwrap();
        let first = ev.downcast_ref::<events::SessionStart>().unwrap();
        assert_eq!(first.parent.as_deref(), Some("7-dads"));

        p.handle(&hub, &events::ResetConversation {});
        assert_eq!(p.current.as_ref().unwrap().parent, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_closes_then_opens_quit_closes() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let (mut p, dir) = plugin("clear-quit");
        let event = p.wait(&hub).expect("first session announced from wait");
        let e = event.downcast_ref::<events::SessionStart>().unwrap();
        let start = events::SessionStart {
            id: e.id.clone(),
            parent: e.parent.clone(),
        };
        p.handle(&hub, &start);
        hub.emit(start);
        let first = p.current.clone().unwrap().id;

        p.handle(&hub, &events::ResetConversation {});
        let second = p.current.clone().unwrap().id;
        assert_ne!(second, first);

        p.handle(&hub, &events::Quit {});
        assert!(p.current.is_none(), "session closed on Quit");

        let events = log.borrow();
        assert_eq!(events.len(), 4, "start/end/start/end");
        assert_eq!(events[0], format!("start:{first}:-"));
        assert_eq!(events[1], format!("end:{first}"));
        assert_eq!(events[2], format!("start:{second}:-"));
        assert_eq!(events[3], format!("end:{second}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jsonl_records_all_event_types() {
        let (mut p, dir) = plugin("events");
        let hub = Hub::new(vec![]);
        p.handle(
            &hub,
            &events::SessionStart {
                id: "5-five".into(),
                parent: Some("4-four".into()),
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
            &events::ConversationMessage {
                json: json!({"role": "user", "content": "hello\nworld"}).to_string(),
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
                preview: "line one".into(),
            },
        );
        p.handle(
            &hub,
            &events::ConversationMessage {
                json: json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}).to_string(),
            },
        );
        p.handle(
            &hub,
            &events::Error {
                text: "boom".into(),
            },
        );
        p.handle(&hub, &events::SessionEnd { id: "5-five".into() });

        let lines = jsonl_of(&dir, "5-five");
        assert_eq!(lines.len(), 9);
        assert_eq!(lines[0]["type"], "session_start");
        assert_eq!(lines[0]["id"], "5-five");
        assert_eq!(lines[0]["parent"], "4-four");
        assert!(lines[0]["ts"].is_u64());
        assert!(lines[0]["cwd"].is_string());

        assert_eq!(
            lines[1],
            json!({"type": "instructions", "title": "Project instructions", "body": "be brief"})
        );
        assert_eq!(
            lines[2],
            json!({"type": "message", "role": "user", "content": "hello\nworld"})
        );
        assert_eq!(lines[3], json!({"type": "info", "text": "thinking"}));
        assert_eq!(
            lines[4],
            json!({"type": "tool", "name": "read", "brief": "Cargo.toml"})
        );
        assert_eq!(
            lines[5],
            json!({"type": "tool_end", "ok": true, "secs": 0.01, "note": "[package]"})
        );
        assert_eq!(
            lines[6],
            json!({"type": "message", "role": "assistant", "content": [{"type": "text", "text": "done"}]})
        );
        assert_eq!(lines[7], json!({"type": "error", "text": "boom"}));
        assert_eq!(lines[8]["type"], "session_end");
        assert!(lines[8]["ts"].is_u64());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_replays_messages_and_exchange_and_appends_session_resume() {
        let dir = scratch("resume-ok");
        // First run: create session 100-abcd and write a turn.
        {
            let mut p1 = SessionPlugin::with_mode(SessionMode::Pinned("100-abcd".into()));
            p1.dir = Some(dir.clone());
            p1.project = Some("proj".into());
            let hub = Hub::new(vec![]);
            let ev = p1.wait(&hub).unwrap();
            let start = ev.downcast_ref::<events::SessionStart>().unwrap();
            p1.handle(
                &hub,
                &events::SessionStart {
                    id: start.id.clone(),
                    parent: start.parent.clone(),
                },
            );
            p1.handle(
                &hub,
                &events::ConversationMessage {
                    json: json!({"role": "user", "content": "what is 2+2?"}).to_string(),
                },
            );
            p1.handle(
                &hub,
                &events::ConversationMessage {
                    json: json!({"role": "assistant", "content": [{"type": "text", "text": "4"}]}).to_string(),
                },
            );
            p1.handle(&hub, &events::SessionEnd { id: "100-abcd".into() });
        }

        // Second run: resume 100-abcd.
        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let mut p2 = SessionPlugin::with_mode(SessionMode::Resume(Some("100-abcd".into())));
        p2.dir = Some(dir.clone());
        p2.project = Some("proj".into());

        let ev = p2.wait(&hub).unwrap();
        let start = ev.downcast_ref::<events::SessionStart>().unwrap();
        assert_eq!(start.id, "100-abcd");
        p2.handle(
            &hub,
            &events::SessionStart {
                id: start.id.clone(),
                parent: start.parent.clone(),
            },
        );

        let recorded = log.borrow();
        assert_eq!(
            *recorded,
            vec![
                format!("replay_msg:{}", json!({"role": "user", "content": "what is 2+2?"})),
                format!(
                    "replay_msg:{}",
                    json!({"role": "assistant", "content": [{"type": "text", "text": "4"}]})
                ),
                "replay_ex:what is 2+2?:4".to_string(),
                "info:resumed 100-abcd (2 messages)".to_string(),
            ]
        );

        let lines = jsonl_of(&dir, "100-abcd");
        assert_eq!(lines.last().unwrap()["type"], "session_resume");
        assert_eq!(lines.last().unwrap()["id"], "100-abcd");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_sanitizes_dangling_tool_use() {
        let dir = scratch("resume-tool");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let file_content = [
            json!({"type": "session_start", "id": "200-tool", "parent": null, "ts": 200, "cwd": "/tmp"}),
            json!({"type": "message", "role": "user", "content": "run ls"}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}]}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        std::fs::write(proj.join("200-tool.jsonl"), file_content).unwrap();

        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let mut p = SessionPlugin::with_mode(SessionMode::Resume(Some("200-tool".into())));
        p.dir = Some(dir.clone());
        p.project = Some("proj".into());

        let ev = p.wait(&hub).unwrap();
        let start = ev.downcast_ref::<events::SessionStart>().unwrap();
        p.handle(
            &hub,
            &events::SessionStart {
                id: start.id.clone(),
                parent: start.parent.clone(),
            },
        );

        let recorded = log.borrow();
        // 3 ReplayMessages: user, assistant(tool_use), synthesized user(tool_result)
        let replay_msgs: Vec<&String> = recorded
            .iter()
            .filter(|s| s.starts_with("replay_msg:"))
            .collect();
        assert_eq!(replay_msgs.len(), 3);
        let synth: Value =
            serde_json::from_str(replay_msgs[2].strip_prefix("replay_msg:").unwrap()).unwrap();
        assert_eq!(synth["role"], "user");
        assert_eq!(synth["content"][0]["type"], "tool_result");
        assert_eq!(synth["content"][0]["tool_use_id"], "call_1");
        assert_eq!(synth["content"][0]["is_error"], true);

        // And the synthesized tool_result was persisted to the .jsonl before session_resume.
        let lines = jsonl_of(&dir, "200-tool");
        assert_eq!(lines[3]["type"], "message");
        assert_eq!(lines[3]["role"], "user");
        assert_eq!(lines[3]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(lines[4]["type"], "session_resume");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_sanitizes_dangling_user_prompt() {
        let dir = scratch("resume-user");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let file_content = [
            json!({"type": "session_start", "id": "300-user", "parent": null, "ts": 300, "cwd": "/tmp"}),
            json!({"type": "message", "role": "user", "content": "hello?"}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        std::fs::write(proj.join("300-user.jsonl"), file_content).unwrap();

        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let mut p = SessionPlugin::with_mode(SessionMode::Resume(Some("300-user".into())));
        p.dir = Some(dir.clone());
        p.project = Some("proj".into());

        let ev = p.wait(&hub).unwrap();
        let start = ev.downcast_ref::<events::SessionStart>().unwrap();
        p.handle(
            &hub,
            &events::SessionStart {
                id: start.id.clone(),
                parent: start.parent.clone(),
            },
        );

        let recorded = log.borrow();
        assert!(recorded.contains(&"replay_ex:hello?:(interrupted)".to_string()));
        let lines = jsonl_of(&dir, "300-user");
        assert_eq!(lines[2]["type"], "message");
        assert_eq!(lines[2]["role"], "assistant");
        assert_eq!(lines[2]["content"][0]["text"], "(interrupted)");
        assert_eq!(lines[3]["type"], "session_resume");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_none_picks_latest_root_and_ignores_subagents() {
        let dir = scratch("resume-latest");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();

        // Older root session
        std::fs::write(
            proj.join("100-oldr.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type": "session_start", "id": "100-oldr", "parent": null, "ts": 100, "cwd": "/tmp"}),
                json!({"type": "message", "role": "user", "content": "old prompt"})
            ),
        )
        .unwrap();
        // Newer root session
        std::fs::write(
            proj.join("200-newr.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type": "session_start", "id": "200-newr", "parent": null, "ts": 200, "cwd": "/tmp"}),
                json!({"type": "message", "role": "user", "content": "new prompt"})
            ),
        )
        .unwrap();
        // Even newer subagent session (parent != null) — must be ignored by Resume(None)
        std::fs::write(
            proj.join("300-sub1.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type": "session_start", "id": "300-sub1", "parent": "200-newr", "ts": 300, "cwd": "/tmp"}),
                json!({"type": "message", "role": "user", "content": "subagent task"})
            ),
        )
        .unwrap();

        let (mut p, _) = plugin_with_mode("resume-latest-p", SessionMode::Resume(None));
        p.dir = Some(dir.clone());
        let hub = Hub::new(vec![]);
        let ev = p.wait(&hub).unwrap();
        let start = ev.downcast_ref::<events::SessionStart>().unwrap();
        assert_eq!(start.id, "200-newr");

        // Also verify format_sessions_in lists root sessions newest-first with subagent count.
        let listing = format_sessions_in(&proj, 260);
        let list_lines: Vec<&str> = listing.lines().collect();
        assert_eq!(list_lines.len(), 2);
        assert!(
            list_lines[0].contains("200-newr")
                && list_lines[0].contains("1m ago")
                && list_lines[0].contains("\"new prompt\"")
                && list_lines[0].contains("(+1 subagent)"),
            "line 0: {}",
            list_lines[0]
        );
        assert!(
            list_lines[1].contains("100-oldr")
                && list_lines[1].contains("2m ago")
                && list_lines[1].contains("\"old prompt\""),
            "line 1: {}",
            list_lines[1]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_missing_session_emits_error_and_quits() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let (mut p, dir) =
            plugin_with_mode("resume-missing", SessionMode::Resume(Some("nope".into())));

        let ev = p.wait(&hub).expect("returns Quit on missing session");
        assert!(ev.downcast_ref::<events::Quit>().is_some());
        let recorded = log.borrow();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].starts_with("error:session not found: nope"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_after_resume_closes_resumed_and_starts_fresh_jsonl() {
        let dir = scratch("clear-after-resume");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("100-orig.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type": "session_start", "id": "100-orig", "parent": null, "ts": 100, "cwd": "/tmp"}),
                json!({"type": "message", "role": "user", "content": "hi"})
            ),
        )
        .unwrap();

        let mut p = SessionPlugin::with_mode(SessionMode::Resume(Some("100-orig".into())));
        p.dir = Some(dir.clone());
        p.project = Some("proj".into());
        let hub = Hub::new(vec![]);

        let ev = p.wait(&hub).unwrap();
        let start = ev.downcast_ref::<events::SessionStart>().unwrap();
        p.handle(
            &hub,
            &events::SessionStart {
                id: start.id.clone(),
                parent: start.parent.clone(),
            },
        );

        // Now /clear: closes the resumed session (writing session_end to 100-orig.jsonl)
        // and starts a fresh <new_id>.jsonl.
        p.handle(&hub, &events::ResetConversation {});
        let new_id = p.current.as_ref().unwrap().id.clone();
        assert_ne!(new_id, "100-orig");

        let orig_lines = jsonl_of(&dir, "100-orig");
        assert_eq!(orig_lines.last().unwrap()["type"], "session_end");

        let new_lines = jsonl_of(&dir, &new_id);
        assert_eq!(new_lines.len(), 1);
        assert_eq!(new_lines[0]["type"], "session_start");
        assert_eq!(new_lines[0]["id"], new_id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slug_collapses_to_safe_lowercase_basename() {
        for (cwd, expected) in [
            ("/home/me/herb/min", "min"),
            ("/srv/work/MIN", "min"),
            ("/tmp/my project!@#2", "my-project---2"),
            ("/tmp/---weird---", "weird"),
            ("/", "root"),
            ("///", "root"),
            ("/tmp/...", "..."),
            ("/tmp/---", "root"),
            (
                "/tmp/abcdefghijklmnopqrstuvwxyz0123456789extra",
                "abcdefghijklmnopqrstuvwxyz012345",
            ),
        ] {
            assert_eq!(slug_project(cwd), expected, "cwd={cwd}");
        }
    }

    #[test]
    fn entropy_suffixes_vary() {
        let suffixes: Vec<String> = (0..=255u8).map(|i| entropy_from([i, i, i, i])).collect();
        let unique: std::collections::HashSet<&String> = suffixes.iter().collect();
        assert_eq!(unique.len(), 36, "the whole alphabet is reached");
        assert_eq!(entropy_from([0, 0, 0, 0]), "0000");
        assert!(suffixes.iter().all(|s| s.len() == 4));
    }
}
