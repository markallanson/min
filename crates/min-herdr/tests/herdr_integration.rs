//! Integration test: min-herdr against a fake `herdr` CLI.
//!
//! Builds a shell script that appends its arguments to a log file, points
//! `HERDR_BIN_PATH` at it, and drives the real plugin through the real
//! `min_core::run` loop with a driver plugin emitting min's lifecycle
//! events. The assertions are the exact argv Herdr would receive, in
//! order — the whole CLI contract, end to end.
//!
//! Unix-only (the fake is a `#!/bin/sh` script).

#![cfg(unix)]

use min_core::{events, Hub, Plugin};
use std::any::Any;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

/// Env vars are process-global and read inside `start()`; tests that touch
/// them must not interleave (cargo runs a binary's tests in parallel).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Emits one event per herdr state, then quits through `wait` like the
/// repl does — so the run loop exercises the release path on exit.
struct Driver;

impl Plugin for Driver {
    fn name(&self) -> &'static str {
        "driver"
    }
    fn start(&mut self, hub: &Hub) {
        // Dispatched in load order, after herdr's start(), so the plugin's
        // env gate is already resolved — same as production.
        hub.emit(events::UserInput { text: "hi".into() }); // → working
        hub.emit(events::MessageEnd {}); // → idle
        hub.emit(events::Error { text: "stuck".into() }); // → blocked
    }
    fn wait(&mut self, _hub: &Hub) -> Option<Box<dyn Any>> {
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

/// The log line the plugin emits for one state, seq `n`.
fn report_line(n: u64, state: &str) -> String {
    format!("pane report-agent pane-test --source custom:min --agent min --seq {n} --state {state}")
}

const RELEASE_LINE: &str = "pane release-agent pane-test --source custom:min --agent min";

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

fn harness() -> Vec<Box<dyn Plugin>> {
    vec![
        Box::new(min_herdr::HerdrPlugin::new()), // production load order
        Box::new(Driver),
    ]
}

#[test]
fn registers_at_start_reports_lifecycle_then_releases() {
    let _guard = ENV_LOCK.lock().unwrap();
    let fake = install_fake_herdr("flow");
    set_herdr_env(&fake);

    min_core::run(harness());

    clear_herdr_env();
    let log = std::fs::read_to_string(&fake.log).unwrap();
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        vec![
            report_line(1, "idle"), // ← start(): registers before any user input
            report_line(2, "working"),
            report_line(3, "idle"),
            report_line(4, "blocked"),
            RELEASE_LINE.to_string(), // ← Quit (via wait): release
        ],
        "the full argv Herdr must receive, in report order"
    );
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
