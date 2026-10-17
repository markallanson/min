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

/// Forget the conversation and start fresh (repl → agent).
pub struct ResetConversation {}

/// Shut down the harness (repl → everyone).
pub struct Quit {}
