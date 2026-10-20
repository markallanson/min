# min-subagents — the `subagent` / `subagent_spawn` / `subagent_collect` tools

Spawns a fresh `min` process as a **subagent**: a whole harness — its own
session, its own conversation, its own tools — given one instruction, and
reports everything it printed back as the tool result.

Three tools, one mechanism:

| tool | waits? | use |
|---|---|---|
| `subagent` | yes, to completion | one subagent, one call — sugar for spawn + collect |
| `subagent_spawn` | no — returns a handle at once | start several; they run in wall-clock parallel |
| `subagent_collect` | yes, for the handles you name | join one or more spawned subagents |

Concurrency lives **inside the tools** (each subagent is a separate
process), so everything around them stays synchronous and in-order:
min-agent's `for call in calls` loop is untouched, and the core stays a
single-threaded loop. A batch of five spawns still occupies five sequential
tool calls — but each call takes milliseconds, and one `collect` waits them
down together.

Nothing here is new capability — the pieces already existed:

| piece | already provided by |
|---|---|
| parenting a session | `min --parent-session <id>` (min-session) |
| one instruction → one turn → clean exit | the repl's non-tty mode (`echo 'hello' \| min`) |
| capture + truncation of process output | the pattern of the `bash` tool (min-tools-shell) |
| the result reaching the model | the tool registry + the agent loop (min-core, min-agent) |

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-subagents` / `"subagents"` |
| shape | **capability** (three `Tool`s, registered in `start`) + listener (`SessionStart`, `Quit`) |
| tools offered | `subagent`, `subagent_spawn`, `subagent_collect` |
| services published / consumed | none — the session id arrives via events, per "the events *are* the interface" (min-session) |
| deps | `min-core`, `serde_json`, `libc` (unix only) |

## Input / output

**`subagent` / `subagent_spawn`** (same input schema):

| field | default | meaning |
|---|---|---|
| `instruction` | required | the complete task for the subagent — it is the child's *only* context beyond its own system prompt and `AGENTS.md` |
| `timeout_secs` | `600` | kill the whole subagent tree after this many seconds (clamped 1–3600) |

`subagent` returns the child's captured output (below). `subagent_spawn`
returns immediately:

```
handle: 3 (running) — pass it to subagent_collect for the result
```

Handles are small integers, allocated in spawn order.

**`subagent_collect`**:

| field | default | meaning |
|---|---|---|
| `handles` | required | array of handles (a single bare string also works) |
| `mode` | `"all"` | `all`: wait for every handle; `any`: first *success* wins, the rest of the batch is killed |

Output is a JSON array of `{"handle", "ok", "result"}`, where `result` is
exactly what a one-call `subagent` would have returned:

```
  exit code: <n>
  <everything the subagent wrote to stdout>
  --- stderr ---                           ← only when stderr is non-empty
  <stderr>
  (no output)                              ← only when stdout was empty
```

killed jobs (deadline, `any`-mode cancellation, quit sweep) carry a `timed
out after <n>s` / `cancelled: …` line ahead of the same shape. Unknown
handles are reported, not fatal: `(no such handle: 7 — already collected,
or never spawned)`. Each `result` is truncated to ~24k chars (head ¾, tail
¼) — same policy as the `bash` tool.

`Ok` vs `Err`: `subagent` is `Ok` only on exit 0; `collect` is `Ok` when
its mode's goal was met (all ok / a winner found) — a partial result is
still delivered either way, so the parent's model sees how far every
subagent got.

## The mechanics

```
 spawn (either tool)
    │
    ├─ parent id = the last SessionStart this plugin saw     (no session service)
    ├─ exe     = current_exe() — a subagent is a whole min
    ├─ spawn:  <exe> --parent-session <id>
    │            stdin  = piped   ← instruction + EOF
    │            stdout = piped   ← its turn's output
    │            stderr = piped   ← its panics and harness errors
    │            own process group (a timeout can kill the whole tree)
    │
    │   child (a full min): repl reads one line → agent turn → tools →
    │   streamed answer → EOF → Quit → SessionEnd → exit 0
    │
    ├─ drain stdout/stderr on threads (no pipe deadlocks)
    └─ spawn returns: `subagent` waits · `subagent_spawn` inserts the
       Job into the table and returns `handle: <n>` at once

 collect (or the one-call wait)
    │
    ├─ remove the named jobs from the table (a collected handle is spent;
    │  nothing sits in the table across a wait)
    ├─ mode all:   wait each in order (they already ran in parallel)
    ├─ mode any:   round-robin try_wait() every 100 ms — a slow first
    │              handle cannot starve a fast second; first *success*
    │              wins, the rest of the batch gets the group kill
    ├─ past a job's deadline: kill its whole process group
    ├─ join drains, stitch stderr, truncate
    └─ [{handle, ok, result}, …]   (+ "(no such handle: …)" notes)
