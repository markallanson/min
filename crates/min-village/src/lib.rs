//! min-village — report this min session to a [Claude Village](https://gitlab.com/ivxs/user-projects/adam.wilson/claude-village) server.
//!
//! The village is a live, pixel-art view of every coding session on the
//! machine. It learns about a session in one of two ways: it reads
//! `~/.claude/sessions/*.json` (the Claude Code registry), or a harness
//! *pushes* the session to it over HTTP. The push path is the village's
//! **RPC protocol** — `POST /api/rpc` with a JSON body of one command:
//!
//! ```text
//! { "method": "upsertSession", "params": { "rec": { … }, "agent": "claude" } }
//! { "method": "ingestRecord", "params": { "target": <pid>, "record": { … } } }
//! { "method": "removeSession", "params": { "pid": 1234 } }
//! ```
//!
//! min is not Claude Code, so it has no registry file the village could
//! read. This plugin is the harness: it registers min's session with the
//! village and streams the conversation into it, so min shows up in the
//! village as a critter of its own.
//!
//! **It activates when it detects the village running.** On `start` the
//! plugin probes the configured URL (default `http://127.0.0.1:7420`,
//! moved with `MIN_VILLAGE_URL`, or `MIN_VILLAGE_HOST` / `MIN_VILLAGE_PORT`)
//! with a `GET /api/state`; a village answers with a `Snapshot`, anything
//! else — a refused connection, a timeout, a foreign server — does not.
//! Without a village the plugin is a complete no-op: no requests are made,
//! no events are emitted. min behaves exactly as if this crate were not
//! loaded.
//!
//! ## What crosses the wire
//!
//! min's own events are mapped onto the village's normalized record kinds:
//!
//! | min event | RPC command |
//! |---|---|
//! | `SessionStart` | `upsertSession` (`idle`) — the session joins the village |
//! | `UserInput` | `upsertSession` (`busy`), then `ingestRecord` `{kind:"prompt"}` |
//! | `LlmStart` | `ingestRecord` `{kind:"assistant"}`, empty — the critter starts thinking |
//! | `AssistantDelta` | buffered for the round |
//! | `Usage` | the round's text as one `{kind:"assistant"}` record, with `contextTokens` |
//! | `ToolStart` | `ingestRecord` `{kind:"assistant"}` carrying the tool, so the critter shows it live |
//! | `ToolEnd` | `ingestRecord` `{kind:"other-user", toolResult:true}` |
//! | idle `wait` after a turn | `upsertSession` (`idle`) — the turn is over |
//! | `SessionEnd` / `Quit` | `removeSession` — the critter leaves the village |
//!
//! A busy critter is only drawn at work when its last record is the
//! assistant's: text makes it think, a tool makes it use that tool. After
//! a prompt or a tool result the last record is the user's, so each LLM
//! round opens with an empty assistant record — without it, min would
//! look idle until the round's reply arrived at its very end.
//!
//! The turn's end is taken from `wait`, not `MessageEnd`: the agent emits
//! `MessageEnd` once per LLM round, before that round's tools run, so it
//! would show min idle between every tool call. The core calls `wait`
//! only once the bus is idle — that is, once the turn has fully returned.
//!
//! The plugin is a **listener**: it never returns an event from `wait` and
//! never blocks the bus beyond one short request to a server on this
//! machine.

use min_core::{events, Hub, Plugin};
use serde_json::{json, Value};
use std::any::Any;
use std::time::Duration;

/// The village's default port. `PORT` in the village's own environment.
const DEFAULT_PORT: u16 = 7420;
/// The village's default host: it listens on the loopback only.
const DEFAULT_HOST: &str = "127.0.0.1";
/// How long the startup probe (and every push) waits to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long a request waits for the village to answer.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);
/// The most assistant text kept for one round, in characters. The village
/// clips its own texts to 2000; this keeps the payload small and the
/// memory bounded.
const MAX_TEXT: usize = 4000;

/// Where commands go. The HTTP village in production; a recorder in tests.
trait Sink {
    fn send(&self, command: &Value) -> Result<(), String>;
}

