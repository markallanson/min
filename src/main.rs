//! min — a minimal terminal agentic coding harness.
//!
//! The binary does nothing but assemble plugins and hand them to the core.
//! Everything you can see or do is a plugin; this file is the only glue.

use min_core::Plugin;

fn main() {
    // Load order = start order. The repl goes first so the terminal is set
    // up before anything wants to print; the agent goes last so every tool
    // is registered before the first turn runs.
    let plugins: Vec<Box<dyn Plugin>> = vec![
        // Box::new(min_repl::Repl::new()),
        // Box::new(min_llm::LlmPlugin),
        // Box::new(min_tools_fs::FsPlugin),
        // Box::new(min_tools_shell::ShellPlugin),
        // Box::new(min_agent::AgentPlugin::new()),
    ];
    min_core::run(plugins);
}
