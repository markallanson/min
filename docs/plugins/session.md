# min-session — session identity, JSONL store, listing, and resume

Owns the full session lifecycle: minting unique `<epoch>-<rand>` session IDs,
announcing [`SessionStart`] / [`SessionEnd`] on the bus, persisting every
session to an append-only JSONL file under the XDG state directory, listing
past sessions (`min --sessions`), and resuming sessions in-place
(`min --resume [<id>]`).

**The events *are* the interface.** There is deliberately no session service;
plugins that track the active session (`min-herdr`, `min-subagents`) listen
for `SessionStart`, and conversation persistence and replay happen through
`ConversationMessage`, `ReplayMessage`, and `ReplayExchange` events.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-session` / `"session"` |
| shape | source (announces from first `wait`) + listener (persists and replays events) |
| tools offered | none |
| services published / consumed | none |
| deps | `min-core`, `serde_json` |

## Ids & CLI modes

```
<epoch seconds>-<4 random alphanumeric characters>
      │                  │
      │                  └─ /dev/urandom → 4 bytes → base-36 alphabet
      │                     (no /dev: clock's sub-second nanos)
      └─ keeps ids sorting chronologically
```

Command line (parsed in `src/main.rs`, passed to `SessionPlugin::with_mode`):

| invocation | mode (`SessionMode`) | behaviour |
|---|---|---|
| `min` | `Fresh { parent: None }` | mint fresh `<epoch>-<rand>` id, open new `<id>.jsonl` |
| `min --parent-session <p>` | `Fresh { parent: Some(p) }` | mint fresh id with `parent = p`, open new `<id>.jsonl` |
| `min --session <id>` | `Pinned(id)` | use `<id>` for the startup session (resuming if `<id>.jsonl` exists) |
| `min --resume` | `Resume(None)` | resume the newest root (`parent == null`) session in the project |
| `min --resume <id>` | `Resume(Some(id))` | resume `<id>.jsonl` in the project (error + quit if missing) |
| `min --sessions` | `list_sessions()` | print root sessions newest-first with age, first prompt, and subagent count |

## Storage & JSONL schema

One file per session under the XDG state tree:

```
$XDG_STATE_HOME/min/sessions/<project>/<id>.jsonl
~/.local/state/min/sessions/<project>/<id>.jsonl    (XDG_STATE_HOME unset/empty)
```

where `<project>` is the slug of the working directory's basename (`min`,
`root` when `/`). Every line is a single-line JSON object with a `"type"`
discriminator:

| event / action | JSONL line |
|---|---|
| New session (`SessionStart`) | `{"type":"session_start","id":"<id>","parent":<str\|null>,"ts":<secs>,"cwd":"<cwd>"}` |
| Resumed session (`SessionStart`) | `{"type":"session_resume","id":"<id>","ts":<secs>,"cwd":"<cwd>"}` |
| `SystemPromptSection` | `{"type":"instructions","title":"<title>","body":"<body>"}` |
| `ConversationMessage` | `{"type":"message","role":"user"\|"assistant","content":...}` |
| `ToolStart` | `{"type":"tool","name":"<name>","brief":"<brief>"}` |
| `ToolEnd` | `{"type":"tool_end","ok":<bool>,"secs":<f64>,"note":"<note>"}` |
| `Info` | `{"type":"info","text":"<text>"}` |
| `Error` | `{"type":"error","text":"<text>"}` |
| `SessionEnd` | `{"type":"session_end","ts":<secs>}` |

## Lifecycle & resume flow

```
 boot    start()  — silent; buffers any boot-time SystemPromptSection events
         │
         ▼
         first wait() — resolves SessionMode (Fresh / Pinned / Resume)
         │              returns SessionStart { id, parent } ──▶ the bus
         ▼
         handle(SessionStart)
         ├─ new <id>.jsonl:
         │    open append, write session_start + buffered instructions
         └─ existing <id>.jsonl (resume):
              1. read & collect {"type":"message", ...} entries
              2. sanitize interrupted trailing state:
                 · trailing assistant tool_use without tool_result
                   → synthesize & persist error tool_result user message
                 · trailing bare user prompt without assistant reply
                   → synthesize & persist "(interrupted)" assistant message
              3. emit ReplayMessage { json } for each message (→ agent, repl)
              4. emit ReplayExchange { prompt, reply } for last turn (→ repl)
              5. emit Info("resumed <id> (<n> messages)")
              6. open <id>.jsonl in append mode, write session_resume

 /clear  ResetConversation in ──▶ write session_end, emit SessionEnd { old }
                                  open fresh <new>.jsonl, emit SessionStart { new }

 exit    Quit in ──▶ write session_end, emit SessionEnd { id }
```

## Events

**Listens:**

| event | reaction |
|---|---|
| `SessionStart` | open `<id>.jsonl` (or replay and append `session_resume` if `<id>.jsonl` already exists) |
| `ClearPromptSections` | clear buffered boot-time instructions |
| `SystemPromptSection` | write `{"type":"instructions",...}` (or buffer until `SessionStart` if emitted during `start`) |
| `ConversationMessage` | write `{"type":"message","role":...,"content":...}` |
| `ToolStart` / `ToolEnd` | write `{"type":"tool",...}` / `{"type":"tool_end",...}` |
| `Info` / `Error` | write `{"type":"info",...}` / `{"type":"error",...}` |
| `TurnCancelled` | write `{"type":"info","text":"turn cancelled by user"}` |
| `SessionEnd` | write `{"type":"session_end",...}` and close file |
| `ResetConversation` | close current session (`SessionEnd`) and open a fresh minted session (`SessionStart`) |
| `Quit` | close current session (`SessionEnd`) |

**Emits:**

| event | when |
|---|---|
| `SessionStart` | first `wait` after boot; again on every `/clear` |
| `SessionEnd` | immediately before each replacement `SessionStart`; on `Quit` |
| `ReplayMessage` | once per persisted message when resuming an existing session |
| `ReplayExchange` | once after `ReplayMessage`s when resuming, carrying the last user prompt and assistant text reply |
| `Info` | `"resumed <id> (<n> messages)"` on resume |
| `Error` | when the session store cannot be opened or a requested `--resume` target does not exist |
| `Quit` | returned from first `wait` if `--resume` fails to find a target session |

## Worked example

First run (one turn, then `/quit`):

```
 ~/.local/state/min/sessions/min/1729180001-k3f9.jsonl
 ─────────────────────────────────────────────────────────────────
 {"type":"session_start","id":"1729180001-k3f9","parent":null,"ts":1729180001,"cwd":"/home/me/min"}
 {"type":"instructions","title":"Project instructions (AGENTS.md)","body":"Keep the core minimal…"}
 {"type":"message","role":"user","content":"what does Cargo.toml say?"}
 {"type":"tool","name":"read","brief":"Cargo.toml"}
 {"type":"tool_end","ok":true,"secs":0.01,"note":"[package]"}
 {"type":"message","role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"read","input":{"path":"Cargo.toml"}}]}
 {"type":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"...","is_error":false}]}
 {"type":"message","role":"assistant","content":[{"type":"text","text":"It declares the workspace."}]}
 {"type":"session_end","ts":1729180014}