/// The village's RPC endpoint.
struct HttpSink {
    http: ureq::Agent,
    url: String,
}

impl Sink for HttpSink {
    fn send(&self, command: &Value) -> Result<(), String> {
        let payload = serde_json::to_vec(command).map_err(|e| e.to_string())?;
        // ureq reports a non-2xx answer as an error of its own, so a 400
        // from a command the village rejected lands here too.
        self.http
            .post(&self.url)
            .set("content-type", "application/json")
            .send_bytes(&payload)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The village's normalized session record. This is the `rec` of the
/// village's `RegistryRecord` — the same shape the Claude Code registry
/// file has, so the village can treat min exactly like a Claude session.
#[derive(Clone, Debug)]
struct SessionRecord {
    pid: u32,
    session_id: String,
    cwd: String,
    started_at: u64,
    status: &'static str,
    status_updated_at: u64,
    name: String,
    /// The session that spawned this one (`--parent-session`), when min is
    /// a subagent. Sent as the registry's `hostSessionId` — "another
    /// session hosts this one" — so the village can tell a helper from a
    /// session of its own.
    host: Option<String>,
}

impl SessionRecord {
    /// The `rec` object of an `upsertSession` command.
    fn to_json(&self) -> Value {
        let mut rec = json!({
            "pid": self.pid,
            "sessionId": self.session_id,
            "cwd": self.cwd,
            "startedAt": self.started_at,
            "status": self.status,
            "statusUpdatedAt": self.status_updated_at,
            "name": self.name,
            "kind": "interactive",
            "entrypoint": "min",
        });
        if let Some(host) = &self.host {
            rec["hostSessionId"] = json!(host);
        }
        rec
    }
}

/// The plugin.
pub struct VillagePlugin {
    /// The village, once the probe found one. `None` means no village is
    /// running: the plugin is inert.
    sink: Option<Box<dyn Sink>>,
    /// The session record last pushed to the village, so a status change
    /// re-sends it whole. `None` before the first `SessionStart`.
    record: Option<SessionRecord>,
    /// The text of the current round, buffered until its `Usage`: the
    /// village wants one assistant record per reply, not one per chunk.
    text: String,
    /// Tool calls seen this session; numbers the ids that pair a
    /// `ToolStart` with its `ToolEnd`.
    tool_seq: u64,
    /// The id of the tool now running, for its `ToolEnd`.
    tool: Option<String>,
    /// Whether the village has been told the session is busy.
    busy: bool,
    /// Whether the last push failed. The first failure is reported; the
    /// rest are not, until a push succeeds again.
    failing: bool,
}

impl VillagePlugin {
    pub fn new() -> Self {
        VillagePlugin {
            sink: None,
            record: None,
            text: String::new(),
            tool_seq: 0,
            tool: None,
            busy: false,
            failing: false,
        }
    }

    /// The village's base URL: `MIN_VILLAGE_URL` when set, else
    /// `http://<MIN_VILLAGE_HOST>:<MIN_VILLAGE_PORT>`.
    ///
    /// The port matters: the village's own `PORT` environment variable is
    /// what moves it off 7420.
    fn configured_base() -> String {
        if let Ok(url) = std::env::var("MIN_VILLAGE_URL") {
            let url = url.trim();
            if !url.is_empty() {
                return url.trim_end_matches('/').to_string();
            }
        }
        let host = std::env::var("MIN_VILLAGE_HOST")
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_string());
        let port = std::env::var("MIN_VILLAGE_PORT")
            .ok()
            .and_then(|p| p.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        format!("http://{host}:{port}")
    }

    /// Look for a village: `GET /api/state` must answer with a snapshot.
    ///
    /// The check is deliberately strict. A village answers `/api/state`
    /// with `{"t":"snapshot",…}`; anything else — a 404 from another
    /// server on the same port, a connection refused, a page that is not
    /// JSON — means there is no village here, and the plugin stays inert.
    fn detect(http: &ureq::Agent, base: &str) -> bool {
        let Ok(resp) = http.get(&format!("{base}/api/state")).call() else {
            return false;
        };
        let Ok(body) = resp.into_json::<Value>() else {
            return false;
        };
        body.get("t").and_then(|t| t.as_str()) == Some("snapshot")
    }

    /// Send one command. A failure is reported once, as `Info` — not
    /// `Error`, which other plugins read as min itself being stuck.
    fn send(&mut self, hub: &Hub, command: Value) {
        let Some(sink) = &self.sink else { return };
        match sink.send(&command) {
            Ok(()) => self.failing = false,
            Err(e) => {
                if !self.failing {
                    hub.emit(events::Info { text: format!("village: {e}") });
                }
                self.failing = true;
            }
        }
    }

    /// Register (or re-register) min's session with the village.
    fn upsert(&mut self, hub: &Hub) {
        let Some(rec) = &self.record else { return };
        let command = json!({
            "method": "upsertSession",
            "params": { "rec": rec.to_json(), "agent": "claude" },
        });
        self.send(hub, command);
    }

    /// Move the session to `busy` or `idle`, if it is not there already.
    fn set_busy(&mut self, hub: &Hub, busy: bool) {
        if self.busy == busy {
            return;
        }
        let Some(rec) = &mut self.record else { return };
        rec.status = if busy { "busy" } else { "idle" };
        rec.status_updated_at = now_ms();
        self.busy = busy;
        self.upsert(hub);
    }

    /// Send one record to the village.
    fn ingest(&mut self, hub: &Hub, record: Value) {
        let Some(rec) = &self.record else { return };
        let command = json!({
            "method": "ingestRecord",
            "params": { "target": rec.pid, "record": record },
        });
        self.send(hub, command);
    }

    /// Send the buffered round as one assistant record, with the context
    /// the model saw when the provider reported it.
    fn flush(&mut self, hub: &Hub, usage: Option<&events::Usage>) {
        let texts = paragraphs(&std::mem::take(&mut self.text));
        let tokens = usage.map(|u| u.usage.prompt_tokens()).filter(|&t| t > 0);
        if texts.is_empty() && tokens.is_none() {
            return;
        }
        let mut record = json!({ "kind": "assistant", "at": now_ms(), "tools": [] });
        if !texts.is_empty() {
            record["texts"] = json!(texts);
        }
        if let Some(u) = usage.filter(|u| !u.model.is_empty()) {
            record["model"] = json!(u.model);
        }
        if let Some(tokens) = tokens {
            record["contextTokens"] = json!(tokens);
        }
        self.ingest(hub, record);
    }

    /// Leave the village.
    fn remove(&mut self, hub: &Hub) {
        let Some(rec) = self.record.take() else { return };
        self.send(hub, json!({ "method": "removeSession", "params": { "pid": rec.pid } }));
        self.text.clear();
        self.tool = None;
        self.busy = false;
    }
}

/// Milliseconds since the epoch — the village's timestamp unit.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The name the village shows for min's critter: the working directory's
/// basename and the random tail of min's session id, e.g. `min-a7k2` for
/// session `1792763682-a7k2`. The village labels a critter by name alone,
/// so without the tail every min in one directory would look the same.
/// It is the same identity min hands Herdr (`--agent-session-id`): the
/// session, not the process, so a `/clear` renames the critter.
fn session_name(cwd: &str, session_id: &str) -> String {
    let trimmed = cwd.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let base = if base.is_empty() { "min" } else { base };
    let tail = session_id.rsplit('-').next().unwrap_or(session_id);
    format!("{base}-{tail}")
}

/// A min tool as the village knows it. The village draws a critter's
/// activity from Claude Code's tool names and input fields; min has only
/// its own name and the call's one-line brief, which is a path for the
/// file tools and the command for bash.
fn village_tool(name: &str, brief: &str) -> (String, Value) {
    match name {
        "read" => ("Read".into(), json!({ "file_path": brief })),
        "edit" => ("Edit".into(), json!({ "file_path": brief })),
        "write" => ("Write".into(), json!({ "file_path": brief })),
        "bash" => ("Bash".into(), json!({ "command": brief })),
        n if n.starts_with("subagent") => ("Task".into(), json!({ "description": brief })),
        n => (n.to_string(), json!({ "brief": brief })),
    }
}

/// The non-empty paragraphs of a reply.
fn paragraphs(text: &str) -> Vec<String> {
    text.split("\n\n")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

impl Plugin for VillagePlugin {
    fn name(&self) -> &'static str {
        "village"
    }

    fn start(&mut self, _hub: &Hub) {
        let http = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build();
        let base = Self::configured_base();
        if !Self::detect(&http, &base) {
            return;
        }
        // The session id is not known yet: the session plugin announces
        // it from its first `wait`, after every plugin has started. The
        // session is registered then, in `handle`.
        self.sink = Some(Box::new(HttpSink { http, url: format!("{base}/api/rpc") }));
    }

    /// Called once the bus is idle — after a turn has returned in full.
    /// The village is told the turn is over; nothing is ever injected.
    fn wait(&mut self, hub: &Hub) -> Option<Box<dyn Any>> {
        if self.busy {
            // A turn that ended without a `Usage` (an error, a cancel)
            // still shows what it said.
            self.flush(hub, None);
            self.set_busy(hub, false);
        }
        None
    }

    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if self.sink.is_none() {
            return;
        }

        if let Some(e) = event.downcast_ref::<events::SessionStart>() {
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let now = now_ms();
            self.record = Some(SessionRecord {
                pid: std::process::id(),
                session_id: e.id.clone(),
                name: session_name(&cwd, &e.id),
                cwd,
                started_at: now,
                status: "idle",
                status_updated_at: now,
                host: e.parent.clone(),
            });
            self.busy = false;
            self.upsert(hub);
        } else if event.downcast_ref::<events::SessionEnd>().is_some()
            || event.downcast_ref::<events::Quit>().is_some()
        {
            self.remove(hub);
        } else if let Some(e) = event.downcast_ref::<events::UserInput>() {
            self.set_busy(hub, true);
            self.ingest(
                hub,
                json!({
                    "kind": "prompt",
                    "at": now_ms(),
                    "text": one_line(&e.text, 160),
                    "fullText": e.text.chars().take(MAX_TEXT).collect::<String>(),
                }),
            );
        } else if let Some(e) = event.downcast_ref::<events::LlmStart>() {
            let mut record = json!({ "kind": "assistant", "at": now_ms(), "tools": [] });
            if !e.model.is_empty() {
                record["model"] = json!(e.model);
            }
            self.ingest(hub, record);
        } else if let Some(e) = event.downcast_ref::<events::AssistantDelta>() {
            let room = MAX_TEXT.saturating_sub(self.text.chars().count());
            self.text.extend(e.text.chars().take(room));
        } else if let Some(e) = event.downcast_ref::<events::Usage>() {
            // `Usage` closes a round: it follows the round's `MessageEnd`
            // and precedes any tools the round asked for.
            self.flush(hub, Some(e));
        } else if let Some(e) = event.downcast_ref::<events::ToolStart>() {
            self.tool_seq += 1;
            let id = format!("min-tool-{}", self.tool_seq);
            let (name, input) = village_tool(&e.name, &e.brief);
            self.ingest(
                hub,
                json!({
                    "kind": "assistant",
                    "at": now_ms(),
                    "tools": [{ "id": id, "name": name, "input": input }],
                }),
            );
            self.tool = Some(id);
        } else if event.downcast_ref::<events::ToolEnd>().is_some() {
            if let Some(id) = self.tool.take() {
                self.ingest(
                    hub,
                    json!({
                        "kind": "other-user",
                        "at": now_ms(),
                        "toolResult": true,
                        "toolResultIds": [id],
                    }),
                );
            }
        }
    }
}

/// A single line, at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use min_core::usage::TokenUsage;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Records every command instead of sending it.
    struct Recorder(Rc<RefCell<Vec<Value>>>);

