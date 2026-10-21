# min

A terminal agentic coding harness in Rust with a deliberately tiny core.
**Every feature — UI, LLM client, tools, the agent loop itself — is a plugin.**
The core is only a loop: it loads plugins and pumps events between them, with
zero functionality of its own. It has **zero dependencies**.

Read [docs/architecture.md](docs/architecture.md) for the full architecture:
diagrams, event semantics, dependency rules, worked examples, and how to
write a plugin.

**Note this is not designed for use by anyone other than myself. It's a wildly
incomplete coding agent. Completely bare. It's a play-thing. It's not suitable
for you or your friends now, and likely never will be.**

## Use

```sh
cargo install --path .   # puts `min` on your PATH
export ANTHROPIC_API_KEY=sk-...
min
```

Built-in endpoint aliases make common providers a one-liner:

```sh
# Z.ai coding plan (OpenAI-compatible endpoint, model defaults to glm-5.3):
export MIN_API=zai MIN_API_KEY=<your z.ai key>
min
```

Gemini on Vertex AI, authenticated with Google Application Default
Credentials (no API key — `gcloud auth application-default login`, a
service-account key in `GOOGLE_APPLICATION_CREDENTIALS`, or the metadata
server on GCP):

```sh
export MIN_API=vertex GOOGLE_CLOUD_PROJECT=<project>   # model defaults to google/gemini-2.5-pro
export GOOGLE_CLOUD_LOCATION=europe-west1              # optional, default global
min
```

Any other OpenAI-compatible endpoint (OpenRouter, vLLM, …):

```sh
export MIN_API=openai
export MIN_BASE_URL=https://openrouter.ai/api/v1   # or another endpoint
export MIN_API_KEY=...
export MIN_MODEL=<model>                           # whatever it serves
```

Ollama, local or on ollama.com (native `/api/chat`, no key needed locally):

```sh
export MIN_API=ollama
export MIN_MODEL=qwen3-coder                       # any tool-capable model
# export OLLAMA_HOST=https://ollama.com OLLAMA_API_KEY=...   # cloud models
```

| env var | default | meaning |
|---|---|---|
| `MIN_API` | `anthropic` | `anthropic` · `openai` · `zai` (Z.ai coding plan) · `openrouter` · `vertex` (Gemini on Vertex AI) · `ollama` (local or ollama.com) — an alias sets dialect + base URL + default model |
| `MIN_BASE_URL` | `https://api.anthropic.com` | API base URL (overrides the alias's choice) |
| `MIN_MODEL` | `claude-sonnet-4-5` | model name (overrides the alias's choice) |
| `MIN_MAX_TOKENS` | `16384` | max output tokens |
| `ANTHROPIC_API_KEY` / `MIN_API_KEY` / `OPENAI_API_KEY` | — | API key |
| `GOOGLE_CLOUD_PROJECT` / `GOOGLE_CLOUD_LOCATION` | ADC's project / `global` | Vertex project and region (`MIN_API=vertex`) |
| `GOOGLE_APPLICATION_CREDENTIALS` | gcloud's ADC file | Google credentials (`MIN_API=vertex`) |
| `HTTPS_PROXY`, `NO_COLOR` | — | honoured as usual |

```sh
min                          # a fresh session id every run
min --resume                 # resume the latest session in this project
min --resume 1729180001-k3f9 # resume a specific session by id
min --sessions               # list sessions in this project
min --session 1729180000     # pin the first session's id
min --parent-session 1729179990   # the first session is its child
```

Commands: `/help`, `/usage` (token ledger for this session), `/clear` (new session id + reset conversation), `/quit`.
Ctrl-C exits at any moment (even mid-generation); Ctrl-D quits at an empty
prompt. Piping works, for scripting: `echo 'explain this repo' | min`.

### 429s — two very different meanings

The z.ai/bigmodel API answers **429** both for real rate limits *and* for a
plan/endpoint mismatch, so check the message body:

- `Insufficient balance or no resource package` (code **1113**) — the key
  doesn't have a coding-plan resource package **on that endpoint**. Plans are
  regional: a key from one portal gets 1113 on the other
  (`api.z.ai` ↔ `open.bigmodel.cn`). min automatically retries the mirror
  portal for the session and says so; if both fail, the error includes a
  fingerprint of the key so you can tell *which* key was rejected. General
  `.../api/paas/v4` endpoints never accept coding-plan keys.
- anything else — a real rate/concurrency limit. Coding plans limit
  **concurrent** generations per key, allowance notwithstanding (other
  clients on the same key count too). min retries with exponential backoff
  (1→32s, honouring `Retry-After`) and shows `retrying in Ns` while waiting.

## Architecture in one paragraph

`min-core` provides four nouns — a `Plugin` trait, an event bus
(`Hub::emit`), typed services (`Hub::service`), and a tool registry
(`Hub::tools`) — and one verb: the run loop, which starts plugins, then
alternates between calling `wait` on idle plugins (this is where blocking
happens — the repl blocks on stdin) and dispatching each returned event to
every plugin's `handle`, in load order, synchronously. That is the entire
concurrency model; see [docs/architecture.md](docs/architecture.md).

### Writing a plugin

A crate, a struct, two impls, one line in `src/main.rs`. The full guide —
the three plugin shapes, events, services, tools, and a complete working
example — is in [docs/plugins.md](docs/plugins.md).

## Self-hosting

min already has the minimum to grow itself: it can read its own source,
edit it (exact-match edits), and run `cargo build` / `cargo test`. Run it
from this repository and ask it to add a plugin.

## Working on min

```sh
git config core.hooksPath .githooks   # enables the pre-commit hook
cargo install cargo-machete --locked  # the hook uses it
```

The pre-commit hook runs `cargo machete --fix`, so a dependency nothing
imports is stripped and staged with the commit. The same check is a cargo
lint (`.cargo/config.toml`), so `cargo +nightly check --workspace --all-targets`
reports it too — it is nightly-only, hence the `+nightly`.

## Limitations (on purpose, for now)

- no context-window compaction
- no way to interrupt a running turn except Ctrl-C (which exits min)
- no cost accounting (token counts are tracked, but not priced), no retries mid-stream, no diff preview
- multiline input is one line; no tab completion

## Layout

See [docs/architecture.md](docs/architecture.md) for a guided tour of every
crate. In brief: `crates/min-core` is the loop; `min-repl`, `min-llm`,
`min-agent`, `min-context`, `min-session`, `min-subagents`, `min-usage`,
`min-herdr` and `min-tools-*` are the plugins (UI, LLM client, agent loop,
project instructions, session identity + JSONL store/resume, subagents,
token accounting, Herdr reporting, filesystem/shell/clock tools);
`src/main.rs` is the only glue — the plugin list.
