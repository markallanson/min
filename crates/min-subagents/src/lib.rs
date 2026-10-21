//! min-subagents — the `subagent` family: `subagent`, `subagent_spawn`,
//! `subagent_collect`.
//!
//! Spawns a fresh `min` process as a *subagent*: a whole harness — its own
//! session, its own conversation, its own tools — given one instruction on
//! stdin, and reports its captured output back as the tool result.
//!
//! The pieces were all already there; this crate only wires them together:
//!
//! - **parenting** — `min --parent-session <id>` (min-session) makes the
//!   child's first session a child of this process's session, so the
//!   subagent's `.jsonl` file (min-session) records its parent;
//! - **the instruction** — the child's repl reads non-tty stdin line by
//!   line (`echo 'hello' | min`): one instruction line becomes one turn,
//!   and EOF after it quits cleanly — so a spawn plus a write plus a close
//!   *is* "one instruction, one turn, exit";
//! - **the result** — everything the subagent prints on stdout/stderr
//!   (its streamed answer, tool activity, errors) is captured while it
//!   works and handed back to the parent's model when it exits.
//!
//! ## One call or many
//!
//! `subagent` runs one child to completion (spawn + collect in one call).
//! For concurrency, `subagent_spawn` returns a handle immediately and
//! `subagent_collect` joins one or more handles — so a batch of subagents
//! runs in wall-clock parallel *inside these tools* (they are separate
//! processes), while everything around the tool calls stays synchronous
//! and in-order: min-agent's `for call in calls` loop is untouched, and
//! the core stays a single-threaded loop.

use min_core::{events, Hub, Plugin, Tool};
use serde_json::{json, Value};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long a subagent may run before it is killed (model can override,
/// clamped to this max). Subagents do whole tasks — minutes, not seconds —
/// so the default is generous.
const DEFAULT_TIMEOUT: u64 = 600;
const MAX_TIMEOUT: u64 = 3600;

/// Live children, keyed by the decimal handle returned by
/// `subagent_spawn`. Owned by the plugin; shared with the tools behind
/// `Rc<RefCell<…>>` (the tools live in the core's registry while the
/// plugin keeps receiving events — same pattern as the session id).
type JobTable = HashMap<String, Job>;

pub struct SubagentsPlugin {
    /// This process's session id, as announced by min-session's
    /// [`events::SessionStart`]. Passed to each child as
    /// `--parent-session`, so subagent sessions are children of ours.
    /// `None` until the first announcement — the events *are* the session
    /// interface; there is no session service.
    session: Rc<RefCell<Option<String>>>,
    /// Children spawned but not yet collected.
    jobs: Rc<RefCell<JobTable>>,
    /// Monotonic handle source. Handles are never reused, so a handle the
    /// parent still holds can only ever mean the child it was issued for —
    /// even after earlier handles have been collected and removed.
    next_handle: Rc<Cell<u64>>,
    /// The executable the tools spawn. `None` means `current_exe()` — a
    /// child is a whole min, same binary as us. Overridable so tests can
    /// run the full tool protocol against a stand-in script (under cargo
    /// test, `current_exe()` is the test binary, which would reject
    /// `--parent-session`).
    exe: Option<std::path::PathBuf>,
}

impl SubagentsPlugin {
    pub fn new() -> Self {
        SubagentsPlugin {
            session: Rc::new(RefCell::new(None)),
            jobs: Rc::new(RefCell::new(HashMap::new())),
            next_handle: Rc::new(Cell::new(0)),
            exe: None,
        }
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
        hub.tools().register(Box::new(SubagentTool {
            session: self.session.clone(),
            exe: self.exe.clone(),
        }));
        hub.tools().register(Box::new(SpawnTool {
            session: self.session.clone(),
            jobs: self.jobs.clone(),
            next_handle: self.next_handle.clone(),
            exe: self.exe.clone(),
        }));
        hub.tools().register(Box::new(CollectTool { jobs: self.jobs.clone() }));
    }

    fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
        // Keep the latest session id: a subagent spawned after `/clear`
        // parents on the *new* session.
        if let Some(e) = event.downcast_ref::<events::SessionStart>() {
            *self.session.borrow_mut() = Some(e.id.clone());
        }
        // Shutdown / cancellation sweep: spawn's contract ends at collect, but
        // an interrupted turn or quitting parent must not leave orphans holding
        // pipes. Every still-live job gets the group kill + reap + drain-join.
        if event.downcast_ref::<events::Quit>().is_some()
            || event.downcast_ref::<events::TurnCancelled>().is_some()
        {
            for job in self.jobs.borrow_mut().values_mut() {
                let _ = job.kill("cancelled — subagent killed");
            }
        }
    }
}

