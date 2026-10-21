# min-llm — the LLM client plugin

Publishes the `LlmRouter` service and registers one provider per dialect
into it — a streaming chat client for exactly one configured endpoint. Every
dialect is normalized to Anthropic-style content blocks, so the rest of min
never cares which wire format is in use:

```json
{ "stop_reason": "end_turn" | "tool_use" | "max_tokens",
  "content": [ {"type": "text", "text": "…"},
               {"type": "tool_use", "id": "…", "name": "…", "input": {…}} ] }
```

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-llm` — six plugins: `"llm-router"`, `"llm-anthropic"`, `"llm-openai"`, `"llm-vertex"`, `"llm-zai"`, `"llm-ollama"` |
| shape | **provider** — publishes a service; listens to nothing |
| tools offered | none |
| services published | `LlmRouter` (one per process; consumed by min-agent via `hub.service::<LlmRouter>()`) |
| deps | `min-core`, `ureq`, `serde_json`, `ring` + `base64` (Google service-account JWTs; already in the tree via rustls) — the largest crate, absorbing all wire-format ugliness |

## Configuration (environment)

| var | default | meaning |
|---|---|---|
| `MIN_API` | `anthropic` | dialect or alias: `anthropic` · `openai` · `zai` (Z.ai coding plan) · `openrouter` · `vertex` · `ollama` |
| `MIN_BASE_URL` | the alias's endpoint | overrides the endpoint |
| `MIN_MODEL` | the alias's model (`claude-sonnet-4-5` etc.) | overrides the model |
| `MIN_MAX_TOKENS` | `16384` | max output tokens |
| keys | — | `ANTHROPIC_API_KEY` (anthropic dialect); `MIN_API_KEY` / `OPENAI_API_KEY` (openai-style); `ZAI_API_KEY` (`MIN_API=zai`) |
| `GOOGLE_CLOUD_PROJECT` (also `CLOUDSDK_CORE_PROJECT`, `GCLOUD_PROJECT`) | the credentials' project | Vertex project (`MIN_API=vertex`) |
| `GOOGLE_CLOUD_LOCATION` / `GOOGLE_CLOUD_REGION` | `global` | Vertex region |
| `GOOGLE_APPLICATION_CREDENTIALS` | gcloud's well-known ADC file | Google credentials file |
| `OLLAMA_HOST` | `http://localhost:11434` | Ollama server (`MIN_API=ollama`); a bare `host[:port]` is accepted |
| `OLLAMA_API_KEY` | — | optional bearer token (e.g. `OLLAMA_HOST=https://ollama.com` for cloud models) |
| `MIN_NUM_CTX` | `32768` | Ollama `options.num_ctx` — the server default is small and silently truncates |
| `HTTPS_PROXY` / `https_proxy` | — | outbound proxy |

An alias resolves to `(dialect, base URL, default model)`:

| `MIN_API` | dialect | base | default model |
|---|---|---|---|
| *(unset)* / `anthropic` | `anthropic` | `https://api.anthropic.com` | `claude-sonnet-4-5` |
| `openai` | `openai` | `https://api.openai.com/v1` | `gpt-4o` |
| `zai` | `openai` | `https://api.z.ai/api/coding/paas/v4` | `glm-5.3` |
| `openrouter` | `openai` | `https://openrouter.ai/api/v1` | `anthropic/claude-sonnet-4.5` |
| `ollama` | `ollama` (native `/api/chat`) | `$OLLAMA_HOST` or `http://localhost:11434` | `qwen3-coder` |
| `vertex` / `vertexai` | `openai` wire, Google auth | `https://{region-}aiplatform.googleapis.com/v1/projects/{p}/locations/{region}/endpoints/openapi` | `google/gemini-2.5-pro` |

### Vertex AI and Application Default Credentials

`MIN_API=vertex` speaks the OpenAI dialect to Vertex AI's OpenAI-compatible
endpoint, but authenticates with a short-lived OAuth token from Google
Application Default Credentials instead of a key (`src/google.rs`):

```
 GOOGLE_APPLICATION_CREDENTIALS ─┐
 gcloud well-known ADC file ─────┼─▶ authorized_user ─▶ refresh-token exchange ─┐
   (application-default login)   │   service_account ─▶ RS256 JWT (ring) ───────┼─▶ access token
 neither ────────────────────────┴─▶ metadata server (GCE/GKE/Cloud Run) ───────┘   cached, refreshed
                                                                                    5 min before expiry
```

- Locating credentials happens at `start` without network I/O; the first
  token is fetched on the first `complete`. Token failures are `Fail::Auth`
  and are never retried.
- The project comes from the environment, else the credentials file
  (`project_id`, then `quota_project_id`), else the metadata server.
  A user credential's `quota_project_id` is also sent as `x-goog-user-project`.
- Unsupported credential types (`external_account`,
  `impersonated_service_account`) are reported as a startup warning.
- Gemini requests `extra_body.google.thinking_config.include_thoughts = true`
  (plus `thinking_level = "HIGH"` on Gemini 3/4 models) and streams thought
  summaries as `delta.content` chunks tagged with
  `extra_content.google.thought = true`, which `parse_openai_chunk` routes to
  `on_thinking` rather than `on_delta`.
- Gemini attaches a *thought signature* to tool calls
  (`extra_content.google.thought_signature`) that must be echoed back on the
  next turn; it rides along on the normalized `tool_use` block as
  `extra_content`. Tool calls without an id get `call_<n>` so results still
  pair up.

### Ollama

`MIN_API=ollama` speaks Ollama's native `/api/chat` rather than its
OpenAI-compatible endpoint, because only the native API accepts
`options.num_ctx`. Without it, Ollama's default context window cuts off
an agent's system prompt and history without reporting it.

