# min-usage — token accounting for the session

Owns one number: how many tokens this session has used. It listens for
[`Usage`] — the normalized per-request report the agent emits after every
LLM round — and folds each one into a `UsageLedger`, which it publishes as
a service. Any other plugin can then ask "how much has this session used"
without keeping a tally of its own:

```rust
if let Some(ledger) = hub.service::<UsageLedger>() {
    let snapshot = ledger.snapshot();
    // snapshot.total.input_tokens, snapshot.last_prompt_tokens, …
}
```

**The plugin counts; it does not decide.** Compacting a conversation when
the context grows too large, warning about a context window, or billing
someone are all jobs for other plugins — and this plugin does not need to
know they exist.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-usage` / `"usage"` |
| shape | **listener** — records `Usage` events; publishes a service in `start` |
| tools offered | none |
| services published | `UsageLedger` (the session's running token total) |
| services consumed | none |
| deps | `min-core` only |

## The chain

```
  LLM round ends
      │
      ▼
  min-llm   normalizes the provider's usage into TokenUsage and puts it
            in the response:  {"stop_reason", "content", "usage"}
      │
      ▼
  min-agent reads message["usage"] ──▶ emits events::Usage { usage, model, round }
      │
      ├──────────────▶ min-usage  ──▶ ledger.record(usage)   (the running total)
      ├──────────────▶ min-session ──▶ {"type":"usage",…}     (the .jsonl record)
      └──────────────▶ min-repl    ──▶ /usage prints the ledger
```

The number travels the same way every other fact does: as an event. The
agent never talks to the usage plugin, and the usage plugin never talks to
the LLM.

## Normalized usage

`min-core/src/usage.rs` holds the vocabulary — two plain-data types, no
behaviour in the core:

```rust
pub struct TokenUsage {
    pub input_tokens: u64,        // prompt tokens not served from cache
    pub output_tokens: u64,       // tokens the model generated
    pub cache_read_tokens: u64,   // prompt tokens served from the cache
    pub cache_write_tokens: u64,  // prompt tokens written into it
}
```

Each endpoint counts its own way, and the plugin that talks to it does the
normalizing:

| endpoint | where the count arrives | how it becomes `TokenUsage` |
|---|---|---|
| Anthropic | `message_start.message.usage` (prompt) and `message_delta.usage` (generated) | `input_tokens` = `input_tokens`; `cache_read_tokens` = `cache_read_input_tokens`; `cache_write_tokens` = `cache_creation_input_tokens` |
| OpenAI / Z.ai / OpenRouter | a trailing chunk with `"choices": []`, sent only because the request sets `stream_options.include_usage` | `input_tokens` = `prompt_tokens`; `output_tokens` = `completion_tokens`; `cache_read_tokens` = `prompt_tokens_details.cached_tokens` |
| Vertex | the same, if the endpoint honours `include_usage` | as OpenAI — and if it does not, the usage is simply absent |
| Ollama | the final NDJSON line | `input_tokens` = `prompt_eval_count`; `output_tokens` = `eval_count` |

Two rules make the dialects agree:

- **The prompt is `input + cache_read + cache_write`.** Anthropic counts
  cached tokens *apart* from the prompt tokens, so `TokenUsage::prompt_tokens()`
  adds them back; OpenAI's `prompt_tokens` already includes its cached
  ones. Both end up as the number a context window is measured against.
- **No report is not zero.** `TokenUsage::is_empty()` is true when nothing
  was reported at all. An endpoint that reports no usage produces an
  *empty* record, never a "free" one — `/usage` says so in words rather
  than printing zeros.

## The ledger

```rust
pub struct UsageLedger { … }   // min-core/src/usage.rs
```

The counters live behind `Rc<…>` of `Cell`s, so:

- `record(&self, usage)` works through the shared reference the hub hands
  out (`hub.service::<UsageLedger>()`), without a `set_service` while a
  `Ref` is alive;
- `snapshot(&self) -> UsageSnapshot` copies the state out — cheap, and it
  borrows nothing, so a consumer can hold it while printing;
- **cloning a ledger shares its counters.** The plugin keeps one handle to
  record into and publishes another; they are the same numbers, not two
  copies. That is what makes the service and the plugin agree.

`UsageSnapshot` carries `requests`, the summed `total`, the
`last_prompt_tokens` (the context as the provider last saw it) and
`reported` (whether anything was reported at all).

A request that reported nothing still counts as a request, but does not
move the totals and does not overwrite the last known context size — an
endpoint that reports usage only sometimes must not drag the numbers back
to zero.

## Events

**Listens:**

| event | reaction |
|---|---|
| `Usage` | `ledger.record(usage)` |

**Emits:** nothing. The plugin's only output is the service it publishes.

## Worked example

One turn against a provider that reports 4242 prompt tokens and 17
generated tokens:

```
key │ core                          agent        usage        repl
────┼────────────────────────────────────────────────────────────────────
 ⏎  │ repl.wait ──▶ UserInput ──▶  [busy: typed]
    │   delivered in order ──────────────────────────────▶ handle: turn()
    │◀──── LlmStart {round: 1} ────────────────────────────│
    │◀──── AssistantDelta("hello") ────────────────────────│  llm.complete() ┅
    │◀──── MessageEnd ────────────────────────────────────│
    │◀──── Usage {usage: {input: 4242, output: 17}, round: 1} ─▶│
    │                                        ledger.record ──┤  in 4242 · out 17
    │                                                     │  context 4242
more│ repl.wait ──▶ (nothing yet) … loop idles on stdin
```

Then, at the prompt:

```
❯ /usage
  usage: 1 request(s) · in 4242 · out 17 · total 4259 · context now 4242
```

`/usage` never prints zeros for an endpoint that reports nothing:

```
❯ /usage
  usage: 3 request(s) — this endpoint does not report token usage
```

## What the tests pin

- `min-core`: `prompt_tokens` adds cache tokens back in; the ledger
  accumulates across requests; a request without usage counts but moves no
  numbers; clones share one set of counters.
- `min-llm`: each dialect's usage is parsed and normalized; the OpenAI
  usage chunk is read even though it carries no choices; the OpenAI body
  asks for usage; the stream is read past the finish reason.
- `min-agent`: a round emits `Usage` with the provider's numbers; a
  provider that reports nothing yields an empty usage event.
- `min-repl`: `/usage` renders the ledger, and says "not loaded" or "does
  not report" instead of showing zeros.
- `min-session`: the `.jsonl` gets one `usage` record per request.
- `tests/usage_end_to_end.rs`: a real HTTP request over a real socket,
  through the real agent, into the real ledger — 4242 in, 17 out.

## See also

[min-llm](llm.md) (where the numbers come from) · [min-agent](agent.md)
(where they are put on the bus) · [min-session](session.md) (where they are
persisted) · [min-repl](repl.md) (where `/usage` prints them) ·
[plugins.md](../plugins.md) (the guide this crate implements)
