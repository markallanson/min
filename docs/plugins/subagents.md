# min-subagents — the `subagent` tool

Spawns a fresh `min` process as a **subagent**: a whole harness — its own
session, its own conversation, its own tools — given one instruction, and
reports everything it printed back as the tool result. One tool call runs
one subagent to completion; the model runs several by making several calls
in one turn, and the agent loop gathers the results as it does for any
tool.

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
| shape | **capability** (one `Tool`, registered in `start`) + listener (`SessionStart`) |
| tools offered | `subagent` |
| services published / consumed | none — the session id arrives via events, per "the events *are* the interface" (min-session) |
| deps | `min-core`, `serde_json`, `libc` (unix only) |

## Input / output

Input (the model's JSON arguments):

| field | default | meaning |
|---|---|---|
| `instruction` | required | the complete task for the subagent — it is the child's *only* context beyond its own system prompt and `AGENTS.md` |
| `timeout_secs` | `600` | kill the whole subagent tree after this many seconds (clamped 1–3600) |

Output (a `Result<String, String>` — `Ok` only on a clean exit 0; a
failing or killed subagent is an `Err` that still carries everything it
printed, so the parent's model sees how far it got):

```
 timed out after <n>s (subagent killed)   ← only when the deadline hit
 exit code: <n>
 <everything the subagent wrote to stdout>
 --- stderr ---                           ← only when stderr is non-empty
 <stderr>
 (no output)                              ← only when stdout was empty
```

Output is truncated to ~24k chars (head ¾, tail ¼) before it reaches the
parent's context — same policy as the `bash` tool.

## The mechanics

```
 model calls subagent {instruction}
   │
   ├─ parent id = the last SessionStart this plugin saw   (no session service)
   ├─ spawn:  <current_exe> --parent-session <id>
   │            stdin  = piped   ← instruction + EOF
   │            stdout = piped   ← its turn's output
   │            stderr = piped   ← its panics and harness errors
   │            own process group (timeout can kill the whole tree)
   │
   │   child (a full min): repl reads one line → agent turn → tools →
   │   streamed answer → EOF → Quit → SessionEnd → exit 0
   │
   ├─ drain stdout/stderr on threads (no pipe deadlocks)
   ├─ poll try_wait() every 100 ms; past the deadline: kill the group
   ├─ stitch, truncate
   └─ exit 0 → Ok(output) · otherwise → Err(output)   (tool error for the model)
```

**Why a fork and not a nested loop:** the child is a whole min — fresh
context window, its own conversation, its own session id, its own session
log (min-session-log files it under the parent's directory, because of
`--parent-session`). It even loads the project's `AGENTS.md` like any
other run. The parent's context stays clean: it sees one tool result, not
the subagent's whole transcript.

**Depth:** the child gets this tool too — so the spec instructs the model
to work directly unless delegation is genuinely needed. Nothing enforces a
depth limit; nested subagents are legal but each level pays a full context
and a full turn.

**Termination is the child's own job.** This plugin never sends the child
an event; the child quits because its stdin hits EOF after the one
instruction line (the repl's non-tty path), the session plugin closes the
session on `Quit`, and the process exits 0. A stuck child is killed with
its whole process group at the deadline — without the group kill, an
orphaned grandchild holding the output pipes would keep min blocked long
past the deadline.

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
| `SessionStart` | remember `id` — it is the `--parent-session` value for every subagent spawned from now on (a spawn after `/clear` parents on the *new* session). Everything else is ignored. |

**Emits**

| event | when |
|---|---|
| nothing | — the tool's output reaches the model through the tool registry, and the repl sees the call only via the agent's usual `ToolStart` / `ToolEnd` |

The session id is read by the tool **at call time** through a shared
`Rc<RefCell<Option<String>>>` (the tool lives in the core's registry while
the plugin keeps receiving events). A call before the first announcement —
which in practice cannot happen, since min-session announces from its
first `wait`, before any input exists — fails with a plain error naming
the likely cause.

## Worked example

The parent's model, told "have subagents summarise the fs and clock
plugins", makes two calls in one turn (the agent loop runs them in order
and gathers both results, like any tool batch):

```
  ▸ subagent {"instruction":"Read crates/min-tools-fs/src/lib.rs and
              summarise the three tools in two sentences each."}
  ✓ 34.2s
  ▸ subagent {"instruction":"Read crates/min-tools-clock/src/lib.rs and
              summarise what the clock tool returns."}
  ✓ 21.8s
```

Inside the first call, the tool spawns — with pipes, not a terminal:

```
 $ min --parent-session 1764-k3fx        ← this process's session id
```

the instruction line goes in on stdin:

```
 Read crates/min-tools-fs/src/lib.rs and summarise the three tools in two
 sentences each.⏎ ⟨EOF⟩
```

and the child, being min, runs a normal turn — `read` calls, streamed
answer — then quits at EOF. What it printed comes back as the tool result
the parent's model sees:

```
 exit code: 0
 `read` returns a numbered listing … `edit` replaces an exact unique match …
 `write` creates or overwrites atomically …
```

Two subagents, two sessions, two logs — one tool result each on the
parent's side:

```
 $ ls ~/.local/state/min/sessions/<project>/
 1764-k3fx.log          ← parent
 1764-lm2q.log          ← child 1 (parent field: 1764-k3fx)
 1764-mm8d.log          ← child 2 (parent field: 1764-k3fx)
```

## What the tests pin

All tool-behaviour tests use the real `collect` path against a stand-in
executable (a shell script), so spawn → stdin → drain → status → format
is exercised end to end:

- the instruction reaches the child's stdin, and the child's arguments
  carry `--parent-session <id>` verbatim;
- a clean exit 0 → `Ok`, containing the exit code line and the child's
  stdout;
- a non-zero exit → `Err` that still carries stdout *and* a `--- stderr
  ---` section — partial work is visible to the parent's model;
- a stuck child is killed at the deadline, promptly, and the reply says
  `timed out after <n>s` (this pins the process-group kill: the fixture
  sleeps via a grandchild-shaped `sleep`, which must not outlive the
  deadline);
- stdout-only, stderr-only, and huge (100k-line) outputs are all handled —
  the last truncated to <26k chars with head *and* tail preserved;
- input validation: missing `instruction`, non-JSON input, and a missing
  session id each produce specific errors;
- `SessionStart` events update the parent id the tool will use, later
  announcements replace earlier ones, and unrelated events change nothing.

## See also

- [session.md](session.md) — where the parent id comes from (`SessionStart`),
  and what `--parent-session` does to the child's first session.
- [tools-shell.md](tools-shell.md) — the spawn/drain/truncate pattern this
  tool follows (threads per pipe, poll + kill, head/tail truncation).
- [repl.md](repl.md) — the non-tty input path that makes "write a line,
  close stdin" a complete one-turn run.
- [agent.md](agent.md) — how tool results (including an `Err` one) flow
  back into the parent's conversation.
- [session-log.md](session-log.md) — why subagent logs land under the
  parent's project directory.
- [plugins.md](plugins.md).
