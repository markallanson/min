//! min-herdr — report min's agent state to [Herdr](https://github.com/herdrdev/herdr).
//!
//! A Herdr pane exports `HERDR_ENV`, `HERDR_PANE_ID` and `HERDR_SOCKET_PATH`
//! (and, in some versions, `HERDR_BIN_PATH`). When those are present, this
//! plugin reports min's lifecycle state through Herdr's CLI ("integrate your
//! own agent") — `$HERDR_BIN_PATH`, else `herdr` on `PATH`:
//!
//! ```text
//! "$HERDR_BIN_PATH" pane report-agent "$HERDR_PANE_ID" \
//!   --source custom:min --agent min --state working
//! ```
//!
//! Outside Herdr it is a complete no-op: the env gate fails and nothing is
//! ever spawned. This keeps min portable — the same binary runs inside and
//! outside a Herdr pane.
//!
//! The integration registers at `start` (one `idle` report, so the pane
//! shows min from launch — Herdr only tracks an agent once it has heard from
//! it) and is thereafter a pure listener: it maps min's events onto Herdr's
//! `idle` / `working` / `blocked` states.
//!
//! | min moment       | herdr state |
//! |------------------|-------------|
//! | `start`          | `idle` — registers the agent (no session id yet) |
//! | `SessionStart`   | re-report of the current state, tagged `--agent-session-id` |
//! | `UserInput`      | `working`   |
//! | `MessageEnd`     | `idle`      |
//! | `Error`          | `blocked`   |
//! | `SessionEnd`     | forget the id (no report) |
//! | `Quit`           | release     |
//!
//! Reports go out synchronously from `start`/`handle`; a report is one fast
//! local spawn against Herdr's socket, and Herdr is expected to keep up.
//! `--seq` is wall-clock nanoseconds — strictly increasing per report, and
//! across runs of min — so Herdr can drop stale, out-of-order reports from
//! the same source. `--agent-session-id` carries min's session id so Herdr
//! can tell min's sessions apart (e.g. across `/clear`) under the one
//! stable `custom:min` source.

use min_core::{events, Hub, Plugin};
use std::any::Any;
use std::process::Command;

/// Stable, unique source id. Herdr tracks lifecycle authority per source;
/// it must never change between runs.
const SOURCE: &str = "custom:min";
/// The agent label Herdr displays for this integration.
const AGENT: &str = "min";

pub struct HerdrPlugin {
    /// Full config once the env gate passes; `None` = not inside Herdr.
    cfg: Option<Config>,
    /// The last sequence number handed out. The next is always strictly
    /// greater: `next_seq` starts from wall-clock nanos and refuses to go
    /// backwards, so reports remain fresh even if the clock steps back
    /// (NTP correction, VM suspend) or min was killed without a `Quit` —
    /// a plain per-process counter restarting at 1 could be stale against
    /// the previous run's reports and be dropped by Herdr.
    last_seq: u64,
    /// The current min session id, as announced by [`events::SessionStart`].
    /// Sent as `--agent-session-id` so Herdr can distinguish min's sessions
    /// under the one stable source. `None` until the first announcement —
    /// the registration report at `start` necessarily goes without it,
    /// because the session plugin announces from its first `wait`, which
    /// only runs once every plugin has started.
    session: Option<String>,
    /// The last state reported. A session change does not change min's
    /// state, but the replacement id must be associated, so `SessionStart`
    /// re-reports whatever this was.
    state: &'static str,
}

struct Config {
    /// The herdr CLI used for all reports — `$HERDR_BIN_PATH`, else `herdr`.
    bin: String,
    /// `$HERDR_PANE_ID` — the pane whose state we report.
    pane: String,
}

impl HerdrPlugin {
    pub fn new() -> Self {
        HerdrPlugin { cfg: None, last_seq: 0, session: None, state: "idle" }
    }

    /// True when running inside a Herdr pane. The docs require gating on
    /// `HERDR_ENV=1` *and* the presence of the variables actually used, so
    /// the integration is a no-op everywhere else. `HERDR_BIN_PATH` is not
    /// required: herdr 0.8 panes don't export it (see [`herdr_bin`]).
    /// `HERDR_SOCKET_PATH` is — the spawned CLI inherits it to find the
    /// right Herdr session.
    fn enabled() -> bool {
        std::env::var("HERDR_ENV").as_deref() == Ok("1")
            && env_nonempty("HERDR_PANE_ID")
            && env_nonempty("HERDR_SOCKET_PATH")
    }

    /// Spawn the herdr CLI with a `report-agent` for `state`. A failed
    /// spawn is silently ignored — reporting must never disturb min itself
    /// (or this would not be a no-op outside Herdr).
    fn report(&mut self, state: &'static str) {
        let Some(cfg) = &self.cfg else { return };
        let seq = next_seq(self.last_seq, now_seq());
        self.last_seq = seq;
        self.state = state;
        let argv = report_args(cfg, seq, self.session.as_deref(), state);
        let _ = Command::new(&argv[0]).args(&argv[1..]).output();
    }

