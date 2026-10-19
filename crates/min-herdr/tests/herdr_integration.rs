//! Integration test: min-herdr against a fake `herdr` CLI.
//!
//! Builds a shell script that appends its arguments to a log file, points
//! `HERDR_BIN_PATH` at it, and drives the real plugin through the real
//! `min_core::run` loop with a driver plugin emitting min's lifecycle
//! events. The assertions are the exact argv Herdr would receive, in
//! order — the whole CLI contract, end to end (the seq is the wall clock
//! in nanoseconds, so it is asserted to be fresh and increasing rather
//! than pinned to a literal).
//!
//! Unix-only (the fake is a `#!/bin/sh` script).
//!
//! The first session announcement reaches the plugin because the harness
//! includes the real `min_session` plugin, in its production load position:
//! it announces from its first `wait`, so every report after the start-time
//! registration carries `--agent-session-id`.

#![cfg(unix)]

use min_core::{events, Hub, Plugin};
use std::any::Any;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

const SESSION_ID: &str = "1729180001-k3f9";

/// Env vars are process-global and read inside `start()`; tests that touch
/// them must not interleave (cargo runs a binary's tests in parallel).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Emits one event per herdr state — from its first `wait`, after the
/// session announcement has reached everyone, like production where nothing
/// happens before the bus is live — then quits through `wait` like the
/// repl does, so the run loop exercises the release path on exit.
struct Driver {
    fired: bool,
}

impl Plugin for Driver {
    fn name(&self) -> &'static str {
        "driver"
    }
    fn start(&mut self, _hub: &Hub) {}
    fn wait(&mut self, hub: &Hub) -> Option<Box<dyn Any>> {
        if !self.fired {
            self.fired = true;
            hub.emit(events::UserInput { text: "hi".into() }); // → working
            hub.emit(events::MessageEnd {}); // → idle
            hub.emit(events::Error { text: "stuck".into() }); // → blocked
            return None;
        }
        Some(Box::new(events::Quit {})) // → release
    }
    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if event.downcast_ref::<events::Quit>().is_some() {
            hub.quit();
        }
    }
}

/// A fake `herdr` binary: logs every invocation's argv, one line per call.
struct FakeHerdr {
    dir: std::path::PathBuf,
    log: std::path::PathBuf,
}

fn install_fake_herdr(tag: &str) -> FakeHerdr {
    let dir = std::env::temp_dir().join(format!("min-herdr-it-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("report.log");
    let bin = dir.join("herdr");
    std::fs::write(&bin, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display())).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakeHerdr { dir, log }
}

fn clear_herdr_env() {
    for k in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH", "HERDR_BIN_PATH"] {
        std::env::remove_var(k);
    }
}

fn set_herdr_env(fake: &FakeHerdr) {
    std::env::set_var("HERDR_ENV", "1");
    std::env::set_var("HERDR_PANE_ID", "pane-test");
    std::env::set_var("HERDR_SOCKET_PATH", fake.dir.join("s.sock"));
    std::env::set_var("HERDR_BIN_PATH", fake.dir.join("herdr"));
}

const RELEASE_LINE: &str = "pane release-agent pane-test --source custom:min --agent min";

fn harness() -> Vec<Box<dyn Plugin>> {
    vec![
        Box::new(min_session::SessionPlugin::new(Some(SESSION_ID.into()), None)), // announces SessionStart from first wait
        Box::new(min_herdr::HerdrPlugin::new()), // production load order
        Box::new(Driver { fired: false }),
    ]
}

