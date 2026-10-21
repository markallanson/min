//! Event types shared between plugins.
//!
//! These are pure data — no behaviour lives here. They live in core only so
//! that plugins never need to depend on each other to speak the same event
//! vocabulary. Adding an event type adds no behaviour to the core: the core
//! routes by type, blindly.

/// The user submitted a line of input (repl → agent).
pub struct UserInput {
    pub text: String,
}

/// The agent is about to call the LLM for one completion round (agent → everyone).
pub struct LlmStart {
    /// 1-based round index within the current turn (1 on the user's prompt,
    /// 2+ after tool results are fed back).
    pub round: usize,
    /// Model identifier configured on the LLM service.
    pub model: String,
    /// Number of conversation messages sent in the request.
    pub messages: usize,
    /// Number of tool specifications offered to the model.
    pub tools: usize,
}

/// The model is working on non-text output — streaming reasoning tokens or
/// assembling a tool call's JSON arguments (llm/agent → everyone).
pub struct Thinking {
    /// Streamed reasoning/thinking text chunk, if any.
    pub text: String,
    /// Name of the tool being assembled, when streaming a tool call.
    pub tool: String,
}

/// A chunk of assistant text arrived while streaming (llm/agent → repl).
pub struct AssistantDelta {
    pub text: String,
}

/// The assistant's message is complete (agent → repl).
pub struct MessageEnd {}

/// The agent is about to run a tool (agent → repl).
pub struct ToolStart {
    pub name: String,
    pub brief: String,
}

/// A tool finished (agent → repl).
pub struct ToolEnd {
    pub ok: bool,
    pub secs: f64,
    /// One-line glance of the result (first output line, capped) — shown on
    /// failure and written to the session log.
    pub note: String,
    /// First lines of the tool's output for the UI — the repl prints them
    /// under the ✓ so a command's progress is visible without opening the
    /// transcript. May be empty (nothing to show).
    pub preview: String,
}

/// Human-facing status text (any plugin → repl).
pub struct Info {
    pub text: String,
}

/// Human-facing error text (any plugin → repl).
pub struct Error {
    pub text: String,
}

/// Forget the conversation and start fresh (repl → everyone). The agent
/// clears its history; context takes it as the cue to re-read instruction
/// files and hand over fresh `SystemPromptSection`s.
pub struct ResetConversation {}

/// A section to append to the agent's system prompt (context → agent),
/// e.g. min-context handing over the project's AGENTS.md. Emitted at
/// startup and again whenever instructions are re-read (`/clear`); every
/// re-read round is preceded by [`ClearPromptSections`], so nothing stale
/// survives.
pub struct SystemPromptSection {
    pub title: String,
    pub body: String,
}

/// Forget every system-prompt section collected so far (context → agent).
/// Precedes a fresh round of [`SystemPromptSection`]s when instructions are
/// re-read, so edited or deleted files leave nothing stale behind.
pub struct ClearPromptSections {}

/// A session began (session → everyone). The session plugin announces the
/// first session once every plugin has started — it returns this from its
/// first `wait`, so the broadcast reaches all plugins, itself included —
/// and a fresh one each time the conversation is cleared
/// (`ResetConversation`), preceded by [`SessionEnd`] for the session it
/// replaces. Ids are `<epoch seconds>-<4 random alphanumeric characters>`:
/// they sort chronologically and cannot collide, however many sessions begin
/// within one second. A plugin that needs to know which session it is in
/// keeps the last `id` it saw in its own state; `parent` links a session to
/// the one that asked for it (`--parent-session`).
pub struct SessionStart {
    pub id: String,
    pub parent: Option<String>,
}

/// A session closed (session → everyone). Emitted immediately before the
/// [`SessionStart`] of its replacement when the conversation is cleared.
pub struct SessionEnd {
    pub id: String,
}

/// A conversation message was appended to the agent's history (agent → session).
/// Carries the serialized JSON of the message (`{"role": ..., "content": ...}`)
/// so `min-core` stays dependency-free.
pub struct ConversationMessage {
    pub json: String,
}

/// A persisted conversation message replayed when resuming a session
/// (session → agent, repl).
pub struct ReplayMessage {
    pub json: String,
}

/// The last prompt/reply exchange of a resumed session (session → repl),
/// emitted after [`ReplayMessage`]s so the user sees where they left off.
pub struct ReplayExchange {
    pub prompt: String,
    pub reply: String,
}

/// The in-flight turn was cancelled by the user (agent/repl → everyone).
pub struct TurnCancelled {}

/// Token accounting for one completed LLM request (agent → everyone).
///
/// Emitted after every round that produced a usage report. The `usage`
/// is normalized across endpoints (see [`crate::usage::TokenUsage`]), so a
/// consumer never has to know which provider answered. Endpoints that do
/// not report usage still emit the event, with an empty `usage` — the
/// event says "a request finished", the payload says "here is what it
/// cost, if the provider told us".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub usage: crate::usage::TokenUsage,
    /// The model that served the request.
    pub model: String,
    /// 1-based round index within the current turn.
    pub round: usize,
}

/// Shut down the harness (repl → everyone).
pub struct Quit {}
