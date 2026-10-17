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
    pub note: String,
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
/// replaces. Ids are epoch seconds, stepped past the previous id when two
/// sessions begin inside one second. A plugin that needs to know which
/// session it is in keeps the last `id` it saw in its own state; `parent`
/// links a session to the one that asked for it (`--parent-session`).
pub struct SessionStart {
    pub id: u64,
    pub parent: Option<u64>,
}

/// A session closed (session → everyone). Emitted immediately before the
/// [`SessionStart`] of its replacement when the conversation is cleared.
pub struct SessionEnd {
    pub id: u64,
}

/// Shut down the harness (repl → everyone).
pub struct Quit {}