/// One spawned child: the process, the deadline its spawn call committed
/// to, its drain threads, and — once some code path has waited it out —
/// the formatted result. Collect removes the job from the table when it
/// waits, so in practice nothing re-reads `output`; it exists so a kill
/// (deadline, `any` cancellation, quit sweep) leaves one authoritative
/// answer on the record instead of a half-reaped process.
struct Job {
    child: Child,
    timeout: u64,
    deadline: Instant,
    output: RefCell<Option<Result<String, String>>>,
    t_out: Option<JoinHandle<String>>,
    t_err: Option<JoinHandle<String>>,
}

impl Job {
    /// Kill the whole process group, then reap + join + format via
    /// [`finish`]. Used on the deadline (via [`wait_job`]) and by the
    /// plugin's `Quit` sweep. The reply — always an `Err`, a killed
    /// subagent did not finish its job — is recorded in `output` so a
    /// second reader of this job sees the same answer.
    fn kill(&mut self, why: &str) -> Result<String, String> {
        kill_group(&mut self.child);
        let reply = match finish(self) {
            Ok(s) | Err(s) => format!("{why}\n{s}"),
        };
        *self.output.borrow_mut() = Some(Err(reply.clone()));
        Err(reply)
    }
}

/// Kill the child's whole process group (it leads its own group, so this
/// takes anything it spawned with it), then the child itself.
fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
}

/// Spawn `exe --parent-session <parent>` with piped stdio and its own
/// process group (so a timeout can kill the whole tree). Executing a file
/// written moments earlier can fail with a transient `ETXTBSY` — the
/// kernel refuses to exec anything still held open for writing, and the
/// writer's close can lose a race with layered FS flushing — so a short
/// bounded retry is applied. That is also what makes the tests
/// deterministic: they spawn scripts they have just written.
fn spawn_subagent(exe: &Path, parent: &str) -> std::io::Result<Child> {
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
/// stdin — and hand it back running. The caller picks the wait policy:
/// `SubagentTool` waits immediately (the original one-call behaviour),
/// `SpawnTool` returns the handle at once.
fn spawn_job(
    exe: &Path,
    parent: &str,
    instruction: &str,
    timeout: u64,
) -> Result<Job, String> {
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

    Ok(Job {
        child,
        timeout,
        deadline: Instant::now() + Duration::from_secs(timeout),
        output: RefCell::new(None),
        t_out: Some(t_out),
        t_err: Some(t_err),
    })
}

/// Reap the child, join the drain threads, stitch stderr under a marker,
/// and format the reply. The one tail shared by every wait path (collect,
/// deadline kill, quit sweep). `Ok` only on exit 0; anything else is an
/// `Err` that still carries everything the subagent printed, so the
/// parent's model sees how far it got.
fn finish(job: &mut Job) -> Result<String, String> {
    let status = job.child.wait().map_err(|e| format!("wait: {e}"))?;
    let mut out = job
        .t_out
        .take()
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    let err = job
        .t_err
        .take()
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    if !err.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("--- stderr ---\n");
        out.push_str(&err);
    }

    let code = status.code().unwrap_or(-1);
    let mut reply = format!("exit code: {code}\n");
    reply.push_str(&truncate(&out, 24_000));
    if out.trim().is_empty() {
        reply.push_str("(no output)\n");
    }
    if code == 0 {
        Ok(reply)
    } else {
        Err(reply)
    }
}