/// One state report: exact flags, with the seq wildcarded — the seq is the
/// wall clock in nanoseconds, so it is asserted to be fresh (unix nanos,
/// not a small counter) and strictly increasing instead.
fn assert_report(line: &str, state: &str, session: Option<&str>, prev_seq: &mut u64) {
    let argv: Vec<&str> = line.split_whitespace().collect();
    let seq_pos = argv.iter().position(|a| *a == "--seq").expect("--seq present");
    let seq: u64 = argv[seq_pos + 1].parse().expect("--seq is a number");
    assert!(seq > *prev_seq, "seq increases strictly, also across reports");
    assert!(seq > 1_600_000_000_000_000_000, "seq is unix nanos, not 1, 2, 3…");
    *prev_seq = seq;

    let mut expected = vec![
        "pane",
        "report-agent",
        "pane-test",
        "--source",
        "custom:min",
        "--agent",
        "min",
        "--seq",
        argv[seq_pos + 1],
    ];
    if let Some(id) = session {
        expected.extend(["--agent-session-id", id]);
    }
    expected.extend(["--state", state]);
    assert_eq!(argv, expected, "report argv for state {state:?}");
}

#[test]
fn registers_at_start_reports_lifecycle_then_releases() {
    let _guard = ENV_LOCK.lock().unwrap();
    let fake = install_fake_herdr("flow");
    set_herdr_env(&fake);

    min_core::run(harness());

    clear_herdr_env();
    let log = std::fs::read_to_string(&fake.log).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines.len(),
        6,
        "register, session-tagged register, working, idle, blocked, release"
    );

    let mut prev_seq = 0;
    assert_report(lines[0], "idle", None, &mut prev_seq); // ← start(): no session id exists yet
    assert_report(lines[1], "idle", Some(SESSION_ID), &mut prev_seq); // ← SessionStart re-report
    assert_report(lines[2], "working", Some(SESSION_ID), &mut prev_seq);
    assert_report(lines[3], "idle", Some(SESSION_ID), &mut prev_seq);
    assert_report(lines[4], "blocked", Some(SESSION_ID), &mut prev_seq);
    assert_eq!(
        lines[5], RELEASE_LINE,
        "← Quit (via wait): release, unchanged"
    );
    let _ = std::fs::remove_dir_all(&fake.dir);
}

#[test]
fn without_bin_path_uses_herdr_on_path() {
    // herdr 0.8 panes export HERDR_ENV / HERDR_PANE_ID / HERDR_SOCKET_PATH
    // but not HERDR_BIN_PATH; reporting must still happen, via PATH.
    let _guard = ENV_LOCK.lock().unwrap();
    clear_herdr_env();
    let fake = install_fake_herdr("path");
    set_herdr_env(&fake);
    std::env::remove_var("HERDR_BIN_PATH");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![fake.dir.clone()];
    dirs.extend(std::env::split_paths(&path));
    std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());

    min_core::run(harness());

    std::env::set_var("PATH", path);
    clear_herdr_env();
    let log = std::fs::read_to_string(&fake.log).expect("herdr on PATH was invoked");
    assert_eq!(log.lines().count(), 6);
    assert_eq!(log.lines().last(), Some(RELEASE_LINE));
    let _ = std::fs::remove_dir_all(&fake.dir);
}

#[test]
fn a_broken_herdr_bin_never_disturbs_the_run() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_herdr_env();
    let fake = install_fake_herdr("broken");
    set_herdr_env(&fake);
    std::env::set_var("HERDR_BIN_PATH", fake.dir.join("no-such-bin"));

    // Every spawn fails; the run loop must still complete cleanly.
    min_core::run(harness());

    assert!(!fake.log.exists());
    let _ = std::fs::remove_dir_all(&fake.dir);
}

#[test]
fn is_a_complete_noop_outside_herdr() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_herdr_env(); // not even an inherited real HERDR_ENV may leak in
    let fake = install_fake_herdr("noop");

    min_core::run(harness());

    assert!(!fake.log.exists(), "nothing may be spawned outside Herdr");
    let _ = std::fs::remove_dir_all(&fake.dir);
}

#[test]
fn requires_all_variables_not_just_the_flag() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_herdr_env();
    let fake = install_fake_herdr("partial");
    std::env::set_var("HERDR_ENV", "1"); // flag set, variables missing

    min_core::run(harness());

    assert!(!fake.log.exists(), "HERDR_ENV=1 alone must not activate reporting");
    let _ = std::fs::remove_dir_all(&fake.dir);
}
