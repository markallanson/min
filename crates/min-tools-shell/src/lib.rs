//! min-tools-shell — the `bash` tool.
//!
//! Runs a command with a deadline, captures stdout+stderr on draining threads
//! (no pipe deadlocks), and truncates huge output before it reaches the
//! model's context.

use min_core::{Hub, Plugin, Tool};
use serde_json::Value;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct ShellPlugin;

impl Plugin for ShellPlugin {
    fn name(&self) -> &'static str {
        "shell"
    }
    fn start(&mut self, hub: &Hub) {
        hub.tools().register(Box::new(BashTool));
    }
}

struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn spec(&self) -> &'static str {
        r#"{"description":"Run a command in bash (inherits min's cwd and environment). Returns the exit code, stdout, then stderr. Output is truncated to ~24k chars. Avoid interactive commands; for long builds pass a higher timeout_secs.","input_schema":{"type":"object","properties":{"command":{"type":"string"},"timeout_secs":{"type":"integer","description":"kill the command after this many seconds (default 120, max 600)"}},"required":["command"]}}"#
    }
    fn run(&self, input: &str) -> Result<String, String> {
        if min_core::is_cancelled() {
            return Err("cancelled by user".into());
        }
        let v: Value = serde_json::from_str(input).map_err(|e| format!("bad input: {e}"))?;
        let cmd = v["command"].as_str().ok_or("missing `command`")?;
        let timeout = v["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 600);
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawn bash: {e}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let t_out = std::thread::spawn(move || drain(stdout));
        let t_err = std::thread::spawn(move || drain(stderr));

        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut timed_out = false;
        let mut cancelled = false;
        let status = loop {
            if min_core::is_cancelled() {
                cancelled = true;
                break None;
            }
            match child.try_wait().map_err(|e| format!("wait: {e}"))? {
                Some(s) => break Some(s),
                None if Instant::now() >= deadline => {
                    timed_out = true;
                    break None;
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        let status = match status {
            Some(s) => s,
            None => {
                kill_process(&mut child);
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

        if cancelled {
            return Err("cancelled by user".into());
        }

        let mut reply = String::new();
        if timed_out {
            reply.push_str(&format!("timed out after {timeout}s (process killed)\n"));
        }
        reply.push_str(&format!("exit code: {}\n", status.code().unwrap_or(-1)));
        reply.push_str(&truncate(&out, 24_000));
        if out.trim().is_empty() && err.trim().is_empty() {
            reply.push_str("(no output)\n");
        }
        Ok(reply)
    }
}

fn kill_process(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
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
    use serde_json::json;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    fn run(v: Value) -> String {
        let _guard = LOCK.lock().unwrap();
        min_core::reset_cancel();
        BashTool.run(&v.to_string()).unwrap()
    }

    #[test]
    fn captures_output_and_exit_code() {
        let out = run(json!({"command": "echo hello; exit 3"}));
        assert!(out.contains("exit code: 3"), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(!out.contains("timed out"), "{out}");
    }

    #[test]
    fn captures_stderr_separately() {
        let out = run(json!({"command": "echo oops >&2"}));
        assert!(out.contains("--- stderr ---"), "{out}");
        assert!(out.contains("oops"), "{out}");
    }

    #[test]
    fn kills_on_timeout() {
        let t0 = Instant::now();
        let out = run(json!({"command": "sleep 30", "timeout_secs": 1}));
        assert!(out.contains("timed out after 1s"), "{out}");
        assert!(t0.elapsed().as_secs() < 10);
    }

    #[test]
    fn truncates_huge_output() {
        let out = run(json!({"command": "seq 1 100000"}));
        assert!(out.len() < 26_000, "len={}", out.len());
        assert!(out.contains("truncated"), "len={}", out.len());
    }

    #[test]
    fn cancels_running_command() {
        let _guard = LOCK.lock().unwrap();
        min_core::reset_cancel();
        let handle = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            min_core::cancel();
        });
        let t0 = Instant::now();
        let out = BashTool.run(&json!({"command": "sleep 30"}).to_string());
        handle.join().unwrap();
        assert!(out.is_err());
        assert!(out.unwrap_err().contains("cancelled"));
        assert!(t0.elapsed().as_secs() < 5, "cancelled promptly");
        min_core::reset_cancel();
    }
}