    impl Sink for Recorder {
        fn send(&self, command: &Value) -> Result<(), String> {
            self.0.borrow_mut().push(command.clone());
            Ok(())
        }
    }

    /// A plugin wired to a recorder, as if a village had been detected.
    fn connected() -> (VillagePlugin, Rc<RefCell<Vec<Value>>>) {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let mut plugin = VillagePlugin::new();
        plugin.sink = Some(Box::new(Recorder(sent.clone())));
        (plugin, sent)
    }

    fn hub() -> Hub {
        Hub::new(Vec::new())
    }

    fn methods(sent: &[Value]) -> Vec<String> {
        sent.iter()
            .map(|c| {
                let m = c["method"].as_str().unwrap();
                match c["params"]["record"]["kind"].as_str() {
                    Some(kind) => format!("{m}:{kind}"),
                    None => match c["params"]["rec"]["status"].as_str() {
                        Some(status) => format!("{m}:{status}"),
                        None => m.to_string(),
                    },
                }
            })
            .collect()
    }

    fn llm_start(round: usize) -> events::LlmStart {
        events::LlmStart { round, model: "m".into(), messages: 1, tools: 0 }
    }

    fn session_start(p: &mut VillagePlugin, hub: &Hub) {
        p.handle(hub, &events::SessionStart { id: "1-abcd".into(), parent: None });
    }

