# min-tools-clock — the `clock` tool

The current UTC time, as a tool the model can call. This is the plugins.md
worked example made real: a crate, a struct, two impls, no dependencies
beyond `min-core` — even the calendar math (days since epoch → Y-M-D,
Howard Hinnant's algorithm) is done by hand rather than pulling in a
datetime crate.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-tools-clock` / `"clock"` |
| shape | **capability** — one `Tool` impl, registered in `start` |
| tools offered | `clock` |
| services published / consumed | none |
| deps | `min-core` only |

## The tool

```rust
impl Tool for ClockTool {
    fn name(&self) -> &'static str { "clock" }
    fn spec(&self) -> &'static str {
        r#"{"description":"Current UTC time",
            "input_schema":{"type":"object","properties":{}}}"#
    }
    fn run(&self, _input: &str) -> Result<String, String> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?;      // clock before 1970: a tool error
        Ok(format_utc(secs))
    }
}
```

- **Input:** none — `"input_schema": {"type":"object","properties":{}}`;
  `run` ignores whatever the model sends.
- **Output:** `YYYY-MM-DD HH:MM:SS` in UTC, e.g. `2026-10-17 09:41:07`.
- **Error:** only if the clock is before the epoch (`Err` → a tool error
  for the model).

## Events

**Listens / emits:** nothing. The plugin's entire life is one `start` call
registering the tool; every subsequent interaction is the tool registry
calling `run` on behalf of the model. What the user sees is min-agent's
`ToolStart`/`ToolEnd` around the call, and the model's answer afterwards.

## Worked example

"what time is it?" — load order `…, repl, llm, …, clock, …, agent`:

```
key │ core                          repl        llm     clock      agent
────┼──────────────────────────────────────────────────────────────────────
 ⏎  │ repl.wait ──▶ UserInput ──▶  [busy: typed]
    │   delivered in order ─────▶ ignores    ignores ignores ──▶ handle:
    │                                                     ┌────── turn()
    │◀──── AssistantDelta("Let me check…") ───────────────│  llm.complete() ┅
    │◀──── MessageEnd ────────────────────────────────────│  tool_use: clock {}
    │◀──── ToolStart {clock} ─────────────────────────────│
    │                                        run() ◀──────│ tools().run()
    │◀──── ToolEnd {ok, 0.0s, "2026-10-17 09:41:07"} ─────│  push tool_result
    │◀──── AssistantDelta("It's 09:41 UTC.") ─────────────│  no tool_use → done
    │◀──── MessageEnd                                     │
more│ repl.wait ──▶ (nothing yet) … loop idles on stdin
```

The tool itself never appears in the dispatch — it is called directly
through the registry, inside the agent's `handle(UserInput)`.

## What the tests pin

`format_utc` against known moments: the epoch, a leap day
(2000-02-29), an arbitrary 2023 timestamp, and the y2038 boundary.

## See also

[min-tools-fs](tools-fs.md) and [min-tools-shell](tools-shell.md) (the
other tool plugins) · [min-agent](agent.md) (the dispatcher) ·
[plugins.md](../plugins.md) (the guide this crate implements)
