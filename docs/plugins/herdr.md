# min-herdr — report agent state to Herdr

Bridges min's event bus to [Herdr](https://github.com/herdrdev/herdr)'s
agent-tracking API: a Herdr pane exports four environment variables
(`HERDR_ENV`, `HERDR_PANE_ID`, `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`), and
when they are all present this plugin reports min's lifecycle state through
Herdr's CLI:

```sh
"$HERDR_BIN_PATH" pane report-agent "$HERDR_PANE_ID" \
  --source custom:min --agent min --state working
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
| deps | `min-core` only (spawns the herdr CLI) |

## Events

Herdr understands three states; each is wired to one min event. Plugin-level:

**Listens:** `UserInput`, `MessageEnd`, `Error`, `Quit` — everything else is
ignored (`handle` runs for each event, but the `cfg` gate makes it a no-op
outside Herdr).

**Emits:** nothing. Herdr reports never touch the min bus; they go straight
to the herdr CLI.

The mapping, event → herdr state:

| min moment | herdr state |
|---|---|
| `start` | `idle` — registers the agent so the pane shows min from launch (Herdr only tracks an agent after its first report) |
| `UserInput` | `working` |
| `MessageEnd` | `idle` |
| `Error` | `blocked` |
| `Quit` | release — hand lifecycle authority back to Herdr |

```
 env gate (start)              event bus
 ────────────────             ────────────────────────────────────────────
 HERDR_ENV == 1  ─┐           UserInput ────▶ report --state working
 HERDR_BIN_PATH ✓ ├─ cfg ──▶ MessageEnd ──▶ report --state idle
 HERDR_PANE_ID  ✓ │           Error ─────▶ report --state blocked
 else: cfg=None,  │           Quit ───────▶ pane release-agent
 everything no-ops┘           (anything else: ignored)
```

## Reliability rules

- **Never disturb min.** A failed spawn is silently ignored
  (`let _ = Command::…output()`); reporting must not break the session it
  observes.
- **`--seq` is strictly increasing per process** (the registration report is
  `1`), so Herdr can drop stale, out-of-order reports from the same source.
- **`SOURCE = "custom:min"` and `AGENT = "min"` are stable** — Herdr tracks
  lifecycle authority per source; they must never change between runs.
- Reports go out synchronously from `handle`; one report is a fast local
  spawn against Herdr's socket, and Herdr is expected to keep up.

## Worked example

A turn with one failing tool, inside a Herdr pane:

```
pane shows   min
  boot       start() → report idle            (pane now shows "idle")
  user types read Cargo.toml ⏎
             UserInput    → report working    (pane shows "working")
             ToolStart / ToolEnd            (no reports — not mapped)
             MessageEnd   → report idle
  the tool fails on a later try
             Error        → report blocked
  /quit      Quit         → release-agent     (pane stops tracking min)
```

The `argv` for every report is built by pure functions
(`report_args` / `release_args`) so tests can assert the exact CLI contract.

## What the tests pin

- Both argv builders produce the documented `herdr pane …` invocations.
- The event→state mapping covers exactly the three states; `ToolEnd` maps
  to nothing.
- A plugin that was never `start`ed inside Herdr reports nothing (`seq`
  stays 0) whatever the test process's environment holds.
- `seq` increments per report; `SOURCE`/`AGENT` are stable.

## See also

[min-repl](repl.md) (where `Quit` comes from) ·
[min-agent](agent.md) (where `UserInput`/`MessageEnd`/`Error` come from) ·
[plugins.md](../plugins.md)