    #[test]
    fn without_a_village_nothing_happens() {
        let hub = hub();
        let mut p = VillagePlugin::new();
        session_start(&mut p, &hub);
        p.handle(&hub, &events::UserInput { text: "hi".into() });
        assert!(p.record.is_none());
        assert!(p.wait(&hub).is_none());
    }

    #[test]
    fn a_turn_with_a_tool_maps_onto_village_commands() {
        let hub = hub();
        let (mut p, sent) = connected();
        session_start(&mut p, &hub);
        p.handle(&hub, &events::UserInput { text: "list\nfiles".into() });
        p.handle(&hub, &llm_start(1));
        p.handle(&hub, &events::AssistantDelta { text: "Let me look.".into() });
        p.handle(&hub, &events::MessageEnd {});
        let usage = TokenUsage { input_tokens: 1200, ..TokenUsage::default() };
        p.handle(&hub, &events::Usage { usage, model: "m".into(), round: 1 });
        p.handle(&hub, &events::ToolStart { name: "bash".into(), brief: "ls -la".into() });
        p.handle(&hub, &events::ToolEnd { ok: true, secs: 0.1, note: String::new(), preview: String::new() });
        p.handle(&hub, &llm_start(2));
        p.handle(&hub, &events::AssistantDelta { text: "Two files.".into() });
        p.handle(&hub, &events::MessageEnd {});
        p.handle(&hub, &events::Usage { usage: TokenUsage::default(), model: "m".into(), round: 2 });
        assert!(p.wait(&hub).is_none());

        let sent = sent.borrow();
        assert_eq!(
            methods(&sent),
            [
                "upsertSession:idle",
                "upsertSession:busy",
                "ingestRecord:prompt",
                "ingestRecord:assistant",
                "ingestRecord:assistant",
                "ingestRecord:assistant",
                "ingestRecord:other-user",
                "ingestRecord:assistant",
                "ingestRecord:assistant",
                "upsertSession:idle",
            ]
        );
        // Each round opens with an empty assistant record, so the critter
        // thinks while the model works rather than after it has finished.
        for opener in [&sent[3], &sent[7]] {
            let record = &opener["params"]["record"];
            assert_eq!(record["tools"], json!([]));
            assert!(record.get("texts").is_none());
        }
        let rec = &sent[0]["params"]["rec"];
        assert_eq!(rec["sessionId"], "1-abcd");
        assert_eq!(rec["pid"], std::process::id());
        assert_eq!(sent[2]["params"]["record"]["text"], "list files");
        let reply = &sent[4]["params"]["record"];
        assert_eq!(reply["texts"], json!(["Let me look."]));
        assert_eq!(reply["contextTokens"], 1200);
        assert_eq!(reply["model"], "m");
        let tool = &sent[5]["params"]["record"]["tools"][0];
        assert_eq!(tool["name"], "Bash");
        assert_eq!(tool["input"]["command"], "ls -la");
        assert_eq!(sent[6]["params"]["record"]["toolResultIds"], json!([tool["id"]]));
        // A provider that reported no usage sends no context reading.
        assert!(sent[8]["params"]["record"].get("contextTokens").is_none());
    }

