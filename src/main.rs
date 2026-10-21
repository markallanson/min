//! min — a minimal terminal agentic coding harness.
//!
//! The binary does nothing but assemble plugins and hand them to the core.
//! Everything you can see or do is a plugin; this file is the only glue.
//! Add a plugin by adding a crate to `crates/` and a line to this list.

use min_core::Plugin;
use min_session::SessionMode;

enum CliAction {
    Run(SessionMode),
    ListSessions,
}

fn parse_args_from<I: Iterator<Item = String>>(args: I) -> Result<CliAction, String> {
    let mut session: Option<String> = None;
    let mut parent: Option<String> = None;
    let mut resume: Option<Option<String>> = None;
    let mut list_sessions = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--session needs a value".to_string())?;
                session = Some(val);
            }
            "--parent-session" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--parent-session needs a value".to_string())?;
                parent = Some(val);
            }
            "--resume" => {
                let id = match args.peek() {
                    Some(next) if !next.starts_with("--") => args.next(),
                    _ => None,
                };
                resume = Some(id);
            }
            "--sessions" => {
                list_sessions = true;
            }
            a => {
                return Err(format!(
                    "unknown argument {a} — try --resume [<id>], --sessions, or --session <id>"
                ))
            }
        }
    }

    if list_sessions {
        if session.is_some() || parent.is_some() || resume.is_some() {
            return Err("--sessions cannot be combined with other session flags".into());
        }
        return Ok(CliAction::ListSessions);
    }

    let modes_set = usize::from(session.is_some())
        + usize::from(parent.is_some())
        + usize::from(resume.is_some());
    if modes_set > 1 {
        return Err("--session, --parent-session, and --resume are mutually exclusive".into());
    }

    let mode = if let Some(target) = resume {
        SessionMode::Resume(target)
    } else if let Some(id) = session {
        SessionMode::Pinned(id)
    } else {
        SessionMode::Fresh { parent }
    };
    Ok(CliAction::Run(mode))
}

fn main() {
    let mode = match parse_args_from(std::env::args().skip(1)) {
        Ok(CliAction::ListSessions) => match min_session::list_sessions() {
            Ok(out) => {
                println!("{out}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("min: {e}");
                std::process::exit(1);
            }
        },
        Ok(CliAction::Run(mode)) => mode,
        Err(e) => {
            eprintln!("min: {e}");
            std::process::exit(2);
        }
    };

    // Load order = start order. The session plugin goes first so its first
    // `wait` — where it announces the session, once everything has started —
    // is reached before the repl blocks on stdin; the repl next, so the
    // terminal is set up before anything wants to print; the agent last, so
    // every tool is registered before the first turn runs. The usage plugin
    // is a pure listener — it records the agent's `Usage` events and
    // publishes the ledger — so its position does not matter.
    let plugins: Vec<Box<dyn Plugin>> = vec![
        Box::new(min_session::SessionPlugin::with_mode(mode)), // session id, JSONL store, resume
        Box::new(min_herdr::HerdrPlugin::new()), // herdr state reporter (no-op outside herdr)
        Box::new(min_repl::Repl::new()),         // terminal UI
        Box::new(min_llm::RouterPlugin),         // LLM router
        Box::new(min_llm::AnthropicPlugin),      // Anthropic provider
        Box::new(min_llm::OpenAiPlugin),         // OpenAI provider
        Box::new(min_llm::VertexPlugin),         // Vertex provider
        Box::new(min_llm::ZaiPlugin),            // Zai provider
        Box::new(min_llm::OllamaPlugin),         // Ollama provider
        Box::new(min_tools_fs::FsPlugin),        // read / edit / write
        Box::new(min_tools_shell::ShellPlugin),  // bash
        Box::new(min_tools_clock::ClockPlugin),  // current UTC time
        Box::new(min_subagents::SubagentsPlugin::new()), // the subagent tool: forks min
        Box::new(min_usage::UsagePlugin::new()),     // token accounting ledger
        Box::new(min_context::ContextPlugin),    // AGENTS.md → system prompt
        Box::new(min_agent::AgentPlugin::new()), // the agent loop
    ];
    min_core::run(plugins);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliAction, String> {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn cli_modes() {
        assert!(matches!(
            parse(&[]),
            Ok(CliAction::Run(SessionMode::Fresh { parent: None }))
        ));
        assert!(matches!(
            parse(&["--session", "123-abcd"]),
            Ok(CliAction::Run(SessionMode::Pinned(ref id))) if id == "123-abcd"
        ));
        assert!(matches!(
            parse(&["--parent-session", "99-papa"]),
            Ok(CliAction::Run(SessionMode::Fresh { parent: Some(ref p) })) if p == "99-papa"
        ));
        assert!(matches!(
            parse(&["--resume"]),
            Ok(CliAction::Run(SessionMode::Resume(None)))
        ));
        assert!(matches!(
            parse(&["--resume", "123-abcd"]),
            Ok(CliAction::Run(SessionMode::Resume(Some(ref id)))) if id == "123-abcd"
        ));
        assert!(matches!(parse(&["--sessions"]), Ok(CliAction::ListSessions)));
        assert!(parse(&["--resume", "--session", "x"]).is_err());
        assert!(parse(&["--session", "x", "--parent-session", "y"]).is_err());
    }
}