```

Listing and resuming:

```
 $ min --sessions
 1729180001-k3f9  1m ago   "what does Cargo.toml say?"

 $ min --resume
 ❯ what does Cargo.toml say?
 It declares the workspace.
 resumed 1729180001-k3f9 (4 messages)
 ❯ _
```

## What the tests pin

- First `wait` announces a fresh `<epoch>-<4-char>` session to all plugins and creates `<id>.jsonl`.
- Pinned ID (`SessionMode::Pinned`) and parent ID apply to the first session only.
- All event types serialize to their documented single-line JSONL records.
- Resuming a session replays `ReplayMessage`s and `ReplayExchange`, emits `Info("resumed …")`, and appends `session_resume` to the same `.jsonl` file.
- Resuming an interrupted session with a dangling `tool_use` synthesizes and persists an error `tool_result` user message.
- Resuming an interrupted session ending on a bare user prompt synthesizes and persists an `(interrupted)` assistant message.
- `Resume(None)` selects the newest root session and ignores subagent (`parent != null`) sessions; `format_sessions_in` lists root sessions newest-first with first prompt and subagent count.
- Resuming a missing session emits `Error` and returns `Quit`.
- `/clear` (`ResetConversation`) after a resume closes the resumed session with `session_end` and opens a fresh `<new_id>.jsonl`.
- Project slug normalization and base-36 entropy generation.

## See also

[min-agent](agent.md) (emits `ConversationMessage`, restores `messages` on `ReplayMessage`) ·
[min-repl](repl.md) (restores history on `ReplayMessage`, prints `ReplayExchange`) ·
[min-subagents](subagents.md) (spawns child processes with `--parent-session <id>`) ·
[min-herdr](herdr.md) (tracks `SessionStart` / `SessionEnd`) ·
[plugins.md](../plugins.md)