/// Wait until the job exits or its deadline passes; past the deadline,
/// kill the whole process group — an orphaned grandchild holding the
/// output pipes would otherwise keep the drains (and min) blocked long
/// past it. Records the formatted result in `job.output`.
fn wait_job(job: &mut Job) -> Result<String, String> {
    loop {
        if min_core::is_cancelled() {
            return job.kill("cancelled by user");
        }
        match job.child.try_wait() {
            Ok(Some(_)) => {
                let r = finish(job);
                *job.output.borrow_mut() = Some(r.clone());
                return r;
            }
            Ok(None) if Instant::now() >= job.deadline => {
                return job.kill(&format!(
                    "timed out after {}s (subagent killed)",
                    job.timeout
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
}

fn drain(r: Option<impl Read>) -> String {
    let mut s = String::new();
    if let Some(r) = r {
        let _ = std::io::BufReader::new(r).read_to_string(&mut s);
    }
    s
}

/// Cap a captured output at `cap` chars, keeping the head ¾ and the tail
/// ¼ so both the beginning of the turn and its conclusion survive — same
/// policy as the bash tool.
fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let total_chars = s.chars().count();
    let head_chars = cap * 3 / 4;
    let tail_chars = cap / 4;
    let head: String = s.chars().take(head_chars).collect();
    let tail: String = s
        .chars()
        .skip(total_chars.saturating_sub(tail_chars))
        .collect();
    format!(
        "{head}\n…[truncated {} chars]…\n{tail}",
        total_chars - head_chars - tail_chars
    )
}

// ---------------------------------------------------------------------------
// the three tools
// ---------------------------------------------------------------------------

/// `subagent` — spawn + wait in one call. The original tool; sugar for
/// `subagent_spawn` + `subagent_collect`. It never registers the job in
/// the table: nobody else could act on it while this call blocks.
struct SubagentTool {
    session: Rc<RefCell<Option<String>>>,
    exe: Option<std::path::PathBuf>,
}

/// `subagent_spawn` — spawn and return a handle at once.
struct SpawnTool {
    session: Rc<RefCell<Option<String>>>,
    jobs: Rc<RefCell<JobTable>>,
    next_handle: Rc<Cell<u64>>,
    exe: Option<std::path::PathBuf>,
}

/// `subagent_collect` — wait for one or more spawned handles.
struct CollectTool {
    jobs: Rc<RefCell<JobTable>>,
}

fn parse_timeout(v: &Value) -> Result<u64, String> {
    Ok(v["timeout_secs"].as_u64().unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT))
}

/// The executable to spawn: the plugin's override, else this very binary —
/// a subagent is a whole min.
fn exe_or_self(exe: &Option<std::path::PathBuf>) -> Result<std::path::PathBuf, String> {
    exe.clone().map(Ok).unwrap_or_else(|| {
        std::env::current_exe().map_err(|e| format!("current_exe: {e}"))
    })
}

impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn spec(&self) -> &'static str {
        r#"{"description":"Spawn a subagent: a fresh min process with its own session (a child of this one) and its own tools, given one instruction. It works with no other context than the instruction — include the goal, relevant paths, and what a good result looks like. Blocks until the subagent's turn ends, then returns its output. Runs one subagent to completion; to run several concurrently, prefer subagent_spawn (returns a handle at once) plus one subagent_collect for all handles. A subagent has these tools too — instruct it to work directly unless delegation is truly needed.","input_schema":{"type":"object","properties":{"instruction":{"type":"string","description":"the complete task for the subagent"},"timeout_secs":{"type":"integer","description":"kill the subagent after this many seconds (default 600, max 3600)"}},"required":["instruction"]}}"#
    }

    fn run(&self, input: &str) -> Result<String, String> {
        if min_core::is_cancelled() {
            return Err("cancelled by user".into());
        }
        let v: Value = serde_json::from_str(input).map_err(|e| format!("bad input: {e}"))?;
        let instruction = v["instruction"].as_str().ok_or("missing `instruction`")?;
        let timeout = parse_timeout(&v)?;
        let exe = exe_or_self(&self.exe)?;
        let parent = self
            .session
            .borrow()
            .clone()
            .ok_or("no session id yet — is the session plugin loaded?")?;
        let mut job = spawn_job(&exe, &parent, instruction, timeout)?;
        wait_job(&mut job)
    }
}

impl Tool for SpawnTool {
    fn name(&self) -> &'static str {
        "subagent_spawn"
    }

    fn spec(&self) -> &'static str {
        r#"{"description":"Spawn a subagent WITHOUT waiting: a fresh min process (its own session, a child of this one; its own tools) starts on the instruction and you get back a handle like `3` immediately. Spawns run in wall-clock parallel with each other and with your own tool calls, because each is a separate process. You MUST later pass every handle to subagent_collect to get results. The instruction is the subagent's only context — include the goal, relevant paths, and what a good result looks like.","input_schema":{"type":"object","properties":{"instruction":{"type":"string","description":"the complete task for the subagent"},"timeout_secs":{"type":"integer","description":"kill the subagent after this many seconds (default 600, max 3600)"}},"required":["instruction"]}}"#
    }

    fn run(&self, input: &str) -> Result<String, String> {
        if min_core::is_cancelled() {
            return Err("cancelled by user".into());
        }
        let v: Value = serde_json::from_str(input).map_err(|e| format!("bad input: {e}"))?;
        let instruction = v["instruction"].as_str().ok_or("missing `instruction`")?;
        let timeout = parse_timeout(&v)?;
        let exe = exe_or_self(&self.exe)?;
        let parent = self
            .session
            .borrow()
            .clone()
            .ok_or("no session id yet — is the session plugin loaded?")?;
        let job = spawn_job(&exe, &parent, instruction, timeout)?;
        let handle = self.next_handle.get() + 1;
        self.next_handle.set(handle);
        let handle = handle.to_string();
        self.jobs.borrow_mut().insert(handle.clone(), job);
        Ok(format!("handle: {handle} (running) — pass it to subagent_collect for the result"))
    }
}

