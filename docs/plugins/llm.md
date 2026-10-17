# min-llm — the LLM client plugin

Publishes the `Llm` service — a streaming chat client for exactly one
configured endpoint — and normalizes every dialect to Anthropic-style
content blocks, so the rest of min never cares which wire format is in use:

```json
{ "stop_reason": "end_turn" | "tool_use" | "max_tokens",
  "content": [ {"type": "text", "text": "…"},
               {"type": "tool_use", "id": "…", "name": "…", "input": {…}} ] }
```

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-llm` / `"llm"` |
| shape | **provider** — publishes a service; listens to nothing |
| tools offered | none |
| services published | `Llm` (one per process; consumed by min-agent via `hub.service::<Llm>()`) |
| deps | `min-core`, `ureq`, `serde_json` — the largest crate, absorbing all wire-format ugliness |

## Configuration (environment)

| var | default | meaning |
|---|---|---|
| `MIN_API` | `anthropic` | dialect or alias: `anthropic` · `openai` · `zai` (Z.ai coding plan) · `openrouter` |
| `MIN_BASE_URL` | the alias's endpoint | overrides the endpoint |
| `MIN_MODEL` | the alias's model (`claude-sonnet-4-5` etc.) | overrides the model |
| `MIN_MAX_TOKENS` | `16384` | max output tokens |
| keys | — | `ANTHROPIC_API_KEY` (anthropic dialect); `MIN_API_KEY` / `OPENAI_API_KEY` (openai-style); `ZAI_API_KEY` (`MIN_API=zai`) |
| `HTTPS_PROXY` / `https_proxy` | — | outbound proxy |

An alias resolves to `(dialect, base URL, default model)`:

| `MIN_API` | dialect | base | default model |
|---|---|---|---|
| *(unset)* / `anthropic` | `anthropic` | `https://api.anthropic.com` | `claude-sonnet-4-5` |
| `openai` | `openai` | `https://api.openai.com/v1` | `gpt-4o` |
| `zai` | `openai` | `https://api.z.ai/api/coding/paas/v4` | `glm-5.3` |
| `openrouter` | `openai` | `https://openrouter.ai/api/v1` | `anthropic/claude-sonnet-4.5` |

## What `start` does

1. Build `Llm::from_env()` from the configuration above.
2. Warn via `Info` when no API key is set.
3. Greet via `Info` (`<model> @ <host> — /help for commands`).
4. `hub.set_service(llm)`.

## Events

**Listens:** nothing — the plugin is a pure provider.

**Emits:**

| event | when |
|---|---|
| `Info` | missing-key warning at `start`; the greeting; retry progress from `complete`'s `on_status` |
| `Error` | never from `start`; `complete` returns failures to the caller (min-agent turns them into `Error`s) |

## The `complete` pipeline

```
 build body ──── anthropic: {system, messages, tools, stream:true}
                openai:    {messages:[system,…], tools→functions, stream:true}
      │
      ▼
 POST {base}/v1/messages   (anthropic)
      {base}/chat/completions   (openai)
      │
      ▼
 read SSE lines ── data: {"type":"content_block_delta", …}  (anthropic)
                    data: {"choices":[{"delta":{…}}]}       (openai)
      │                │
      │                └─ text deltas → Blocks + on_delta(t)   ──▶ repl, live
      ▼
 Blocks.finish() ──▶ normalized Anthropic-style Value (see top)
```

Key rules:

- **A retry never replays text.** Once any delta has reached the user, a
  failure aborts — the caller keeps what was printed.
- History is Anthropic-style internally; `to_openai_messages` converts
  assistant `tool_use` blocks to `tool_calls` and user `tool_result` blocks
  to `role:"tool"` messages on the way out.

## Failure classification and retries

`Fail` classifies every failed attempt:

| failure | retryable? | policy |
|---|---|---|
| HTTP 429, body says *insufficient balance / no resource package / `"1113"`* | **no** — plan mismatch, not rate limiting | switch to the mirror portal (below), else fail with a hint + key fingerprint |
| HTTP 429, anything else | yes | exponential backoff `1→2→4→8→16→32s`, honouring `Retry-After` |
| HTTP 5xx | yes | same schedule |
| network / transport error | yes | same schedule |
| HTTP 4xx, stream error mid-flight | no | fail immediately |
| any failure after text streamed | no | fail immediately (a retry would duplicate output) |

Giving up reports the failure plus the total backoff spent (~63s).

### The z.ai mirror portal

The Z.ai coding plan is served from two regional portals; a key provisioned
on one gets "no resource package" (a 429, code 1113) on the other. On that
error, and only that error, min switches to the mirror **for the rest of the
session** and says so:

```
api.z.ai/api/coding/…        ⇄        open.bigmodel.cn/api/coding/…
```

If the current base is neither, there is no mirror and the 429 becomes a
final error. Errors include a fingerprint of the key (first 6 / last 4
chars) so you can tell *which* key was rejected.

## Worked example

A real rate limit (`429`, `Retry-After: 7`) on attempt 1:

```
agent: llm.complete(…)
  ├─▶ on_status("429 rate-limited — retrying in 7s (attempt 1/6)")  → bus → repl prints
  ├─ sleep 7s
  └─ attempt 2 → 200 → SSE deltas → on_delta("Hello") …
        → Ok({"stop_reason":"end_turn","content":[{"type":"text",…}]})
```

A plan mismatch on `zai`:

```
attempt 1 → 429 code 1113 → is_plan_mismatch ✓
  └─ base := open.bigmodel.cn/api/coding/paas/v4
     on_status("key has no resource package on https://api.z.ai/… — switching …")
     attempt 2 → 200 (or final 429 with hint + key fingerprint)
```

## What the tests pin

- `Blocks` normalization: partial `input_json_delta` fragments assemble into
  a parsed tool `input`.
- Plan-mismatch 429s are not retryable; genuine 429/5xx/transport errors
  are; 401 and mid-stream errors are not.
- `mirror_base` is bidirectional for the two coding portals and `None` for
  anything else (including the general `api/paas/v4` endpoint).
- Alias resolution and the Anthropic→OpenAI history conversion.

## See also

[min-agent](agent.md) (the only consumer of the `Llm` service — the one
documented import exception) · [README](../../README.md) (429 field guide) ·
[plugins.md](../plugins.md)
