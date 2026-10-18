# Writing plugins

Everything in min is a plugin: the UI, the LLM client, every tool, the agent
loop itself. This guide is for writing a new one. For how the core works —
the run loop, event delivery, the reasoning behind the rules — see
[architecture.md](architecture.md).

**Every plugin must ship with its own architecture document** —
`docs/plugins/<name>.md`, one per crate, explaining how it works with a
diagram, a worked example, and the events it listens to and emits. This is
not optional polish: the plugin docs are how the next plugin (human or
min) finds the service, event or precedent it needs without reading every
crate. [Plugin documentation](#plugin-documentation-required) below has
the template; the [index of existing docs](#the-plugins) is at the bottom.
Everything else on this page is the mechanics the docs describe.

The whole API surface is two traits and one line of glue:

```rust
pub trait Plugin {
    fn name(&self) -> &'static str;
    fn start(&mut self, _hub: &Hub) {}                              // once, at startup
    fn wait(&mut self, _hub: &Hub) -> Option<Box<dyn Any>> { None } // idle; may block
    fn handle(&mut self, _hub: &Hub, _event: &dyn Any) {}           // every event
}

pub trait Tool {
    fn name(&self) -> &'static str;
    fn spec(&self) -> &'static str;   // {"description":…,"input_schema":…}
    fn run(&self, input: &str) -> Result<String, String>;
}
```

A plugin is a crate under `crates/`, a struct implementing `Plugin`, and one
line in `src/main.rs`. That's it — no manifest, no registration file, no
lifecycle beyond the three hooks.

---

## The plugins

One architecture document per plugin, in `docs/plugins/`. **Read the one
closest to what you're building before writing a new plugin** — and write
the matching one for yours. Every doc follows the same template (see
[Plugin documentation](#plugin-documentation-required)): at-a-glance,
state, diagrams, events listened to and emitted, a worked example, and what
the tests pin.

| plugin | document | shape | one line |
|---|---|---|---|
| repl | [repl.md](plugins/repl.md) | source | terminal UI: raw-mode editor, streaming output, owns `Quit` |
| session | [session.md](plugins/session.md) | source + listener | session identity: unique `<epoch>-<rand>` id (+ optional parent), announced to all |
| session-log | [session-log.md](plugins/session-log.md) | listener | session activity → one append-only log per session (XDG state dir) |
| llm | [llm.md](plugins/llm.md) | provider | streaming LLM service (anthropic + openai wire), retries, z.ai mirror |
| fs | [tools-fs.md](plugins/tools-fs.md) | capability | `read` / `edit` / `write` — exact-match, atomic edits |
| shell | [tools-shell.md](plugins/tools-shell.md) | capability | `bash` with timeout, deadlock-free capture, output truncation |
| clock | [tools-clock.md](plugins/tools-clock.md) | capability | current UTC time — the worked example, made real |
| context | [context.md](plugins/context.md) | source + listener | `~/.config/min/AGENTS.md` + project files → system-prompt sections |
| herdr | [herdr.md](plugins/herdr.md) | listener | agent state → Herdr (no-op outside a Herdr pane; reports `--agent-session-id`) |
| agent | [agent.md](plugins/agent.md) | listener/worker | the agent loop: LLM ↔ tools until done |

Load order is exactly this table's order (bottom of `src/main.rs` is the
authority); the docs cross-link where it matters.

---

## Plugin documentation (required)

Every plugin crate ships with `docs/plugins/<name>.md` — the file that
explains how it works to the next person (or the next instance of min)
without making them read the crate. A new plugin is not done until the doc
exists; a changed plugin's doc is changed in the same commit. No new page
styles: the ten existing docs are the canon — copy the structure.

Required contents, in order (skip a section only when genuinely empty —
say so in one line rather than dropping the heading):

1. **One-paragraph summary** — what it does, in terms of the bus: what
   comes in, what goes out.
2. **At a glance** — a two-column table: crate and plugin `name()`, which
   of the [three shapes](#the-three-shapes-a-plugin-takes) it takes, tools
   offered, services published/consumed, dependencies.
3. **A diagram** — ASCII, in a fenced code block. The shape of the thing:
   its state machine, its data flow, or the lifecycle of what it owns.
   Every doc has at least one; the event-driven ones have an event-flow
   diagram too.
4. **Events** — two tables, explicit and exhaustive: **Listens** (event →
   reaction) and **Emits** (event → when). "Nothing" is a valid answer for
   either, but say it — the tables are how plugins find each other.
5. **A worked example** — one concrete run, start to finish, with the
   actual inputs and outputs (the clock doc's transcript is the model).
6. **What the tests pin** — the behaviours the crate's tests guarantee, so
   a reader knows what is contract and what is accident.
7. **See also** — links to the plugin docs it talks to (or wishes it could)
   and back to [plugins.md](plugins.md).

Keep it truthful to the code, not aspirational: line counts drift, event
tables rot — when you change a plugin, change its doc in the same commit.

## The three shapes a plugin takes

Almost every plugin is one of these. Pick the shape, copy the skeleton.

### 1. A listener — reacts to events (`handle`)

```rust
use min_core::{events, Hub, Plugin};
use std::any::Any;

pub struct Notepad;

impl Plugin for Notepad {
    fn name(&self) -> &'static str { "notepad" }
    fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
        if let Some(e) = event.downcast_ref::<events::ToolEnd>() {
            // downcast, react, ignore everything that isn't yours
        }
    }
}
```

This is the most common shape: loggers, UIs, metrics, anything that watches.

### 2. A source — originates work (`wait`)

```rust
fn wait(&mut self, _hub: &Hub) -> Option<Box<dyn Any>> {
    let line = read_line_blocking()?;   // block on the outside world
    Some(Box::new(events::UserInput { text: line }))
}
```

The core calls `wait` on each plugin, in load order, whenever nothing is
happening. **Blocking here is correct and expected** — the repl blocks on
stdin, and that is the heartbeat of the whole system. If your `wait` has
nothing to do, return `None` — but never return `None` *immediately, always*:
a set of plugins that neither block nor produce events will spin the CPU.

### 3. A capability — gives the model something to call (`Tool`)

```rust
impl Plugin for ClockPlugin {
    fn name(&self) -> &'static str { "clock" }
    fn start(&mut self, hub: &Hub) {
        hub.tools().register(Box::new(ClockTool));
    }
}
```

(A fourth shape — *provider*, publishing a service with `hub.set_service` —
is described under [Services](#services) below.)

---

## The hooks

| hook | when it runs | use it to |
|---|---|---|
| `start` | once, in load order | register tools, publish services |
| `wait` | whenever the system is idle, in load order | **block** on the outside world (stdin, a socket, a timer) and return an event when you have one |
| `handle` | for every event, in load order | react: print, mutate your state, emit more events |

`Box<dyn Any>` is deliberate: the core never inspects events, so an event
can be any `'static` type defined anywhere — including inside your own
crate, if no other plugin needs it.

**Load order matters.** `handle` and `wait` are called in the order plugins
appear in `src/main.rs`: the UI goes first (so the terminal exists before
anyone prints), tools register before the agent starts (so the model sees
them on the first turn).

---

## Events: the shared vocabulary

Events are pure data. The catalogue lives in `min-core/src/events.rs` so
plugins never import each other:

| event | direction | payload |
|---|---|---|
| `UserInput` | repl → agent | `text` |
| `AssistantDelta` | agent → repl | streamed text chunk |
| `MessageEnd` | agent → repl | (none) |
| `ToolStart` / `ToolEnd` | agent → repl | tool name + brief / ok, secs, note, first lines of the result as `preview` |
| `Info` / `Error` | any → repl | human-facing status |
| `SessionStart` / `SessionEnd` | session → everyone | session id + parent / id — a fresh id on boot and `/clear` |
| `ResetConversation` | repl → everyone | (none) — context re-reads instruction files, session re-rolls the id |
| `ClearPromptSections` | context → agent | (none) — agent drops its collected system-prompt sections |
| `SystemPromptSection` | context → agent | title + markdown body to append to the system prompt |
| `Quit` | repl → everyone | (none) — the repl calls `hub.quit()` on it |

Delivery rules in one breath: `hub.emit(e)` is immediate, synchronous and
in load order; a plugin that is itself executing misses the events emitted
during its own execution (nothing is queued or replayed). The full
semantics — with a delivery diagram and the reason recursion terminates —
are in [architecture.md](architecture.md#events). Two consequences for
authors:

- **Emit from `wait`, not from `handle`,** if you must not miss an event
  yourself: an event returned from `wait` is delivered *after* `wait`
  returns, so its own producer receives it too. The same trick makes a
  startup broadcast complete: min-session announces the first session from
  its first `wait`, because the core only calls `wait` after *every*
  plugin's `start` — anything emitted in `start` is missed by all plugins
  loaded later.
- Emit `events::Info` / `events::Error` for anything a human should see —
  don't print from your plugin; the repl owns the terminal.
- To add a brand-new event *type*, put it in `min-core/src/events.rs` —
  the core routes by type, blindly, so this adds no behaviour to the core.

---

## Services

A service is one typed value a plugin publishes for others — the lazy,
optional alternative to an import:

```rust
// the provider, in start():            // the consumer, anywhere:
hub.set_service(Llm::from_env());       let Some(llm) = hub.service::<Llm>() else {
                                            // degrade gracefully — it's optional
                                        };
```

One value per type; a second `set_service::<S>` replaces the first. The
lookup returns `None` when absent, so a consumer works even when the
provider plugin isn't loaded. The type still has to be *nameable* by the
consumer — see "The one exception" in
[architecture.md](architecture.md#dependency-rules) for how min handles
that today.

---

## Tools

Tools are how plugins extend what the **model** can do. The core stores and
dispatches them; schema and behaviour belong entirely to you:

```rust
struct ClockTool;

impl Tool for ClockTool {
    fn name(&self) -> &'static str { "clock" }
    fn spec(&self) -> &'static str {
        r#"{"description":"Current UTC time",
            "input_schema":{"type":"object","properties":{}}}"#
    }
    fn run(&self, _input: &str) -> Result<String, String> {
        Ok(format_utc(secs_since_epoch()))
    }
}
```

- `spec` is a JSON object string; the registry adds `"name"` when building
  the request for the model.
- `input` is the model's JSON arguments as a string — parse it with
  `serde_json` (or anything).
- Return `Err` to mark the result as a tool error; the agent reports it to
  the model, which usually retries or changes course.
- Names must be unique; a later registration replaces the earlier one.

---

## Worked example: the clock plugin, complete

This is `crates/min-tools-clock` — real, shipped code (comments trimmed).
A crate, a struct, two impls:

```rust
use min_core::{Hub, Plugin, Tool};

pub struct ClockPlugin;                       // the plugin: registers the tool

impl Plugin for ClockPlugin {
    fn name(&self) -> &'static str { "clock" }
    fn start(&mut self, hub: &Hub) {
        hub.tools().register(Box::new(ClockTool));
    }
}

struct ClockTool;                             // the capability itself

impl Tool for ClockTool {
    fn name(&self) -> &'static str { "clock" }
    fn spec(&self) -> &'static str {
        r#"{"description":"Current UTC time",
            "input_schema":{"type":"object","properties":{}}}"#
    }
    fn run(&self, _input: &str) -> Result<String, String> {
        Ok(format_utc(secs_since_epoch()))    // Err(_) → tool error for the model
    }
}
```

## Shipping it: five steps

1. `cargo new crates/min-tools-<x> --lib`; depend on `min-core` only
   (plus whatever your capability needs — but never another plugin).
2. Write the structs — one of the three shapes above, or a mix.
3. Add the crate to the workspace `Cargo.toml`.
4. Add one line to `src/main.rs`, **in load order** (UI first; things that
   register tools before the agent).
5. Write `docs/plugins/<name>.md` — the required architecture document
   ([template above](#plugin-documentation-required)) — and add it to the
   [plugin index](#the-plugins).

Then `cargo build && cargo test --workspace` — and if you're adding the
capability to min itself, ask min to do it; it can edit its own source.

---

## Rules of thumb

- **Plugins never import each other.** Coordinate through events, services,
  or the tool registry — all core-owned vocabulary.
- **Blocking belongs in `wait`.** Nothing else in a plugin may block.
- **No threads, no async, no locks.** The core is synchronous and
  single-threaded; a plugin that spawns threads breaks the model.
- **Don't touch the core.** New capability → new plugin. The only accepted
  core change is a new event *type* in `events.rs`.
- **Degrade gracefully.** `service::<T>()` can return `None`; your plugin
  should still be loadable when its provider is missing.
- **Document the plugin.** `docs/plugins/<name>.md`, per the required
  template, added to the index — in the same change as the code.
