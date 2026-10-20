# min-tools-shell — the `bash` tool

Registers the `bash` tool: run a command in bash with a deadline, capture
its output, and truncate huge output before it reaches the model's context.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-tools-shell` / `"shell"` |
| shape | **capability** — one `Tool` impl, registered in `start` |
| tools offered | `bash` |
| services published / consumed | none |
| deps | `min-core`, `serde_json` |

## Input / output

Input (the model's JSON arguments):

| field | default | meaning |
|---|---|---|
| `command` | required | run via `bash -c`, inheriting min's cwd and environment |
| `timeout_secs` | `120` | kill the process after this many seconds (clamped 1–600) |

Output (a `Result<String, String>` — `Err` only for bad input or a failed
spawn; a *failing command* is a successful tool run that reports its exit
code):

```
 timed out after <n>s (process killed)     ← only when the deadline hit
 exit code: <n>
 <stdout>
 --- stderr ---                            ← only when stderr is non-empty
 <stderr>
 (no output)                               ← only when both were empty
```

## The mechanics

```
 spawn bash -c <command>
   stdout ─┐
           ├─ each drained on its own thread (no pipe deadlocks: even if
   stderr ─┘   the child fills one pipe and blocks, both keep draining)
 poll try_wait() every 20 ms
   ├─ cancelled (is_cancelled()) → kill process group, wait(), return Err("cancelled by user")
   ├─ exits before the deadline  → status
   └─ deadline passes            → kill(), wait(), mark timed_out
 join both drainers, stitch stdout + stderr, truncate, report
```

Draining on threads matters: reading stdout to completion *before* stderr
would deadlock any child that writes enough to fill a pipe.

## Truncation

Output is capped at ~24k characters, keeping the head (¾) and tail (¼) so
both the beginning of a build log and its final error survive:

```
 <first ¾>
 …[truncated <n> chars]…
 <last ¼>
```

Counts and cuts are in characters, not bytes (UTF-8 safe).

## Events

**Listens / emits:** nothing — same as all tool plugins. The bus sees only
what min-agent emits around the call: `ToolStart { name: "bash", brief:
<first line of the command> }` before, and `ToolEnd { ok, secs, note,
preview }` after.

## Worked example

`{"command": "cargo test -p min-core", "timeout_secs": 300}`:

```
 ▸ bash cargo test -p min-core            (repl, from ToolStart)
 …running…
 ✓ 4.2s                                   (repl, from ToolEnd)
 exit code: 0
 running 4 tests
 test tests::events_reach_other_plugins_not_busy_ones ... ok
 …
```

A hang: `{"command": "sleep 30", "timeout_secs": 1}` →

```
 ✗ 1.1s
 timed out after 1s (process killed)
 exit code: -1
```

(the process was killed; the `-1` is the conventional "no exit code").

## What the tests pin

- Exit code and stdout captured (`echo hello; exit 3` → `exit code: 3`).
- stderr stitched under a `--- stderr ---` marker.
- Timeout kills within its window (1s deadline finishes well under 10s).
- Cancellation kills the running command promptly (`cancels_running_command`).
- `seq 1 100000` comes back truncated, under ~26k chars, with the marker.

## See also

[min-tools-fs](tools-fs.md) (the other half of self-modification: read and
edit, then verify here) · [min-agent](agent.md) · [plugins.md](../plugins.md)
