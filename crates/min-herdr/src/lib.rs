//! min-herdr — report min's agent state to [Herdr](https://github.com/herdrdev/herdr).
//!
//! A Herdr pane exports `HERDR_ENV`, `HERDR_PANE_ID`, `HERDR_BIN_PATH` and
//! `HERDR_SOCKET_PATH`. When all are present, this plugin reports min's
//! lifecycle state through Herdr's CLI ("integrate your own agent"):
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
//! | min moment   | herdr state |
//! |--------------|-------------|
//! | `start`      | `idle` — registers the agent |
//! | `UserInput`  | `working`   |
//! | `MessageEnd` | `idle`      |
//! | `Error`      | `blocked`   |
//! | `Quit`       | release     |
//!
//! Reports go out synchronously from `start`/`handle`; a report is one fast
//! local spawn against Herdr's socket, and Herdr is expected to keep up. `--seq`
//! is strictly increasing per process so Herdr can drop stale, out-of-order
//! reports from the same source.

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
    /// Strictly increasing sequence number, per source, for stale-report
    /// rejection. The first report carries `--seq 1`.
    seq: u64,
}

struct Config {
    /// `$HERDR_BIN_PATH` — the herdr CLI used for all reports.
    bin: String,
    /// `$HERDR_PANE_ID` — the pane whose state we report.
    pane: String,
}

impl HerdrPlugin {
    pub fn new() -> Self {
        HerdrPlugin { cfg: None, seq: 0 }
    }

    /// True when running inside a Herdr pane. The docs require gating on
    /// `HERDR_ENV=1` *and* the presence of the variables actually used, so
    /// the integration is a no-op everywhere else.
    fn enabled() -> bool {
        std::env::var("HERDR_ENV").as_deref() == Ok("1")
            && env_nonempty("HERDR_BIN_PATH")
            && env_nonempty("HERDR_PANE_ID")
    }

    /// Spawn the herdr CLI with the given `report-agent` arguments. A
    /// failed spawn is silently ignored — reporting must never disturb
    /// min itself (or this would not be a no-op outside Herdr).
    fn report(&mut self, extra: &[&str]) {
        let Some(cfg) = &self.cfg else { return };
        self.seq += 1;
        let argv = report_args(cfg, self.seq, extra);
        let _ = Command::new(&argv[0]).args(&argv[1..]).output();
    }

    /// Hand the pane's lifecycle authority for this source back to Herdr.
    fn release(&mut self) {
        let Some(cfg) = &self.cfg else { return };
        let argv = release_args(cfg);
        let _ = Command::new(&argv[0]).args(&argv[1..]).output();
    }
}

fn env_nonempty(key: &str) -> bool {
    std::env::var(key).map(|v| !v.is_empty()) == Ok(true)
}

/// One `pane report-agent` invocation, as full argv. Pure so tests can
/// assert the exact CLI contract.
fn report_args(cfg: &Config, seq: u64, extra: &[&str]) -> Vec<String> {
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
    argv.extend(extra.iter().map(|s| s.to_string()));
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
                bin: std::env::var("HERDR_BIN_PATH").unwrap_or_default(),
                pane: std::env::var("HERDR_PANE_ID").unwrap_or_default(),
            });
            // Register at once, not at the first message: Herdr only tracks
            // an agent after its first report, so without this the pane would
            // show nothing until the user typed. min is idle at startup.
            self.report(&["--state", "idle"]);
        }
    }

    fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
        // The cfg gate makes this a no-op outside Herdr, even though
        // handle() still runs for every event.
        if self.cfg.is_none() {
            return;
        }
        if event.downcast_ref::<events::UserInput>().is_some() {
            self.report(&["--state", "working"]);
        } else if event.downcast_ref::<events::MessageEnd>().is_some() {
            self.report(&["--state", "idle"]);
        } else if event.downcast_ref::<events::Error>().is_some() {
            self.report(&["--state", "blocked"]);
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
        Config { bin: "herdr".into(), pane: "pane-7".into() }
    }

    #[test]
    fn report_argv_matches_herdr_cli_contract() {
        let argv = report_args(&cfg(), 3, &["--state", "working"]);
        assert_eq!(
            argv,
            vec![
                "herdr", "pane", "report-agent", "pane-7",
                "--source", "custom:min",
                "--agent", "min",
                "--seq", "3",
                "--state", "working",
            ]
        );
    }

    #[test]
    fn release_argv_matches_herdr_cli_contract() {
        let argv = release_args(&cfg());
        assert_eq!(
            argv,
            vec![
                "herdr", "pane", "release-agent", "pane-7",
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
        p.handle(&hub, &events::Quit {});
        assert_eq!(p.seq, 0, "no reports without the env gate");
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let mut p = HerdrPlugin::new();
        p.cfg = Some(cfg());
        p.report(&["--state", "working"]);
        p.report(&["--state", "idle"]);
        p.report(&["--state", "blocked"]);
        assert_eq!(p.seq, 3);
    }

    #[test]
    fn source_and_agent_labels_are_stable() {
        assert_eq!(SOURCE, "custom:min");
        assert_eq!(AGENT, "min");
    }
}
