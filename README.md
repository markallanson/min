# min

A terminal agentic coding harness in Rust with a deliberately tiny core.
**Every feature — UI, LLM client, tools, the agent loop itself — is a plugin.**
The core coordinates plugins and does nothing else. It is ~340 lines with
**zero dependencies**.

```
┌─────────────────────────── min binary (21 lines) ───────────────────────────┐
│  load plugins, call min_core::run()                                          │
└──────────────────────────────────────────────────────────────────────────────┘
   │                                                                           │
   │  plugins (never import each other — only min-core)                        │
   │                                                                           │
   │   min-repl   301 lines   terminal UI: raw-mode line editor, streaming     │
   │   min-llm    432 lines   streaming LLM client (anthropic + openai wire)   │
   │   min-agent  204 lines   the agent loop: LLM ↔ tools until done           │
   │   min-tools-fs     181   read / edit / write                              │
   │   min-tools-shell  147   bash with timeout + capture                      │
   ▼                                                                           │
┌───────────────────────────── min-core (no deps) ────────────────────────────┐
│  Plugin trait · event bus · services · tool registry · run loop             │
└──────────────────────────────────────────────────────────────────────────────┘
```

## Use

```sh
cargo install --path .   # puts `min` on your PATH
export ANTHROPIC_API_KEY=sk-...
min
```

Any OpenAI-compatible endpoint works too (OpenRouter, Z.ai, vLLM, …):

```sh
export MIN_API=openai
export MIN_BASE_URL=https://openrouter.ai/api/v1   # or another endpoint
export MIN_API_KEY=...
export MIN_MODEL=glm-5.3                           # whatever it serves
```

| env var | default | meaning |
|---|---|---|
| `MIN_API` | `anthropic` | wire dialect: `anthropic` or `openai` |
| `MIN_BASE_URL` | `https://api.anthropic.com` | API base URL |
| `MIN_MODEL` | `claude-sonnet-4-5` | model name |
| `MIN_MAX_TOKENS` | `16384` | max output tokens |
| `ANTHROPIC_API_KEY` / `MIN_API_KEY` / `OPENAI_API_KEY` | — | API key |
| `HTTPS_PROXY`, `NO_COLOR` | — | honoured as usual |

Commands: `/help`, `/clear` (reset conversation), `/quit`. Ctrl-C exits at any
moment (even mid-generation); Ctrl-D quits at an empty prompt. Piping works,
for scripting: `echo 'explain this repo' | min`.

## Design rules

1. **The core does nothing but coordinate.** `min-core` has no I/O, no
   async, no deps. It cannot even print. Every capability is a plugin.
2. **Plugins never import each other.** They communicate only through
   core-owned vocabulary: `emit(Event)`, `service::<T>()`, `tools()`.
   Drop a plugin crate from the build and the rest still runs.
3. **Synchronous, single-threaded, in-order.** `emit` delivers immediately.
   Idle work happens in `Plugin::wait`, which may block (the repl blocks on
   stdin — that's the whole event source). No tokio, no channels, no locks.
4. **Complexity lives at the edge.** The biggest plugin (llm, ~430 lines)
   absorbs wire-format uglyness so the agent loop can stay trivial.

### Event semantics

`hub.emit(e)` calls every plugin's `handle` right away, in load order. A
plugin that is itself executing does not receive the event (nothing is
queued). `Plugin::wait` returns an optional event that is emitted only after
`wait` returns, so it reaches everyone — including the plugin that produced
it. This is the entire concurrency model.

### The plugin contract

```rust
pub trait Plugin {
    fn name(&self) -> &'static str;
    fn start(&mut self, _hub: &Hub) {}                      // register things
    fn wait(&mut self, _hub: &Hub) -> Option<Box<dyn Any>> { None } // idle; may block
    fn handle(&mut self, _hub: &Hub, _event: &dyn Any) {}   // react to events
}

pub trait Tool {
    fn name(&self) -> &'static str;
    fn spec(&self) -> &'static str;   // {"description":…,"input_schema":…}
    fn run(&self, input: &str) -> Result<String, String>;
}
```

## Writing a plugin

A crate, a struct, two impls, one line in `src/main.rs`:

```rust
// crates/min-tools-clock/src/lib.rs
use min_core::{Hub, Plugin, Tool};

pub struct ClockPlugin;
impl Plugin for ClockPlugin {
    fn name(&self) -> &'static str { "clock" }
    fn start(&mut self, hub: &Hub) { hub.tools().register(Box::new(ClockTool)); }
}

struct ClockTool;
impl Tool for ClockTool {
    fn name(&self) -> &'static str { "clock" }
    fn spec(&self) -> &'static str {
        r#"{"description":"Current UTC time","input_schema":{"type":"object","properties":{}}}"#
    }
    fn run(&self, _input: &str) -> Result<String, String> {
        Ok(format!("{}s since epoch", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()))
    }
}
```

That's the whole API surface. A UI-less plugin can also just listen for
events (`handle`) or publish state for others (`set_service`).

## Self-hosting

min already has the minimum to grow itself: it can read its own source,
edit it (exact-match edits), and run `cargo build` / `cargo test`. Run it
from this repository and ask it to add a plugin.

## Limitations (on purpose, for now)

- single conversation, no session persistence, no context-window management
- no way to interrupt a running turn except Ctrl-C (which exits min)
- no token/cost accounting, no retries mid-stream, no diff preview
- multiline input is one line; no tab completion

## Layout

```
crates/min-core          the bus: Plugin, Hub, events, Tool, ToolRegistry
crates/min-repl          terminal UI
crates/min-llm           Anthropic Messages + OpenAI chat client (streaming)
crates/min-agent         conversation + tool loop, system prompt
crates/min-tools-fs      read / edit / write
crates/min-tools-shell   bash (timeout, output capture, truncation)
src/main.rs              the only glue
```
