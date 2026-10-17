# Architecture

min is a terminal agentic coding harness in Rust with one architectural law:

> **The core is only a loop.** It loads plugins and pumps events between
> them — forever. It contains zero functionality and zero policy.
> Every capability — UI, LLM client, tools, the agent loop itself — is a
> plugin.

Everything below is this law applied: what the loop is, what plugins may
say to each other, and what happens on a real turn. **For how to write a
plugin — the hooks, the three shapes, a complete worked example — see
[plugins.md](plugins.md).** Line counts are approximate and drift; the
*shape* — who owns what, who talks to whom — is what matters.

---

## The system in one picture

```
 ┌───────────── min binary — src/main.rs, the only glue ──────────┐
 │   build the plugin list, call min_core::run(plugins)           │
 └────────────────────────────────────────────────────────────────┘
    │
    │  plugins, in load order (never import each other — only min-core)
    │
    │   min-repl          terminal UI: raw-mode editor, streaming output
    │   min-session       session identity: unique id + parent, announced to all
    │   min-session-log   session activity → one append-only log per session
    │                     (XDG state dir)
    │   min-llm           streaming LLM client (anthropic + openai wire)
    │   min-tools-fs      read / edit / write
    │   min-tools-shell   bash with timeout + output capture
    │   min-tools-clock   current UTC time
    │   min-context       ~/.config/min/AGENTS.md + project files → system prompt
    │   min-herdr         agent state → Herdr (no-op outside a Herdr pane)
    │   min-agent         the agent loop: LLM ↔ tools until done
    ▼
 ┌──────────────── min-core — the loop (zero deps) ───────────────┐
 │   Plugin trait · event bus · services · tool registry          │
 └────────────────────────────────────────────────────────────────┘
```

The binary is the only file that names plugins. Comment out a line there and
the rest still runs — that is the whole test of the architecture.

---

## The core: four nouns and a loop

`min-core` has an empty `[dependencies]` section, no I/O and
no async. It cannot even print. It provides exactly four nouns:

| noun | what it is | who defines behaviour |
|---|---|---|
| `Plugin` | the contract every feature implements | the plugin |
| `Hub::emit` | the event bus — deliver a value to every plugin | the emitter |
| `Hub::service` | one typed value, published for whoever needs it | the publisher |
| `Hub::tools` | name → callable registry the agent offers to the model | the registering plugin |

…and one verb, the loop. The real code (`run` in `min-core/src/lib.rs`),
lightly abridged:

```rust
pub fn run(plugins: Vec<Box<dyn Plugin>>) {
    let hub = Hub::new(plugins);
    for i in 0..n {                     // 1. start: register tools/services
        hub.with(i, |p, h| p.start(h));
    }
    while hub.is_running() {            // 2. the loop
        for i in 0..n {                 // 3. idle: who has something to say?
            if let Some(e) = hub.with(i, |p, h| p.wait(h)) {
                hub.dispatch(&*e);      // 4. deliver it to everyone
                break;                  //    (then back to idle)
            }
        }
    }                                   // 5. repeat until Hub::quit
}
```

That is the entire runtime. There is no scheduler, no queue, no threads, no
locks, no channels — a `RefCell<Vec<…>>` of plugins and a `while`.

**What the core does not do:** parse input, call APIs, touch files, hold
conversation state, decide when to quit on its own. If a change adds any of
that to `min-core`, the architecture has been violated — it belongs in a
plugin. (Adding a *data type* to `min-core/src/events.rs` is allowed: the
core routes events blindly by type, so a new event adds no behaviour.)

### The two rhythms

The loop has two alternating phases, and each plugin hook belongs to one:

```
        ┌─────────────────────────────────────────────┐
        │              IDLE  (nobody speaking)        │
        │  core calls wait() on plugin 1, 2, 3 …      │
        │  each may block:                            │
        │    · repl blocks on the keyboard            │
        │    · others return None immediately         │
        └──────────────────┬──────────────────────────┘
              wait() returns Some(event)
                           ▼
        ┌─────────────────────────────────────────────┐
        │            BUSY  (event propagating)        │
        │  core calls handle(e) on 1, 2, 3 … in order │
        │  handlers may emit more events (delivered   │
        │  immediately, depth-first)                  │
        └──────────────────┬──────────────────────────┘
                           ▼
                     back to IDLE
```

