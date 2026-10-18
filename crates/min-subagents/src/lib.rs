//! min-subagents — the `subagent` tool.
//!
//! Spawns a fresh `min` process as a *subagent*: a whole harness — its own
//! session, its own conversation, its own tools — given one instruction on
//! stdin, and reports its captured output back as the tool result.
//!
//! The pieces were all already there; this crate only wires them together:
//!
//! - **parenting** — `min --parent-session <id>` (min-session) makes the
//!   child's first session a child of this process's session, so the
//!   subagent's log (min-session-log) files under its parent's;
//! - **the instruction** — the child's repl reads non-tty stdin line by
//!   line (`echo 'hello' | min`): one instruction line becomes one turn,
//!   and EOF after it quits cleanly — so a spawn plus a write plus a close
//!   *is* "one instruction, one turn, exit";
//! - **the result** — everything the subagent prints on stdout/stderr
//!   (its streamed answer, tool activity, errors) is captured while it
//!   works and handed back to the parent's model when it exits.

use min_core::{events, Hub, Plugin, Tool};
use serde_json::Value;
use std::any::Any;
use std::cell::RefCell;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// How long a subagent may run before it is killed (model can override,
/// clamped to this max). Subagents do whole tasks — minutes, not seconds —
/// so the default is generous.
const DEFAULT_TIMEOUT: u64 = 600;
const MAX_TIMEOUT: u64 = 3600;

pub struct SubagentsPlugin {
    /// This process's session id, as announced by min-session's
    /// [`events::SessionStart`]. Passed to each child as
    /// `--parent-session`, so subagent sessions are children of ours.
    /// Shared with the tool behind an `Rc<RefCell<…>>` because the tool is
    /// stored in the core's registry while the plugin keeps receiving
    /// events. `None` until the first announcement — the events *are* the
    /// session interface; there is no session service.
    session: Rc<RefCell<Option<String>>>,
}

impl SubagentsPlugin {
    pub fn new() -> Self {
        SubagentsPlugin { session: Rc::new(RefCell::new(None)) }
    }
}

impl Default for SubagentsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SubagentsPlugin {
    fn name(&self) -> &'static str {
        "subagents"
    }

    fn start(&mut self, hub: &Hub) {
        hub.tools().register(Box::new(SubagentTool { session: self.session.clone() }));
    }

    fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
        // Keep the latest session id: a subagent spawned after `/clear`
        // parents on the *new* session. Nothing to do until session exists.
        if let Some(e) = event.downcast_ref::<events::SessionStart>() {
            *self.session.borrow_mut() = Some(e.id.clone());
        }
    }
}

struct SubagentTool {
    session: Rc<RefCell<Option<String>>>,
}

impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn spec(&self) -> &'static str {
        r#"{"description":"Spawn a subagent: a fresh min process with its own session (a child of this one) and its own tools, given one instruction. It works with no other context than the instruction — include the goal, relevant paths, and what a good result looks like. Blocks until the subagent's turn ends, then returns its output. One call runs one subagent to completion; to run several, make several calls. A subagent has this tool too — instruct it to work directly unless delegation is truly needed.","input_schema":{"type":"object","properties":{"instruction":{"type":"string","description":"the complete task for the subagent"},"timeout_secs":{"type":"integer","description":"kill the subagent after this many seconds (default 600, max 3600)"}},"required":["instruction"]}}"#
    }

    fn run(&self, input: &str) -> Result<String, String> {
        let v: Value = serde_json::from_str(input).map_err(|e| format!("bad input: {e}"))?;
        let instruction = v["instruction"].as_str().ok_or("missing `instruction`")?;
        let timeout = v["timeout_secs"].as_u64().unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT);
        let parent = self
            .session
            .borrow()
            .clone()
            .ok_or("no session id yet — is the session plugin loaded?")?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        collect(&exe, &parent, instruction, timeout)
    }
}