- No key is needed for a local server (`Auth::None`). `OLLAMA_API_KEY`
  (or `MIN_API_KEY`), if set, is sent as `Authorization: Bearer …`.
- The response streams as bare NDJSON lines rather than SSE `data:`
  lines. `message.thinking` goes to `on_thinking`, and `message.content`
  goes to `on_delta`.
- Tool calls arrive complete, with `arguments` already parsed into an
  object, and usually without an id, so `Blocks.finish` assigns `call_<n>`.
  Ollama reports `done_reason: "stop"` even when the turn ended in tool
  calls, so any tool call makes the stop reason `tool_use`.
- On the way out, `to_ollama_messages` reuses the OpenAI conversion, turns
  tool arguments back into objects, and tags `role:"tool"` results with
  `tool_name`, which Ollama uses to pair a result with its call.
- `max_tokens` is sent as `options.num_predict`. Errors come back as
  `{"error": "…"}` with a string value.

## What `start` does

`RouterPlugin` publishes the router; each provider plugin registers itself
with it, but only if `MIN_API` selects its dialect:

1. `RouterPlugin` — `hub.set_service(LlmRouter::default())`.
2. `AnthropicPlugin` — registers when `MIN_API` is `anthropic` or unset.
3. `OpenAiPlugin` / `VertexPlugin` / `ZaiPlugin` / `OllamaPlugin` — register
   when `MIN_API` is `openai` / `vertex` / `zai` / `ollama` respectively.

A registered provider is an `Llm::from_env()` built from the configuration
above. No provider is registered when `MIN_API` names a dialect that has no
plugin, and the router then fails the agent's call with
`no LLM provider configured for model …`.

## Events

**Listens:** nothing — the plugin is a pure provider.

**Emits:**

| event | when |
|---|---|
| `Info` | never — the provider reports progress through the `on_status` callback, which the agent forwards onto the bus |
| `Error` | never — `complete` returns failures to the caller (min-agent turns them into `Error`s) |

The plugin emits no usage events of its own — it *reports* usage by
including it in the normalized response, and min-agent puts it on the bus
as `events::Usage`. See [min-usage](usage.md) for the whole chain.

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
      │                ├─ thinking / tool JSON → on_thinking(text, tool) ──▶ repl spinner
      │                └─ text deltas → Blocks + on_delta(t)             ──▶ repl, live
      ▼
 Blocks.finish() ──▶ normalized Anthropic-style Value (see top)
```

Key rules:

- **A retry never replays text.** Once any delta has reached the user, a
  failure aborts — the caller keeps what was printed.
- History is Anthropic-style internally; `to_openai_messages` converts
  assistant `tool_use` blocks to `tool_calls` and user `tool_result` blocks
  to `role:"tool"` messages on the way out.

## Token usage

The provider's own token count is parsed per dialect and normalized into
`min_core::usage::TokenUsage`, then included in the response:

```json
{ "stop_reason": "end_turn",
  "content": [ … ],
  "usage": { "input_tokens": 4242, "output_tokens": 17,
             "cache_read_tokens": 0, "cache_write_tokens": 0 } }
```

Where each dialect puts it, and the two things that have to be right:

| dialect | where | note |
|---|---|---|
| Anthropic | `message_start.message.usage` (prompt) + `message_delta.usage` (generated) | cache tokens are counted *apart* from the prompt tokens |
| OpenAI family | a trailing chunk with `"choices": []` | only sent because the body sets `stream_options.include_usage` |
| Ollama | the final NDJSON line | `prompt_eval_count` / `eval_count` |

- **The usage chunk must be read before the choices.** The trailing chunk
  carries no choices at all, so a parser that looks for `choices[0]` first
  drops the count on the floor.
- **The stream must not stop at the finish reason.** The usage chunk
  arrives *after* the one carrying `finish_reason`, so an OpenAI-family
  stream is read to its end (`[DONE]`) rather than cut short there.

Endpoints that report no usage (Vertex may not) yield an *empty* usage,
never a zero-cost one — see [min-usage](usage.md).

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
- `on_thinking` callbacks fire for Anthropic `thinking`/`tool_use`/`input_json_delta`
  chunks and OpenAI `reasoning_content`/`tool_calls` chunks (carrying the tool name
  across subsequent argument fragments).
- Plan-mismatch 429s are not retryable; genuine 429/5xx/transport errors
  are; 401 and mid-stream errors are not.
- `mirror_base` is bidirectional for the two coding portals and `None` for
  anything else (including the general `api/paas/v4` endpoint).
- Alias resolution and the Anthropic→OpenAI history conversion.
- Ollama: `OLLAMA_HOST` normalization, native body shape (`num_ctx`,
  object arguments, `tool_name` on results), and NDJSON chunk parsing
  (thinking, whole tool calls, `stop`→`tool_use`, `length`→`max_tokens`, errors).
- Vertex endpoint construction (global vs regional host); Gemini thought
  signatures and missing tool-call ids survive the round trip.
- ADC file parsing: `authorized_user`, `service_account` (needs a PEM key),
  unsupported types rejected.
- `cancel_reader_aborts_on_cancellation_or_interrupted` and `complete_aborts_immediately_if_cancelled`:
  in-flight reads and completions abort immediately when `is_cancelled()` is set.
- Token usage: each dialect's count is parsed and normalized; the OpenAI
  usage chunk is read even though it carries no choices; the OpenAI body
  asks for usage (`stream_options.include_usage`); the stream is read past
  the finish reason. `usage_survives_a_real_stream` proves it over a real
  socket, and fails if either early exit is put back.

## See also

[min-agent](agent.md) (the consumer of the `LlmRouter` service) ·
[min-usage](usage.md) (where the token counts go) ·
[README](../../README.md) (429 field guide) · [plugins.md](../plugins.md)