A plugin that wants to *originate* work returns it from `wait`; a plugin
that wants to *react* implements `handle`. The repl owns the keyboard, so
in practice min is driven by one long-lived `wait` (stdin) and everything
else is reaction.

---

## The Plugin contract

```rust
pub trait Plugin {
    fn name(&self) -> &'static str;
    fn start(&mut self, _hub: &Hub) {}                              // once
    fn wait(&mut self, _hub: &Hub) -> Option<Box<dyn Any>> { None } // idle
    fn handle(&mut self, _hub: &Hub, _event: &dyn Any) {}           // events
}
```

That is the entire surface a plugin implements — three hooks and a name.
What each hook is for, when to use which, and a complete working example
are in [plugins.md](plugins.md). Here we care about what the *core*
guarantees about them:

- `start` runs once per plugin, in load order, before the loop begins.
- `wait` is only ever called when the bus is quiet, so blocking in it is
  safe and expected; the first `Some(event)` ends the idle phase.
- `handle` sees every event exactly once, in load order — except events
  emitted while this plugin itself is executing (see below).

`Box<dyn Any>` is deliberate: the core never inspects events, so events can
be any `'static` type defined anywhere. The shared vocabulary lives in
`min-core/src/events.rs` purely so plugins don't have to import each other
to speak the same language.

---

## Events

The catalogue — pure data, no behaviour:

| event | direction | payload |
|---|---|---|
| `UserInput` | repl → agent | `text` |
| `AssistantDelta` | agent → repl | streamed text chunk |
| `MessageEnd` | agent → repl | (none) |
| `ToolStart` / `ToolEnd` | agent → repl | tool name + brief / ok, secs, note |
| `Info` / `Error` | any → repl | human-facing status |
| `SessionStart` / `SessionEnd` | session → everyone | session id + parent / id — a fresh id on boot and `/clear` |
| `ResetConversation` | repl → everyone | (none) — context re-reads AGENTS.md, session re-rolls the id on it |
| `ClearPromptSections` | context → agent | (none) — agent drops its collected sections |
| `SystemPromptSection` | context → agent | title + markdown body to append to the system prompt |
| `Quit` | repl → everyone | (none) — repl calls `hub.quit()` on it |

### Semantics

`hub.emit(e)` is **immediate, synchronous, depth-first, in load order**:

```
agent emits AssistantDelta("Hello")
 ├─ 1. repl.handle   → prints "Hello"            (receives)
 ├─ 2. llm.handle    → not its event, ignores    (receives)
 ├─ …                                                   
 └─ 6. agent.handle  → busy: it is the emitter   (does NOT receive)
```

- A plugin executing `start`/`wait`/`handle` **misses events emitted during
  its own execution**. Nothing is queued or replayed — this is what keeps
  the core tiny.
