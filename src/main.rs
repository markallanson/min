//! min — a minimal terminal agentic coding harness.
//!
//! The binary does nothing but assemble plugins and hand them to the core.
//! Everything you can see or do is a plugin; this file is the only glue.
//! Add a plugin by adding a crate to `crates/` and a line to this list.

use min_core::Plugin;

/// `--session <id>` — pin the first session's id (`--parent-session` alone
/// makes it the child of the given session). Both together are an error.
fn parse_args() -> Result<(Option<String>, Option<String>), String> {
    let mut session = None;
    let mut parent = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--session" => session = Some(value("--session")?),
            "--parent-session" => parent = Some(value("--parent-session")?),
            a => return Err(format!("unknown argument {a} — try --session <id>")),
        }
    }
    if session.is_some() && parent.is_some() {
        return Err("--session and --parent-session are mutually exclusive".into());
    }
    Ok((session, parent))
}

fn main() {
    let (session, parent) = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("min: {e}");
            std::process::exit(2);
        }
    };

    // Load order = start order. The session plugin goes first so its first
    // `wait` — where it announces the session, once everything has started —
    // is reached before the repl blocks on stdin; the repl next, so the
    // terminal is set up before anything wants to print; the agent last, so
    // every tool is registered before the first turn runs.
    let plugins: Vec<Box<dyn Plugin>> = vec![
        Box::new(min_session::SessionPlugin::new(session, parent)), // session id
        Box::new(min_session_log::SessionLogPlugin::new()), // session activity → XDG state log
        Box::new(min_herdr::HerdrPlugin::new()), // herdr state reporter (no-op outside herdr)
        Box::new(min_repl::Repl::new()),        // terminal UI
        Box::new(min_llm::LlmPlugin),           // streaming LLM client
        Box::new(min_tools_fs::FsPlugin),       // read / edit / write
        Box::new(min_tools_shell::ShellPlugin), // bash
        Box::new(min_tools_clock::ClockPlugin),   // current UTC time
        Box::new(min_subagents::SubagentsPlugin::new()), // the subagent tool: forks min
        Box::new(min_context::ContextPlugin),     // AGENTS.md → system prompt
        Box::new(min_agent::AgentPlugin::new()), // the agent loop
    ];
    min_core::run(plugins);
}
