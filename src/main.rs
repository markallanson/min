//! min — a minimal terminal agentic coding harness.
//!
//! The binary does nothing but assemble plugins and hand them to the core.
//! Everything you can see or do is a plugin; this file is the only glue.
//! Add a plugin by adding a crate to `crates/` and a line to this list.

use min_core::Plugin;

fn main() {
    // Load order = start order. The repl goes first so the terminal is set
    // up before anything wants to print; the agent goes last so every tool
    // is registered before the first turn runs.
    let plugins: Vec<Box<dyn Plugin>> = vec![
        Box::new(min_herdr::HerdrPlugin::new()), // herdr state reporter (no-op outside herdr)
        Box::new(min_repl::Repl::new()),        // terminal UI
        Box::new(min_llm::LlmPlugin),           // streaming LLM client
        Box::new(min_tools_fs::FsPlugin),       // read / edit / write
        Box::new(min_tools_shell::ShellPlugin), // bash
        Box::new(min_tools_clock::ClockPlugin),   // current UTC time
        Box::new(min_context::ContextPlugin),     // AGENTS.md → system prompt
        Box::new(min_agent::AgentPlugin::new()), // the agent loop
    ];
    min_core::run(plugins);
}