```

The job table is `Rc<RefCell<HashMap<String, Job>>>`, shared between the
plugin and the tools — the same pattern as the session id: the tools live
in the core's registry while the plugin keeps receiving events.

**Why a fork and not a nested loop:** the child is a whole min — fresh
context window, its own conversation, its own session id, its own session
JSONL file (min-session files it under the parent's project directory, with
`parent` set from `--parent-session`). It even loads the project's `AGENTS.md` like any
other run. The parent's context stays clean: it sees one tool result per
subagent, not their whole transcripts. And because children are processes,
parallelism costs the core nothing — the parent process never grows a
second thread of control beyond the per-pipe drain threads the bash-tool
pattern already uses.

**Depth:** the child gets these tools too — so the specs instruct the
model to work directly unless delegation is genuinely needed. Nothing
enforces a depth limit; nested subagents are legal but each level pays a
full context and a full turn.

**Termination is the child's own job.** The plugin never sends a running
child an event; the child quits because its stdin hits EOF after the one
instruction line (the repl's non-tty path). Three things can kill a job
earlier, and all three use the same primitive — kill the child's whole
process group (it leads its own group, so orphaned grandchildren holding
the output pipes die too), then reap, join the drains, and record the
partial output:

1. the job's own deadline, in `collect` or the one-call wait;
2. `any`-mode cancellation, once a sibling has won;
3. the plugin's `Quit` handler — a quitting parent sweeps every job still
   in the table, so a `/quit` (or ctrl-d) never leaves subagents running.

**`ETXTBSY` retry:** exec-ing a file that was written moments earlier can
fail transiently (`Text file busy`) — the kernel refuses to exec anything
still held open for writing. `spawn_subagent` retries that one error,
bounded (50 × 20 ms). Harmless in production (the binary is long-closed);
it is what makes the tests, which spawn scripts they have just written,
deterministic.

## Events

**Listens**

| event | reaction |
|---|---|
| `SessionStart` | remember `id` — it is the `--parent-session` value for every subagent spawned from now on (a spawn after `/clear` parents on the *new* session) |
| `TurnCancelled` | kill every job still in the table so cancelled turns do not leave running children |
| `Quit` | kill every job still in the table (group kill + reap + drain join), so no subagent outlives min |

**Emits**

| event | when |
|---|---|
| nothing | — the tools' output reaches the model through the tool registry, and the repl sees the calls only via the agent's usual `ToolStart` / `ToolEnd` |

## Worked example

The parent's model, told "have subagents summarise the fs and clock
plugins", can run them concurrently:

```
  ▸ subagent_spawn {"instruction":"Read crates/min-tools-fs/src/lib.rs and
                    summarise the three tools in two sentences each."}
  ✓ 0.0s  handle: 1 (running) — pass it to subagent_collect for the result
  ▸ subagent_spawn {"instruction":"Read crates/min-tools-clock/src/lib.rs and
                    summarise what the clock tool returns."}
  ✓ 0.0s  handle: 2 (running) — pass it to subagent_collect for the result
  ▸ subagent_collect {"handles":["1","2"]}
  ✓ 34.2s
  [{"handle":"1","ok":true,"result":"exit code: 0\n`read` returns a numbered
    listing …"},{"handle":"2","ok":true,"result":"exit code: 0\nthe clock
    tool returns the current UTC time …"}]
```

Both children ran at once: 34 s wall clock, not 56 s of summed turns. The
spawn calls themselves cost milliseconds — the parent could have read
files, or spawned more children, between them.

Inside each spawn, the tool runs — with pipes, not a terminal:

```
  $ min --parent-session 1764-k3fx        ← this process's session id
```

the instruction line goes in on stdin:

```
  Read crates/min-tools-fs/src/lib.rs and summarise the three tools in two
  sentences each.⏎ ⟨EOF⟩
```

and the child, being min, runs a normal turn — `read` calls, streamed
answer — then quits at EOF. Two subagents, two sessions, two logs — one
`{handle, ok, result}` entry each on the parent's side:

```
  $ ls ~/.local/state/min/sessions/<project>/
  1764-k3fx.jsonl        ← parent
  1764-lm2q.jsonl        ← child 1 (parent field: 1764-k3fx)
  1764-mm8d.jsonl        ← child 2 (parent field: 1764-k3fx)
```

## What the tests pin

All tool-behaviour tests use the real spawn path against a stand-in
executable (a shell script); the tools take their exe from the plugin's
injectable field, so the full JSON-input → spawn → stdin → drain → status
→ format protocol is exercised end to end (under cargo test,
`current_exe()` is the test binary, which would reject
`--parent-session`):

- the instruction reaches the child's stdin, and the child's arguments
  carry `--parent-session <id>` verbatim (one-call `subagent`);
- a clean exit 0 → `Ok`, containing the exit code line and the child's
  stdout; a non-zero exit → `Err` that still carries stdout *and* a
  `--- stderr ---` section — partial work is visible to the parent's model;
- a stuck child is killed at its deadline, promptly, and the reply says
  `timed out after <n>s` (this pins the process-group kill: the fixture
  sleeps via a grandchild-shaped `sleep`, which must not outlive the
  deadline);
- stdout-only, stderr-only, and huge (100k-line) outputs are all handled —
  the last truncated to <26k chars with head *and* tail preserved;
- `subagent_spawn` returns a handle immediately; `subagent_collect` on
  that handle returns the same shape as the one-call tool, and the handle
  is spent (a second collect reports `no such handle`);
- **wall-clock parallelism**: two spawned 1-s subagents, joined by one
  collect, take ~1 s — not ~2 s (this is the regression test for the
  whole feature);
- collect mode `any` round-robins, returns at the first *success*, kills
  the losers, and reports them as `cancelled`;
- input validation: missing `instruction`, non-JSON input, missing or
  empty `handles`, and a missing session id each produce specific errors;
- `SessionStart` events update the parent id the tools will use, later
  announcements replace earlier ones, and unrelated events change nothing;
- `cancels_running_subagent`: in-flight subagents are aborted and killed promptly when `is_cancelled()` is set;
- `turn_cancelled_kills_every_live_job`: `TurnCancelled` sweeps the table and kills any live jobs;
- `Quit` sweeps the table: every live job is group-killed, reaped, and its
  drain threads joined — nothing outlives the parent.

## See also

- [session.md](session.md) — where the parent id comes from (`SessionStart`),
  what `--parent-session` does to the child's first session, and how child
  `.jsonl` files land under the project directory.
- [tools-shell.md](tools-shell.md) — the spawn/drain/truncate pattern these
  tools follow (threads per pipe, poll + kill, head/tail truncation).
- [repl.md](repl.md) — the non-tty input path that makes "write a line,
  close stdin" a complete one-turn run.
- [agent.md](agent.md) — how tool results (including an `Err` one) flow
  back into the parent's conversation.
- [plugins.md](plugins.md).