- An event returned from `wait` is emitted *after* `wait` returns, so its
  own producer receives it too (that's how the repl sees its own `Quit`).
- Emitting the same event type from inside `handle` does not recurse
  forever: the emitter is busy for the nested dispatch, so the chain
  terminates. `min-core`'s tests prove this with an `Echo` plugin.

The one rule to internalise: **plugins that must not miss an event should
emit from `wait`, not from `handle`.**

---

## Services and tools

Two more channels, both just maps the core holds. The core's role is
storage and lookup only — behaviour belongs entirely to the plugins.

- **Services** — one typed value per type. A provider plugin publishes it
  in `start` (`set_service`); any consumer fetches it by type
  (`service::<T>()`), getting `None` if the provider isn't loaded — so
  consumers degrade gracefully instead of breaking.
- **Tools** — how plugins extend what the *model* can do. A plugin
  registers implementations in `start`; the agent lists their specs for the
  model and dispatches calls through the registry. Tools exchange JSON
  strings, so the registry stays dependency-free. `Err` from a tool becomes
  a tool error for the model.

Usage examples for both are in [plugins.md](plugins.md).

---

## Worked example: one turn, end to end

The user types `what time is it?` (the model answers with the clock tool).
Load order: `repl, llm, fs, shell, clock, agent`.

```
 key │ core                          repl        llm     clock      agent
─────┼──────────────────────────────────────────────────────────────────────────
  ⏎  │ repl.wait ──▶ UserInput ──▶  [busy: typed]
     │   delivered in order ─────▶ ignores     ignores ignores ──▶ handle:
     │                                                    ┌────────────── turn()
     │                                                    │  push user msg
     │                                                    │  service::<Llm>() ✓
     │                                                    │  tools().specs()
     │◀──── AssistantDelta("Let me check…") ──────────────│  llm.complete() ┅
     │ prints chunk                                       │
     │◀──── MessageEnd ───────────────────────────────────│  stream done
     │                                                    │  tool_use: clock {}
     │◀──── ToolStart {clock} ────────────────────────────│
     │ prints "▸ clock"                                   │
     │                                        run() ◀────│ tools().run()
     │                                                    │
     │◀──── ToolEnd {ok, 0.0s} ───────────────────────────│
     │ prints "✓ 0.0s 2026-10-17 …"                       │  push tool_result
     │                                                    │  …next round…
     │◀──── AssistantDelta("It's 09:41 UTC.") ────────────│  no tool_use → done
     │◀──── MessageEnd                                    │
     │ turn() returns                                     │
 more│ repl.wait ──▶ (nothing yet) … loop idles on stdin
```

Every arrow is an event from the table above; every box on the right is the
agent plugin executing *inside* its own `handle(UserInput)` — which is why
it never sees its own emissions, only the repl does. The safety valve around
all this: the agent caps a turn at 50 LLM↔tool rounds.

---

## Dependency rules

```
 main ──▶ session  repl    llm    tools-fs   tools-shell   tools-clock   agent
            │       │       │        │           │              │          │└┄┄▶ min-llm (type only, see below)
            └───────┴───────┴────────┴───────────┴──────────────┴──────────┘
                                    │
                                    ▼
                                 min-core
```

- Plugins never import each other. Coordination is events, services, or the
  tool registry — all core-owned vocabulary. This is what makes any plugin
  deletable.
- **The one exception, on record:** `min-agent` imports `min-llm` to name
  the `Llm` service type (`service::<Llm>()`). It is a key, not a link —
  the lookup is core-mediated and optional (no service → an `Error` event,
  agent still runs). Removing it means publishing `Box<dyn ChatLlm>` with
  the trait in `min-core`; kept concrete so far for simplicity.
- `min-core` imports nothing.

---

## Design rules

1. **The core is only a loop.** Zero functionality, zero policy: load
   plugins, dispatch events, repeat. Touch the core only when plugins
   cannot express a feature at all — and prefer adding an event *type*
   over adding behaviour.
2. **Every capability is a plugin.** Missing something? The default answer
   is a new crate plus one line in `src/main.rs`
   ([how](plugins.md)).
3. **Plugins never import each other.** Events, services, tools — nothing
   else. Drop any plugin from the build and the rest still runs.
4. **Synchronous, single-threaded, in-order.** Blocking lives in `wait`.
   No tokio, no channels, no locks, no queues.
5. **Complexity lives at the edge.** The biggest crate (`min-llm`, ~640
   lines) absorbs wire-format ugliness so the agent loop stays ~200.

## Where things live

```
crates/min-core          the loop: Plugin, Hub, events, Tool, ToolRegistry
crates/min-repl          terminal UI (raw mode, ANSI, input history)
crates/min-llm           Anthropic Messages + OpenAI chat clients (streaming)
crates/min-agent         conversation state + tool loop + system prompt
crates/min-context       AGENTS.md (~/.config/min first, then whole tree, root-first) → system prompt
crates/min-session       session identity: epoch-second id (+ optional parent), announced
                         to every plugin on boot (from first `wait`) and on each `/clear`
crates/min-session-log   session activity → one append-only `<id>.log` per session in
                         `$XDG_STATE_HOME/min/sessions` (default `~/.local/state/...`):
                         header, prompts, assistant messages, tool calls, errors
crates/min-tools-fs      read / edit / write
crates/min-tools-shell   bash (timeout, capture, truncation)
crates/min-tools-clock   the plugins.md worked example, made real
crates/min-herdr         reports idle/working/blocked to Herdr via its CLI
                         (active only when HERDR_ENV=1; releases on Quit)
src/main.rs              the only glue: the plugin list
```