impl Tool for CollectTool {
    fn name(&self) -> &'static str {
        "subagent_collect"
    }

    fn spec(&self) -> &'static str {
        r#"{"description":"Wait for subagents spawned with subagent_spawn and return their results. mode \"all\" (default) waits for every handle; mode \"any\" returns as soon as one succeeds and kills the rest. Results come back as a JSON array of {handle, ok, result}.","input_schema":{"type":"object","properties":{"handles":{"type":"array","items":{"type":"string"},"description":"handles from subagent_spawn (a single handle as a bare string also works)"},"mode":{"type":"string","enum":["all","any"],"description":"all (default): wait for every handle; any: first success wins"}},"required":["handles"]}}"#
    }

    fn run(&self, input: &str) -> Result<String, String> {
        let v: Value = serde_json::from_str(input).map_err(|e| format!("bad input: {e}"))?;
        let handles: Vec<String> = match &v["handles"] {
            Value::Array(a) => a
                .iter()
                .map(|h| h.as_str().map(String::from).ok_or("handles: expected strings"))
                .collect::<Result<_, _>>()?,
            Value::String(s) => vec![s.clone()],
            Value::Null => return Err("missing `handles`".into()),
            _ => return Err("`handles` must be a string or an array of strings".into()),
        };
        if handles.is_empty() {
            return Err("no handles given".into());
        }
        let any = v["mode"].as_str() == Some("any");

        // Take the requested jobs out of the table up front: none of them
        // may sit in the map across a wait, where a second collect (or the
        // quit sweep) could see them and double-wait.
        let mut waiting: Vec<(String, Job)> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        {
            let mut jobs = self.jobs.borrow_mut();
            for h in &handles {
                match jobs.remove(h) {
                    Some(job) => waiting.push((h.clone(), job)),
                    None => missing.push(h.clone()),
                }
            }
        }

        // Mode "any" round-robins so a slow first handle cannot starve a
        // fast second one; the first success becomes the winner and the
        // rest of the batch is killed. Mode "all" (the default) waits each
        // job out in order — with the whole batch spawned up front, that is
        // still wall-clock parallel.
        let mut results: Vec<Value> = Vec::new();
        let mut winner: Option<String> = None;
        if any {
            while !waiting.is_empty() && winner.is_none() {
                if min_core::is_cancelled() {
                    break;
                }
                let mut i = 0;
                while i < waiting.len() && winner.is_none() {
                    if min_core::is_cancelled() {
                        break;
                    }
                    if waiting[i].1.child.try_wait().is_ok_and(|s| s.is_some()) {
                        let (handle, mut job) = waiting.swap_remove(i);
                        let output = finish(&mut job);
                        *job.output.borrow_mut() = Some(output.clone());
                        if output.is_ok() {
                            winner = Some(handle.clone());
                        }
                        results.push(json!({
                            "handle": handle,
                            "ok": output.is_ok(),
                            "result": output.unwrap_or_else(|e| e),
                        }));
                    } else if Instant::now() >= waiting[i].1.deadline {
                        let (handle, mut job) = waiting.swap_remove(i);
                        let output = job.kill(&format!(
                            "timed out after {}s (subagent killed)",
                            job.timeout
                        ));
                        results.push(
                            json!({"handle": handle, "ok": false, "result": output.unwrap_err()}),
                        );
                    } else {
                        i += 1;
                    }
                }
                if winner.is_none() && !waiting.is_empty() {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            if min_core::is_cancelled() {
                for (handle, mut job) in waiting.drain(..) {
                    let output = job.kill("cancelled by user");
                    results.push(json!({"handle": handle, "ok": false, "result": output.unwrap_err()}));
                }
                return Err("cancelled by user".into());
            }
            for (handle, mut job) in waiting.drain(..) {
                let output =
                    job.kill("cancelled: subagent_collect mode \"any\" already returned");
                results.push(json!({"handle": handle, "ok": false, "result": output.unwrap_err()}));
            }
        } else {
            for (handle, mut job) in waiting {
                if min_core::is_cancelled() {
                    let output = job.kill("cancelled by user");
                    results.push(json!({
                        "handle": handle,
                        "ok": false,
                        "result": output.unwrap_err(),
                    }));
                } else {
                    let output = wait_job(&mut job);
                    results.push(json!({
                        "handle": handle,
                        "ok": output.is_ok(),
                        "result": output.unwrap_or_else(|e| e),
                    }));
                }
            }
            if min_core::is_cancelled() {
                return Err("cancelled by user".into());
            }
        }

        let mut reply = json!(results).to_string();
        if !missing.is_empty() {
            reply.push_str(&format!(
                "\n(no such handle: {} — already collected, or never spawned)",
                missing.join(", ")
            ));
        }
        if results.is_empty() {
            return Err(reply); // nothing collected: handles unknown/never spawned
        }
        let all_ok = results.iter().all(|r| r["ok"] == json!(true));
        if any {
            if winner.is_some() {
                Ok(reply)
            } else {
                Err(reply)
            }
        } else if all_ok {
            Ok(reply)
        } else {
            Err(reply)
        }
    }
}

// ---------------------------------------------------------------------------
// tests — the real spawn path against a stand-in executable
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    fn lock_test() -> MutexGuard<'static, ()> {
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        min_core::reset_cancel();
        g
    }

    /// A stand-in "min": a shell script that ignores its arguments, prints
    /// what it was given on stdin (what the real child turns into a turn),
    /// and exits with the status in its body. Uses the real spawn path end
    /// to end. The name is unique per call — tests run in parallel, and
    /// exec-ing a file another test still holds open for writing fails
    /// with `Text file busy`.
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

    /// Tools wired the way `SubagentsPlugin::start` does, with the exe
    /// overridden to the stand-in script (under cargo test, `current_exe()`
    /// is the test binary, which would reject `--parent-session`).
    fn tool(session: &str, exe: &Path) -> (SubagentTool, SpawnTool, CollectTool) {
        let s = Rc::new(RefCell::new(Some(session.to_string())));
        let jobs: Rc<RefCell<JobTable>> = Rc::new(RefCell::new(HashMap::new()));
        let nh: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        (
            SubagentTool { session: s.clone(), exe: Some(exe.to_path_buf()) },
            SpawnTool {
                session: s,
                jobs: jobs.clone(),
                next_handle: nh,
                exe: Some(exe.to_path_buf()),
            },
            CollectTool { jobs },
        )
    }

    #[test]
    fn instruction_goes_to_stdin_parent_to_args_output_comes_back() {
        let _guard = lock_test();
        let child = fake_min("echo \"parent: $2\"\ncat\necho done", "ok");
        let (sub, _, _) = tool("1768-abc1", &child);
        let out = sub
            .run(r#"{"instruction":"write the tests","timeout_secs":10}"#)
            .unwrap();
        assert!(out.contains("exit code: 0"), "{out}");
        assert!(out.contains("parent: 1768-abc1"), "{out}");
        assert!(out.contains("write the tests"), "{out}");
        assert!(out.contains("done"), "{out}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn nonzero_exit_is_an_error_that_still_carries_the_output() {
        let _guard = lock_test();
        let child = fake_min("cat\necho partial work\necho more >&2\nexit 3", "fail");
        let (sub, _, _) = tool("p", &child);
        let err = sub
            .run(r#"{"instruction":"keep going","timeout_secs":10}"#)
            .unwrap_err();
        assert!(err.contains("exit code: 3"), "{err}");
        assert!(err.contains("partial work"), "{err}");
        assert!(err.contains("--- stderr ---"), "{err}");
        assert!(err.contains("keep going"), "{err}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn timeout_kills_a_stuck_subagent() {
        let _guard = lock_test();
        let child = fake_min("sleep 30", "stuck");
        let (sub, _, _) = tool("p", &child);
        let t0 = Instant::now();
        let err = sub
            .run(r#"{"instruction":"hurry","timeout_secs":1}"#)
            .unwrap_err();
        assert!(err.contains("timed out after 1s"), "{err}");
        assert!(t0.elapsed().as_secs() < 10, "killed promptly");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn stderr_only_output_still_reaches_the_parent() {
        let _guard = lock_test();
        let child = fake_min("echo oops >&2", "quiet");
        let (sub, _, _) = tool("p", &child);
        let out = sub.run(r#"{"instruction":"say nothing","timeout_secs":10}"#).unwrap();
        assert!(out.contains("exit code: 0"), "{out}");
        assert!(out.contains("oops"), "{out}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn huge_output_is_truncated_but_head_and_tail_survive() {
        let _guard = lock_test();
        let child = fake_min("seq 1 100000; cat >/dev/null", "big");
        let (sub, _, _) = tool("p", &child);
        let out = sub.run(r#"{"instruction":"count","timeout_secs":10}"#).unwrap();
        assert!(out.len() < 26_000, "len={}", out.len());
        assert!(out.contains("1\n"), "{out}");
        assert!(out.contains("100000"), "{out}");
        assert!(out.contains("truncated"), "{out}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn tool_needs_a_session_and_an_instruction() {
        let _guard = lock_test();
        let child = fake_min("true", "sess");
        let s: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let jobs: Rc<RefCell<JobTable>> = Rc::new(RefCell::new(HashMap::new()));
        let exe = Some(child.clone());
        let sub = SubagentTool { session: s.clone(), exe: exe.clone() };
        let spawn = SpawnTool { session: s, jobs, next_handle: Rc::new(Cell::new(0)), exe };
        let err = sub.run(r#"{"instruction":"hi"}"#).unwrap_err();
        assert!(err.contains("no session id yet"), "{err}");
        let err = spawn.run(r#"{"instruction":"hi"}"#).unwrap_err();
        assert!(err.contains("no session id yet"), "{err}");

        let (sub, spawn, _) = tool("1768-abc1", &child);
        assert!(sub.run("{}").unwrap_err().contains("missing `instruction`"));
        assert!(sub.run("not json").unwrap_err().contains("bad input"));
        assert!(spawn.run("{}").unwrap_err().contains("missing `instruction`"));
        assert!(spawn.run("not json").unwrap_err().contains("bad input"));
        let _ = std::fs::remove_file(&child);
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

    #[test]
    fn spawn_returns_a_handle_at_once_and_collect_joins_the_result() {
        let _guard = lock_test();
        let child = fake_min("echo \"parent: $2\"\ncat", "spawn");
        let (_, spawn, collect) = tool("p1", &child);
        let reply = spawn
            .run(r#"{"instruction":"background work","timeout_secs":10}"#)
            .unwrap();
        assert!(reply.starts_with("handle: 1 (running)"), "{reply}");

        // The tool returned before the child finished — that is the point.
        // Collect then waits it out and reports the same shape as a
        // one-call `subagent`.
        let out = collect.run(r#"{"handles":["1"]}"#).unwrap();
        assert!(out.contains("\"handle\":\"1\""), "{out}");
        assert!(out.contains("\"ok\":true"), "{out}");
        assert!(out.contains("parent: p1"), "{out}");
        assert!(out.contains("background work"), "{out}");
        let _ = std::fs::remove_file(&child);

        // Collected handles are gone from the table.
        let err = collect.run(r#"{"handles":["1"]}"#).unwrap_err();
        assert!(err.contains("no such handle: 1"), "{err}");
    }

    #[test]
    fn two_spawns_run_in_wall_clock_parallel() {
        let _guard = lock_test();
        let child = fake_min("sleep 1; cat", "par");
        let (_, spawn, collect) = tool("p2", &child);
        let t0 = Instant::now();
        for _ in 0..2 {
            spawn.run(r#"{"instruction":"nap","timeout_secs":10}"#).unwrap();
        }
        let spawned = t0.elapsed();
        let out = collect.run(r#"{"handles":["1","2"]}"#).unwrap();
        let waited = t0.elapsed() - spawned;
        assert!(out.matches("\"ok\":true").count() == 2, "{out}");
        // Two 1s naps, joined together: the collect waits ~1s, not ~2s.
        assert!(waited.as_secs() < 2, "collect took {waited:?} — not parallel");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn collect_reports_missing_handles() {
        let _guard = lock_test();
        let (_, _, collect) = tool("p3", &fake_min("true", "p3"));
        let err = collect.run(r#"{"handles":["9"]}"#).unwrap_err();
        assert!(err.contains("no such handle: 9"), "{err}");
        assert!(collect.run(r#"{"handles":[]}"#).unwrap_err().contains("no handles"));
        assert!(collect.run(r#"{}"#).unwrap_err().contains("missing `handles`"));
    }

    #[test]
    fn handles_are_never_reused_after_a_collect() {
        let _guard = lock_test();
        let child = fake_min("cat", "reuse");
        let (_, spawn, collect) = tool("p6", &child);
        spawn.run(r#"{"instruction":"one","timeout_secs":10}"#).unwrap();
        collect.run(r#"{"handles":["1"]}"#).unwrap();
        // The table is empty again — the next spawn must still not become
        // handle 1, or a stale handle could collect the wrong child.
        let reply = spawn.run(r#"{"instruction":"two","timeout_secs":10}"#).unwrap();
        assert!(reply.starts_with("handle: 2"), "{reply}");
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn collect_mode_any_returns_the_first_success_and_kills_the_rest() {
        let _guard = lock_test();
        let slow = fake_min("sleep 30; cat", "any-slow");
        let fast = fake_min("echo quick; cat", "any-fast");
        let s = Rc::new(RefCell::new(Some("p4".to_string())));
        let jobs: Rc<RefCell<JobTable>> = Rc::new(RefCell::new(HashMap::new()));
        let nh: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        // One table, two exes: handle 1 runs the slow script, handle 2 the
        // fast one — `any` must return for the fast one without waiting.
        let spawn_slow =
            SpawnTool {
                session: s.clone(),
                jobs: jobs.clone(),
                next_handle: nh.clone(),
                exe: Some(slow.clone()),
            };
        let spawn_fast =
            SpawnTool {
                session: s,
                jobs: jobs.clone(),
                next_handle: nh.clone(),
                exe: Some(fast.clone()),
            };
        let collect = CollectTool { jobs: jobs.clone() };
        spawn_slow.run(r#"{"instruction":"stall","timeout_secs":30}"#).unwrap();
        spawn_fast.run(r#"{"instruction":"answer","timeout_secs":30}"#).unwrap();
        let t0 = Instant::now();
        let out = collect.run(r#"{"handles":["1","2"],"mode":"any"}"#).unwrap();
        assert!(t0.elapsed().as_secs() < 10, "returned early");
        assert!(out.contains("quick"), "{out}");
        // The loser was killed and reported, with its spawn visible.
        assert!(out.contains("cancelled"), "{out}");
        assert!(out.contains("\"ok\":false"), "{out}");
        let _ = std::fs::remove_file(&slow);
        let _ = std::fs::remove_file(&fast);
    }

    #[test]
    fn quit_kills_every_live_job() {
        let _guard = lock_test();
        let child = fake_min("sleep 30; cat", "quit");
        let mut plugin = SubagentsPlugin::new();
        *plugin.session.borrow_mut() = Some("p5".into());
        let hub = Hub::new(vec![]);
        let jobs = plugin.jobs.clone();

        let spawn = SpawnTool {
            session: plugin.session.clone(),
            jobs: jobs.clone(),
            next_handle: plugin.next_handle.clone(),
            exe: Some(child.clone()),
        };
        let reply = spawn
            .run(r#"{"instruction":"forever","timeout_secs":60}"#)
            .unwrap();
        assert!(reply.starts_with("handle: 1"), "{reply}");

        // The sweep runs on Quit — through the real handle, like the core.
        plugin.handle(&hub, &events::Quit {});
        assert!(jobs.borrow().get("1").unwrap().output.borrow().is_some());

        // The child is gone: reap without blocking long.
        let mut table = jobs.borrow_mut();
        let mut job = table.remove("1").unwrap();
        let t0 = Instant::now();
        let _ = job.child.wait();
        assert!(t0.elapsed().as_secs() < 5, "child was killed, not left running");
        drop(table);
        let _ = std::fs::remove_file(&child);
    }

    #[test]
    fn cancels_running_subagent() {
        let _guard = lock_test();
        let child = fake_min("sleep 30; cat", "sub-cancel");
        let (sub, _, _) = tool("p", &child);
        let handle = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            min_core::cancel();
        });
        let t0 = Instant::now();
        let err = sub
            .run(r#"{"instruction":"long task","timeout_secs":60}"#)
            .unwrap_err();
        handle.join().unwrap();
        assert!(err.contains("cancelled"));
        assert!(t0.elapsed().as_secs() < 5, "cancelled promptly");
        let _ = std::fs::remove_file(&child);
        min_core::reset_cancel();
    }

    #[test]
    fn turn_cancelled_kills_every_live_job() {
        let _guard = lock_test();
        let child = fake_min("sleep 30; cat", "turn-cancel");
        let mut plugin = SubagentsPlugin::new();
        *plugin.session.borrow_mut() = Some("p-cancel".into());
        let hub = Hub::new(vec![]);
        let jobs = plugin.jobs.clone();

        let spawn = SpawnTool {
            session: plugin.session.clone(),
            jobs: jobs.clone(),
            next_handle: plugin.next_handle.clone(),
            exe: Some(child.clone()),
        };
        let reply = spawn
            .run(r#"{"instruction":"forever","timeout_secs":60}"#)
            .unwrap();
        assert!(reply.starts_with("handle: 1"), "{reply}");

        plugin.handle(&hub, &events::TurnCancelled {});
        assert!(jobs.borrow().get("1").unwrap().output.borrow().is_some());

        let mut table = jobs.borrow_mut();
        let mut job = table.remove("1").unwrap();
        let t0 = Instant::now();
        let _ = job.child.wait();
        assert!(t0.elapsed().as_secs() < 5, "child was killed on turn cancellation");
        drop(table);
        let _ = std::fs::remove_file(&child);
    }
}