/// Spawn `exe --parent-session <parent>` with piped stdio and its own
/// process group (so a timeout can kill the whole tree). Executing a file
/// written moments earlier can fail with a transient `ETXTBSY` — the
/// kernel refuses to exec anything still held open for writing, and the
/// writer's close can lose a race with layered FS flushing — so a short
/// bounded retry is applied. That is also what makes the tests
/// deterministic: they spawn scripts they have just written.
fn spawn_subagent(exe: &Path, parent: &str) -> std::io::Result<std::process::Child> {
    const TRIES: usize = 50;
    let mut last = None;
    for attempt in 0..TRIES {
        let spawned = Command::new(exe)
            .arg("--parent-session")
            .arg(parent)
            .stdin(Stdio::piped()) // the instruction arrives here
            .stdout(Stdio::piped()) // its turn's output
            .stderr(Stdio::piped()) // its panics and harness errors
            .process_group(0) // own group: a timeout kills the whole tree
            .spawn();
        match spawned {
            Ok(child) => return Ok(child),
            Err(e) if e.raw_os_error() == Some(26 /* ETXTBSY */) && attempt + 1 < TRIES => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap())
}

/// Spawn one subagent — `exe --parent-session <parent>`, `instruction` on
/// stdin — wait for its turn to end, and return its captured output. `Ok`
/// on a clean exit (0); anything else (non-zero exit, signal, timeout) is
/// an `Err` that still carries everything the subagent printed, so the
/// parent's model sees how far it got. Shared with the tests, which pass a
/// stand-in executable.
fn collect(
    exe: &Path,
    parent: &str,
    instruction: &str,
    timeout: u64,
) -> Result<String, String> {
    let mut child = spawn_subagent(exe, parent).map_err(|e| format!("spawn subagent: {e}"))?;

    // The instruction, then EOF. The child reads one line — the whole
    // instruction *is* the line — runs its turn, and quits at EOF. Written
    // before draining: the child writes nothing to stdout until it has read
    // its input, so the pipe cannot fill and deadlock either side.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "{instruction}");
    }

    // Drain stdout and stderr on their own threads (no pipe deadlocks —
    // same pattern as the bash tool) while the child works.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let t_out = std::thread::spawn(move || drain(stdout));
    let t_err = std::thread::spawn(move || drain(stderr));

    let deadline = Instant::now() + Duration::from_secs(timeout);
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(s) => break Some(s),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let mut timed_out = false;
    let status = match status {
        Some(s) => s,
        None => {
            timed_out = true;
            // The child leads its own process group: kill the whole tree —
            // anything it spawned included — or an orphaned grandchild
            // holding the output pipes would keep the drains (and this
            // tool, and min) blocked long past the deadline.
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            child.wait().map_err(|e| format!("wait after kill: {e}"))?
        }
    };
    let mut out = t_out.join().unwrap_or_default();
    let err = t_err.join().unwrap_or_default();
    if !err.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("--- stderr ---\n");
        out.push_str(&err);
    }

    let code = status.code().unwrap_or(-1);
    let mut reply = String::new();
    if timed_out {
        reply.push_str(&format!("timed out after {timeout}s (subagent killed)\n"));
    }
    reply.push_str(&format!("exit code: {code}\n"));
    reply.push_str(&truncate(&out, 24_000));
    if out.trim().is_empty() {
        reply.push_str("(no output)\n");
    }
    if timed_out || code != 0 {
        Err(reply)
    } else {
        Ok(reply)
    }
}

fn drain(r: Option<impl Read>) -> String {
    let mut s = String::new();
    if let Some(r) = r {
        let _ = std::io::BufReader::new(r).read_to_string(&mut s);
    }
    s
}

fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let total_chars = s.chars().count();
    let head_chars = cap * 3 / 4;
    let tail_chars = cap / 4;
    let head: String = s.chars().take(head_chars).collect();
    let tail: String = s.chars().skip(total_chars.saturating_sub(tail_chars)).collect();
    format!("{head}\n…[truncated {} chars]…\n{tail}", total_chars - head_chars - tail_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// A stand-in "min": a shell script that ignores its arguments, prints
    /// what it was given on stdin (what the real child turns into a turn),
    /// and exits with the status in its body. Uses the real spawn-and-
    /// collect path end to end. The name is unique per call — tests run in
    /// parallel, and exec-ing a file another test still holds open for
    /// writing fails with `Text file busy`.
    fn fake_min(body: &str, tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "min-subagent-test-{}-{tag}-{n}",
            std::process::id()
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn instruction_goes_to_stdin_parent_to_args_output_comes_back() {
        let child = fake_min("echo \"parent: $2\"\ncat\necho done", "ok");
        let out = collect(&child, "1768-abc1", "write the tests", 10).unwrap();
        assert!(out.contains("exit code: 0"), "{out}");
        assert!(out.contains("parent: 1768-abc1"), "{out}");
        assert!(out.contains("write the tests"), "{out}");
        assert!(out.contains("done"), "{out}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn nonzero_exit_is_an_error_that_still_carries_the_output() {
        let child = fake_min("cat\necho partial work\necho more >&2\nexit 3", "fail");
        let err = collect(&child, "p", "keep going", 10).unwrap_err();
        assert!(err.contains("exit code: 3"), "{err}");
        assert!(err.contains("partial work"), "{err}");
        assert!(err.contains("--- stderr ---"), "{err}");
        assert!(err.contains("keep going"), "{err}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn timeout_kills_a_stuck_subagent() {
        let child = fake_min("sleep 30", "stuck");
        let t0 = Instant::now();
        let err = collect(&child, "p", "hurry", 1).unwrap_err();
        assert!(err.contains("timed out after 1s"), "{err}");
        assert!(t0.elapsed().as_secs() < 10, "killed promptly");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn stderr_only_output_still_reaches_the_parent() {
        let child = fake_min("echo oops >&2", "quiet");
        let out = collect(&child, "p", "say nothing", 10).unwrap();
        assert!(out.contains("exit code: 0"), "{out}");
        assert!(out.contains("oops"), "{out}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn huge_output_is_truncated_but_head_and_tail_survive() {
        let child = fake_min("seq 1 100000; cat >/dev/null", "big");
        let out = collect(&child, "p", "count", 10).unwrap();
        assert!(out.len() < 26_000, "len={}", out.len());
        assert!(out.contains("1\n"), "{out}");
        assert!(out.contains("100000"), "{out}");
        assert!(out.contains("truncated"), "{out}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn tool_needs_a_session_and_an_instruction() {
        let tool = SubagentTool { session: Rc::new(RefCell::new(None)) };
        let err = tool
            .run(r#"{"instruction":"hi"}"#)
            .unwrap_err();
        assert!(err.contains("no session id yet"), "{err}");

        let tool = SubagentTool {
            session: Rc::new(RefCell::new(Some("1768-abc1".into()))),
        };
        assert!(tool.run("{}").unwrap_err().contains("missing `instruction`"));
        assert!(tool.run("not json").unwrap_err().contains("bad input"));
    }

    #[test]
    fn session_events_update_the_parent_passed_to_children() {
        // The plugin keeps the latest announced id; the tool reads it at
        // call time, so a subagent spawned after /clear parents on the new
        // session. No service, no imports — the event payload only.
        let mut plugin = SubagentsPlugin::new();
        let hub = Hub::new(vec![]);
        let tool_session = plugin.session.clone();

        plugin.handle(&hub, &events::SessionStart { id: "1-first".into(), parent: None });
        assert_eq!(tool_session.borrow().as_deref(), Some("1-first"));

        plugin.handle(&hub, &events::SessionStart { id: "2-second".into(), parent: None });
        assert_eq!(tool_session.borrow().as_deref(), Some("2-second"));

        plugin.handle(&hub, &events::UserInput { text: "unrelated".into() });
        assert_eq!(tool_session.borrow().as_deref(), Some("2-second"));
    }
}