    /// Hand the pane's lifecycle authority for this source back to Herdr.
    fn release(&mut self) {
        let Some(cfg) = &self.cfg else { return };
        let argv = release_args(cfg);
        let _ = Command::new(&argv[0]).args(&argv[1..]).output();
    }
}

/// The herdr CLI to spawn: `$HERDR_BIN_PATH` when the pane exports it,
/// else `herdr` resolved on `PATH`.
fn herdr_bin(bin_path: Option<String>) -> String {
    bin_path.filter(|b| !b.is_empty()).unwrap_or_else(|| "herdr".into())
}

fn env_nonempty(key: &str) -> bool {
    std::env::var(key).map(|v| !v.is_empty()) == Ok(true)
}

/// The sequence number for a report: unix time in nanoseconds, like
/// Herdr's own integrations. Monotonic across runs of min.
fn now_seq() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Strictly greater than `last`, never below the clock: a backwards-stepping
/// clock (NTP, suspend) must not make two reports tie or regress.
fn next_seq(last: u64, now: u64) -> u64 {
    if now > last {
        now
    } else {
        last + 1
    }
}

/// One `pane report-agent` invocation, as full argv. Pure so tests can
/// assert the exact CLI contract. `session` — min's current session id,
/// when one has been announced — is passed as `--agent-session-id`.
fn report_args(cfg: &Config, seq: u64, session: Option<&str>, state: &str) -> Vec<String> {
    let mut argv = vec![
        cfg.bin.clone(),
        "pane".into(),
        "report-agent".into(),
        cfg.pane.clone(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
        "--seq".into(),
        seq.to_string(),
    ];
    if let Some(id) = session {
        argv.push("--agent-session-id".into());
        argv.push(id.to_string());
    }
    argv.push("--state".into());
    argv.push(state.to_string());
    argv
}

/// One `pane release-agent` invocation, as full argv.
fn release_args(cfg: &Config) -> Vec<String> {
    vec![
        cfg.bin.clone(),
        "pane".into(),
        "release-agent".into(),
        cfg.pane.clone(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
    ]
}

impl Default for HerdrPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for HerdrPlugin {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn start(&mut self, _hub: &Hub) {
        if Self::enabled() {
            self.cfg = Some(Config {
                bin: herdr_bin(std::env::var("HERDR_BIN_PATH").ok()),
                pane: std::env::var("HERDR_PANE_ID").unwrap_or_default(),
            });
            // Register at once, not at the first message: Herdr only tracks
            // an agent after its first report, so without this the pane would
            // show nothing until the user typed. min is idle at startup.
            // No session id yet — the session plugin announces from its
            // first `wait`, after every plugin has started; `SessionStart`
            // below re-reports under the id as soon as it arrives.
            self.report("idle");
        }
    }

    fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
        // The cfg gate makes this a no-op outside Herdr, even though
        // handle() still runs for every event.
        if self.cfg.is_none() {
            return;
        }
        if let Some(e) = event.downcast_ref::<events::SessionStart>() {
            // Remember the id (the session plugin's prescribed pattern:
            // keep the last id you saw), then re-report the current state
            // so Herdr associates it with the new session — a `/clear`
            // does not otherwise produce a report.
            self.session = Some(e.id.clone());
            self.report(self.state);
        } else if event.downcast_ref::<events::SessionEnd>().is_some() {
            // The id is gone; reports in the gap (there are none — End is
            // immediately followed by Start or Quit) must not carry it.
            self.session = None;
        } else if event.downcast_ref::<events::UserInput>().is_some() {
            self.report("working");
        } else if event.downcast_ref::<events::MessageEnd>().is_some() {
            self.report("idle");
        } else if event.downcast_ref::<events::Error>().is_some() {
            self.report("blocked");
        } else if event.downcast_ref::<events::Quit>().is_some() {
            // When min exits (any path that reaches the bus: /quit, ctrl-d,
            // ctrl-c at the prompt), release the same source's lifecycle
            // authority. The repl calls hub.quit() on this same event, so
            // this runs once, at the very end.
            self.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        // A bin that cannot exist: stateful tests below drive `report`, and
        // the failed spawn must stay a silent no-op (never touch a real
        // herdr that might be installed on the test machine).
        Config { bin: "./min-herdr-test-no-such-bin".into(), pane: "pane-7".into() }
    }

    #[test]
    fn report_argv_matches_herdr_cli_contract() {
        let argv = report_args(&cfg(), 3, Some("1729180001-k3f9"), "working");
        assert_eq!(
            argv,
            vec![
                "./min-herdr-test-no-such-bin", "pane", "report-agent", "pane-7",
                "--source", "custom:min",
                "--agent", "min",
                "--seq", "3",
                "--agent-session-id", "1729180001-k3f9",
                "--state", "working",
            ]
        );
    }

    #[test]
    fn report_argv_without_a_session_has_no_session_flag() {
        // The registration report at `start` — no SessionStart can have run.
        let argv = report_args(&cfg(), 1, None, "idle");
        assert_eq!(
            argv,
            vec![
                "./min-herdr-test-no-such-bin", "pane", "report-agent", "pane-7",
                "--source", "custom:min",
                "--agent", "min",
                "--seq", "1",
                "--state", "idle",
            ]
        );
    }

    #[test]
    fn release_argv_matches_herdr_cli_contract() {
        let argv = release_args(&cfg());
        assert_eq!(
            argv,
            vec![
                "./min-herdr-test-no-such-bin", "pane", "release-agent", "pane-7",
                "--source", "custom:min",
                "--agent", "min",
            ]
        );
    }

    #[test]
    fn event_mapping_covers_the_lifecycle() {
        // The three states Herdr understands, each wired to one min event.
        assert_eq!(state_for(&events::UserInput { text: String::new() }), Some("working"));
        assert_eq!(state_for(&events::MessageEnd {}), Some("idle"));
        assert_eq!(state_for(&events::Error { text: String::new() }), Some("blocked"));
        assert_eq!(state_for(&events::ToolEnd { ok: true, secs: 0.0, note: String::new(), preview: String::new() }), None);
    }

    /// The mapping from `handle`, expressed purely: `Quit` is the release
    /// case and is handled separately.
    fn state_for(event: &dyn Any) -> Option<&'static str> {
        if event.downcast_ref::<events::UserInput>().is_some() {
            Some("working")
        } else if event.downcast_ref::<events::MessageEnd>().is_some() {
            Some("idle")
        } else if event.downcast_ref::<events::Error>().is_some() {
            Some("blocked")
        } else {
            None
        }
    }

    #[test]
    fn disabled_without_herdr_env() {
        // A plugin that was never start()ed inside Herdr must report
        // nothing, whatever this test process's environment holds.
        let mut p = HerdrPlugin::new();
        assert!(p.cfg.is_none());
        let hub = Hub::new(vec![]);
        p.handle(&hub, &events::UserInput { text: "hi".into() });
        p.handle(&hub, &events::MessageEnd {});
        p.handle(&hub, &events::Error { text: "e".into() });
        p.handle(&hub, &events::SessionStart { id: "1-s".into(), parent: None });
        p.handle(&hub, &events::Quit {});
        assert_eq!(p.last_seq, 0, "no reports without the env gate");
        assert!(p.session.is_none());
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let mut p = HerdrPlugin::new();
        p.cfg = Some(cfg());
        p.report("working");
        p.report("idle");
        p.report("blocked");
        assert!(p.last_seq > 0, "seq starts at the wall clock, not 1");
        // report() stores each seq before spawning the (failing) CLI.
        let first = p.last_seq;
        p.report("idle");
        assert!(p.last_seq > first, "each report is strictly newer");
    }

    #[test]
    fn next_seq_survives_a_backwards_clock() {
        // Normal case: follow the clock.
        assert_eq!(next_seq(100, 200), 200);
        // Clock behind the last report (NTP step, VM suspend): still move.
        assert_eq!(next_seq(200, 100), 201);
        // Never tie, however many times the clock lands in the same nanosecond.
        assert_eq!(next_seq(201, 100), 202);
    }

    #[test]
    fn session_start_reassociates_and_rereports_the_state() {
        let mut p = HerdrPlugin::new();
        p.cfg = Some(cfg());
        p.report("working");
        assert!(p.session.is_none());

        p.handle(
            &Hub::new(vec![]),
            &events::SessionStart { id: "1729180001-k3f9".into(), parent: None },
        );
        // The last id is kept (the session plugin's prescribed pattern) …
        assert_eq!(p.session.as_deref(), Some("1729180001-k3f9"));
        // … and the current state was re-reported, so the id is associated
        // even though `/clear` itself changes no state.
        assert_eq!(p.state, "working");
        assert!(p.last_seq > 0);
    }

    #[test]
    fn session_end_forgets_the_id() {
        let mut p = HerdrPlugin::new();
        p.cfg = Some(cfg());
        p.session = Some("1729180001-k3f9".into());
        p.handle(&Hub::new(vec![]), &events::SessionEnd { id: "1729180001-k3f9".into() });
        assert!(p.session.is_none());
    }

    #[test]
    fn bin_falls_back_to_herdr_on_path() {
        assert_eq!(herdr_bin(Some("/opt/herdr".into())), "/opt/herdr");
        assert_eq!(herdr_bin(Some(String::new())), "herdr");
        assert_eq!(herdr_bin(None), "herdr");
    }

    #[test]
    fn source_and_agent_labels_are_stable() {
        assert_eq!(SOURCE, "custom:min");
        assert_eq!(AGENT, "min");
    }
}
