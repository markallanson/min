# min-agent — the agent loop

The plugin that makes min an agent. It owns the conversation: on
[`UserInput`] it appends the user's message to its history and then loops —
call the LLM service → stream text out as it arrives → run any tool calls
through the core's tool registry → feed the results back — until the model
stops asking for tools.

There is deliberately nothing clever here: no subagents, no planning, no
context-window management. This is the simplest loop that can edit code.

```
        UserInput(text)                      (from the repl, via the bus)
             │
             ▼
 ┌────────────────────────────── turn() ──────────────────────────────┐
 │  system = base prompt + collected sections                         │
 │  messages.push(user)                                               │
 │                                                                    │
 │  ┌────────────────────── up to MAX_ROUNDS ────────────────────┐    │
 │  │  llm = hub.service::<min_llm::Llm>()   ── none? → Error ──▶│────┼──▶ out
 │  │  tools = hub.tools().specs()                               │    │
 │  │  emit LlmStart {round, model, messages, tools}             │    │
 │  │  msg = llm.complete(...) ┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅┅ │    │
 │  │        ├─ on thinking/tool JSON: emit Thinking             │    │
 │  │        └─ on each delta: emit AssistantDelta               │    │
 │  │  emit MessageEnd                                           │    │
 │  │  messages.push(assistant)                                  │    │
 │  │  tool_use blocks? ── none ──────────────────▶ return ─────▶│────┼──▶ done
 │  │  for each call: emit ToolStart → tools().run() → ToolEnd   │    │
 │  │  messages.push(tool_result envelope)                       │    │
 │  └────────────────────────────────────────────────────────────┘    │
 │  > MAX_ROUNDS rounds → emit Error("gave up…")                      │
 └────────────────────────────────────────────────────────────────────┘
```

Everything on the right edge of the diagram is an event; the LLM call is
the only line of work that leaves the plugin (via the service).

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-agent` / `"agent"` |
| shape | listener + the harness's worker (all work happens inside `handle`) |
| tools offered | none |
| services published | none |
| services consumed | `min_llm::Llm` — the one documented import exception |
| deps | `min-core`, `min-llm` (type only), `serde_json` |

## State

- `messages: Vec<Value>` — the whole conversation, Anthropic-style
  (`{"role", "content"}`); cleared on `ResetConversation`.
- `system: String` — the base system prompt, built once in `start`
  (identity, cwd, platform, date, working style).
- `sections: Vec<(title, body)>` — extra prompt sections handed over via
  events (see below), appended under `# <title>` after the base prompt on
  every call.

## Events

**Listens:**

| event | reaction |
|---|---|
| `UserInput` | run a full turn (blocks the bus for its duration — that is by design; nothing else is happening) |
| `SystemPromptSection` | collect `(title, body)` into `sections` |
| `ClearPromptSections` | drop all collected sections (the emitter re-sends fresh ones immediately after) |
| `ResetConversation` | clear `messages`, emit `Info("conversation cleared")` |

**Emits:**

| event | when |
|---|---|
| `LlmStart` | before each `llm.complete` call — `round` (1-based), `model`, `messages` count, `tools` count |
| `Thinking` | every non-text stream chunk from `on_thinking` — `text` (reasoning tokens) and `tool` (name of the tool call being streamed) |
| `AssistantDelta` | every streamed text chunk, forwarded from the LLM's `on_delta` callback |
| `MessageEnd` | after each complete assistant message |
| `ToolStart` | before each tool runs — `name` + a one-line `brief` (`command`'s first line or `path`, capped at 80 chars) |
| `ToolEnd` | after each tool — `ok`, `secs`, `note` (first non-blank line, 100 chars), `preview` (first 5 non-blank lines, 120 cols each, UTF-8-safe) |
| `Info` | retry/status chatter forwarded from the LLM's `on_status` |
| `Error` | LLM failure, missing LLM service, or the round-cap give-up |

It never receives its own emissions (a plugin executing `handle` is busy),
which is exactly why the streaming deltas go to the repl and not back into
the loop.

## Tool results

Each model tool call becomes a `tool_result` envelope appended as a *user*
message, in Anthropic shape — which min-llm translates for OpenAI-style
endpoints:

```json
{ "type": "tool_result", "tool_use_id": "<id from the call>",
  "content": "<tool output>", "is_error": false }
```

An `Err` from the tool becomes `is_error: true` with the error as content;
the model sees it and usually corrects course.

## The one import exception

`min-agent` is the only plugin that imports another plugin's crate —
`use min_llm::Llm` — and only to *name* the service type in
`hub.service::<Llm>()`. The lookup is core-mediated and optional: with no
LLM plugin loaded the agent emits an `Error` and keeps running. Details in
[architecture.md](../architecture.md#dependency-rules).

## Worked example

"what time is it?" — load order `…, repl, llm, …, clock, …, agent`:

```
key │ core                          repl      llm    clock      agent
────┼───────────────────────────────────────────────────────────────────
 ⏎  │ repl.wait ──▶ UserInput ──▶  [busy]
    │   delivered in order ─────▶ (busy)   ignores ignores ──▶ handle:
    │                                                     ┌────── turn()
    │                                                     │ push user msg
    │                                                     │ service::<Llm>() ✓
    │                                                     │ tools().specs()
    │◀─ AssistantDelta("Let me check…") ──────────────────│ llm.complete() ┅
    │ prints chunk                                        │
    │◀─ MessageEnd ───────────────────────────────────────│ stream done
    │                                                     │ tool_use: clock {}
    │◀─ ToolStart {clock} ────────────────────────────────│
    │ prints "▸ clock"                                    │
    │                                       run() ◀───────│ tools().run()
    │◀─ ToolEnd {ok, 0.0s} ───────────────────────────────│
    │ prints "✓ 0.0s …"                                   │ push tool_result
    │◀─ AssistantDelta("It's 09:41 UTC.") ────────────────│ no tool_use → done
    │◀─ MessageEnd                                        │
more│ repl.wait ──▶ (nothing yet) … loop idles on stdin
```

The whole turn runs inside the agent's one `handle(UserInput)` — which is
why it never sees its own events, only the repl does.

## Safety valve

`MAX_ROUNDS = 1000` LLM↔tool round-trips per user turn. A model stuck in a
tool-call loop ends the turn with an `Error` instead of hanging forever.

## What the tests pin

- `preview`: first five non-blank lines, 120 columns, cut on char
  boundaries (a byte-cut would tear a multi-byte character).
- Sections arrive via events, not file reads; `ClearPromptSections` drops
  old ones so a re-emitted file replaces rather than duplicates.

## See also

[min-llm](llm.md) (the service consumed) ·
[min-tools-*](tools-fs.md) (what `tools().run()` dispatches to) ·
[min-context](context.md) (where sections come from) ·
[plugins.md](../plugins.md)
