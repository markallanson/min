# min-herdr — report agent state to Herdr

Bridges min's event bus to [Herdr](https://github.com/herdrdev/herdr)'s
agent-tracking API: a Herdr pane exports four environment variables
(`HERDR_ENV`, `HERDR_PANE_ID`, `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`), and
when they are all present this plugin reports min's lifecycle state through
Herdr's CLI:

```sh
"$HERDR_BIN_PATH" pane report-agent "$HERDR_PANE_ID" \
  --source custom:min --agent min \
  --seq 1792315281664507903 --agent-session-id 1729180001-k3f9 \
  --state working
```

Outside Herdr it is a complete no-op — the env gate fails and nothing is
ever spawned. The same min binary runs inside and outside a Herdr pane.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-herdr` / `"herdr"` |
| shape | pure listener (plus one registration report in `start`) |
| tools offered | none |
| services published / consumed | none |
| deps | `min-core` + `min-session` (tests only; the plugin itself spawns the herdr CLI) |

## State

```
HerdrPlugin {
    cfg:      Option<Config>,  // Some once the env gate passes at start()
    last_seq: u64,             // last --seq handed out (see Reliability)
    session:  Option<String>,  // last id seen in SessionStart
    state:    &'static str,    // last state reported ("idle" at boot)
}
```

`session` follows the session plugin's rule — *the events are the interface;
keep the last id you saw* — and goes out on every report as
`--agent-session-id`, so Herdr can tell min's sessions apart under the one
stable `custom:min` source (e.g. across `/clear`).

## Events

Herdr understands three states; each is wired to one min event. Plugin-level:

**Listens:** `SessionStart`, `SessionEnd`, `UserInput`, `MessageEnd`,
`Error`, `Quit` — everything else is ignored (`handle` runs for each event,
but the `cfg` gate makes it a no-op outside Herdr).

**Emits:** nothing. Herdr reports never touch the min bus; they go straight
to the herdr CLI.

The mapping, event → herdr state:

| min moment | herdr state |
|---|---|
| `start` | `idle` — registers the agent so the pane shows min from launch (Herdr only tracks an agent after its first report). No session id exists yet, so this report carries no `--agent-session-id`. |
| `SessionStart` | re-report of the *current* state, now tagged `--agent-session-id` — `/clear` changes no state, but the replacement id must be associated. The startup announcement comes from the session plugin's first `wait`, so it always reaches this plugin. |
| `UserInput` | `working` |
| `MessageEnd` | `idle` |
| `Error` | `blocked` |
| `SessionEnd` | forget the id (no report) |
| `Quit` | release — hand lifecycle authority back to Herdr |

```
 env gate (start)              event bus
 ────────────────             ─────────────────────────────────────────────────
 HERDR_ENV == 1  ─┐           SessionStart ─▶ remember id, re-report state
 HERDR_BIN_PATH ✓ ├─ cfg ──▶ UserInput    ──▶ report --state working
 HERDR_PANE_ID  ✓ │           MessageEnd  ──▶ report --state idle
 else: cfg=None,  │           Error      ──▶ report --state blocked
 everything no-ops┘           SessionEnd  ──▶ forget the id
                              Quit        ──▶ pane release-agent
                              (anything else: ignored)
```

## Reliability rules

- **Never disturb min.** A failed spawn is silently ignored
  (`let _ = Command::…output()`); reporting must not break the session it
  observes.
- **`--seq` is unix time in nanoseconds**, never below the clock and strictly
  greater than the previous report (`next_seq`): monotonically increasing
  across runs of min, and immune to a backwards-stepping clock (NTP step,
  VM suspend). A per-process counter restarting at 1 could be *stale* against
  the previous run's reports after a crash — no `Quit`, no `release-agent` —
  and Herdr would drop them. This mirrors Herdr's own integrations
  (`report_seq = time.time_ns()`).
- **`--agent-session-id` carries min's session id** from `SessionStart`. The
  registration report at `start` cannot have one: the session plugin
  announces from its first `wait`, after every plugin has started, so
  `SessionStart` re-reports the current state as soon as the id arrives.
- **`SOURCE = "custom:min"` and `AGENT = "min"` are stable** — Herdr tracks
  lifecycle authority per source; they must never change between runs.
- Reports go out synchronously from `handle`; one report is a fast local
  spawn against Herdr's socket, and Herdr is expected to keep up.

## Worked example

A turn with one failing tool, inside a Herdr pane:

```
pane shows   min
  boot       start() → report idle                (pane now shows "idle")
             session.wait() → SessionStart {id} → re-report idle, with id
  user types read Cargo.toml ⏎
             UserInput    → report working        (pane shows "working")
             ToolStart / ToolEnd                (no reports — not mapped)
             MessageEnd   → report idle
  the tool fails on a later try
             Error        → report blocked
  /clear     SessionEnd → SessionStart {new id} → re-report blocked, new id
  /quit      Quit         → release-agent     (pane stops tracking min)
```

The `argv` for every report is built by a pure function (`report_args`) so
tests can assert the exact CLI contract.

## What the tests pin

- `report_args` produces the documented `herdr pane report-agent` invocation,
  with and without a session id; `release_args` produces the unchanged
  `release-agent` one.
- The event→state mapping covers exactly the three states; `ToolEnd` maps
  to nothing.
- A plugin that was never `start`ed inside Herdr reports nothing (`last_seq`
  stays 0) whatever the test process's environment holds.
- `seq` is the wall clock, strictly increasing, and survives a backwards
  clock (`next_seq`); `SOURCE`/`AGENT` are stable.
- `SessionStart` re-reports the current state under the new id;
  `SessionEnd` forgets the id.
- Integration (fake CLI): the six invocations Herdr receives are register →
  session-tagged register → working → idle → blocked → release, with seqs
  asserted fresh (unix nanos) and increasing rather than pinned; the harness
  includes the real `min_session` in its production load position so the
  announcement arrives exactly as in production.

## See also

[min-session](session.md) (where `SessionStart`/`SessionEnd` come from) ·
[min-repl](repl.md) (where `Quit` comes from) ·
[min-agent](agent.md) (where `UserInput`/`MessageEnd`/`Error` come from) ·
[plugins.md](../plugins.md)