    #[test]
    fn a_subagent_names_its_host_session() {
        let hub = hub();
        let (mut p, sent) = connected();
        session_start(&mut p, &hub);
        p.handle(&hub, &events::SessionStart { id: "2-efgh".into(), parent: Some("1-abcd".into()) });
        let sent = sent.borrow();
        assert!(sent[0]["params"]["rec"].get("hostSessionId").is_none());
        assert_eq!(sent[1]["params"]["rec"]["hostSessionId"], "1-abcd");
    }

    #[test]
    fn idle_wait_outside_a_turn_sends_nothing() {
        let hub = hub();
        let (mut p, sent) = connected();
        session_start(&mut p, &hub);
        p.wait(&hub);
        p.wait(&hub);
        assert_eq!(methods(&sent.borrow()), ["upsertSession:idle"]);
    }

    #[test]
    fn a_failed_turn_still_flushes_and_goes_idle() {
        let hub = hub();
        let (mut p, sent) = connected();
        session_start(&mut p, &hub);
        p.handle(&hub, &events::UserInput { text: "hi".into() });
        p.handle(&hub, &events::AssistantDelta { text: "partial".into() });
        p.handle(&hub, &events::TurnCancelled {});
        p.wait(&hub);
        let sent = sent.borrow();
        assert_eq!(
            methods(&sent)[3..],
            ["ingestRecord:assistant", "upsertSession:idle"]
        );
        assert_eq!(sent[3]["params"]["record"]["texts"], json!(["partial"]));
    }

    #[test]
    fn clear_replaces_the_session_and_quit_removes_it() {
        let hub = hub();
        let (mut p, sent) = connected();
        session_start(&mut p, &hub);
        p.handle(&hub, &events::SessionEnd { id: "1-abcd".into() });
        p.handle(&hub, &events::SessionStart { id: "2-efgh".into(), parent: None });
        p.handle(&hub, &events::Quit {});
        p.handle(&hub, &events::Quit {});
        let sent = sent.borrow();
        assert_eq!(
            methods(&sent),
            ["upsertSession:idle", "removeSession", "upsertSession:idle", "removeSession"]
        );
        assert_eq!(sent[2]["params"]["rec"]["sessionId"], "2-efgh");
        // The name follows the session, as Herdr's session id does.
        assert_eq!(sent[0]["params"]["rec"]["name"], "min-village-abcd");
        assert_eq!(sent[2]["params"]["rec"]["name"], "min-village-efgh");
        assert_eq!(sent[3]["params"]["pid"], std::process::id());
    }

    #[test]
    fn assistant_text_is_capped_on_a_char_boundary() {
        let hub = hub();
        let (mut p, _) = connected();
        session_start(&mut p, &hub);
        let chunk = "é".repeat(MAX_TEXT);
        p.handle(&hub, &events::AssistantDelta { text: chunk.clone() });
        p.handle(&hub, &events::AssistantDelta { text: chunk });
        assert_eq!(p.text.chars().count(), MAX_TEXT);
    }

    #[test]
    fn min_tools_take_claude_code_names() {
        assert_eq!(village_tool("read", "a.rs").0, "Read");
        assert_eq!(village_tool("bash", "ls").1["command"], "ls");
        assert_eq!(village_tool("subagent_spawn", "x").0, "Task");
        assert_eq!(village_tool("clock", "").0, "clock");
    }

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("hello\n  world", 20), "hello world");
        assert_eq!(one_line("abcdef", 4), "abc…");
    }

    #[test]
    fn session_name_is_the_basename_and_the_session_tail() {
        assert_eq!(session_name("/home/user/project", "1792763682-a7k2"), "project-a7k2");
        assert_eq!(session_name("/home/user/project/", "1-x9"), "project-x9");
        assert_eq!(session_name("", "pinned"), "min-pinned");
    }
}
