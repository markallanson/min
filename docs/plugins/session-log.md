# min-session-log — an append-only log of session activity

One file per session — `<session id>.log` — under the project's directory in
the XDG state tree:

```
$XDG_STATE_HOME/min/sessions/<project>/<id>.log
~/.local/state/min/sessions/<project>/<id>.log    (XDG_STATE_HOME unset/empty)
```

`<project>` is the slug of the basename of the session's working directory
(`min`, `min-k3f9` for a second `…/herb/min`, `root` when there is no
basename). Created the moment a session is announced; appended to as the
session unfolds; **never rewritten** — a later release can resume a session
by reading it back.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-session-log` / `"session-log"` |
| shape | **pure listener** — introduces no events, offers no tools, publishes no services |
| tools offered | none |
| services published / consumed | none |
| deps | `min-core`, `serde_json` (for the JSON escaping only) |

## Events

**Listens:** `SessionStart`, `UserInput`, `AssistantDelta`, `MessageEnd`,
`ToolStart`, `ToolEnd`, `Info`, `Error`, `SystemPromptSection`,
`SessionEnd` — the deliberately exhaustive mapping below. Ignores
`ResetConversation` (its effects arrive as session events) and `Quit`.

**Emits:** `Error`, once, when logging is disabled by a setup failure.
Otherwise silent — it is a recorder, not a participant.

Every fact it records is already broadcast by another plugin; the mapping,
kept deliberately exhaustive (anything that helps a future resume), mirrors
the repl's own display:

| event | log entry |
|---|---|
| `SessionStart` | header — `session_start: id=<id> parent=<p\|-> started=<secs> cwd=<cwd>` |
| `UserInput` | `prompt: "<json-escaped>"` |
| `AssistantDelta` | buffered; flushed as one `assistant: "…"` line on `MessageEnd` |
| `ToolStart` | `tool: <name> "<brief>"` |
| `ToolEnd` | `tool_end: <ok\|fail> <secs>s "<note>"` |
| `Info` | `info: "…"` |
| `Error` | `error: "…"` |
| `SystemPromptSection` | `instructions: "<title>" "<body>"` |
| `SessionEnd` | footer — `session_end: <secs>`, then the file handle is dropped |

Prompt, assistant and tool text are JSON-encoded on the way in, so the log
stays **one entry per line** no matter what newlines run through it.

## The pipeline

```
 events ──▶ handle() ──┬─ SessionStart → create dir, open <id>.log (append), write header
                       ├─ deltas → buffer in memory ─┐
                       ├─ everything else ───────────┼─▶ writeln! to the open file
                       └─ MessageEnd → take() buffer ─┘   (silent on I/O failure)
```

## Robustness

Logging must never disturb a session:

- **Setup failure** (no state dir, cannot create the project directory,
  cannot open the file) → degrade to a no-op and announce it once via
  `Error` (`session-log disabled: …`). The repl owns the terminal — this
  plugin announces, it does not print.
- **Every `SessionStart` retries the setup**, so a full disk that frees up
  mid-run is picked up at the next `/clear`.
- **Per-line I/O failures are silent** — a broken log must not break the
  session it is recording.
- **A hard kill that skips `SessionEnd` loses nothing of the next session:**
  the new id alone opens the new file; nothing of the old one can be
  clobbered.

## The project slug

`slug_project`: basename of the cwd → lowercased → every character outside
`[a-z0-9._-]` collapsed to `-` → capped at 32 → `-` trimmed; `/` (no
basename) and all-dashes become `root`. `/home/me/herb/min` and
`/srv/work/min` both log under `min/`; the header's `cwd=` tells them apart.

## Worked example

One turn (`read Cargo.toml`, then a short answer), then `/quit`:

```
 ~/.local/state/min/sessions/min/1729180001-k3f9.log
 ─────────────────────────────────────────────────────────────────
 session_start: id=1729180001-k3f9 parent=- started=1729180001 cwd="/home/me/herb/min"
 instructions: "Project instructions (AGENTS.md)" "Keep the core minimal…"
 prompt: "what does the Cargo.toml say?"
 tool: read "Cargo.toml"
 tool_end: ok 0.0s "[package]"
 assistant: "It declares the workspace: five crates…"
 session_end: 1729180014
```

Seven events in, seven lines out, one line each — whatever the messages
contained.

## What the tests pin

- One file per session, inside the project directory (not beside it).
- The header line's exact shape (timestamps and cwd excepted).
- The full event→line mapping, exact, in documented order: header +
  `instructions` + `prompt` + `info` + `tool` + `tool_end` + `error` +
  footer.
- Deltas buffer mid-stream (nothing logged) and flush exactly on
  `MessageEnd`.
- Fresh sessions get fresh files, with and without a preceding `SessionEnd`
  (hard-kill path).
- `state_dir()`: `XDG_STATE_HOME` wins; an *empty* value means unset (XDG
  rule); otherwise `~/.local/state`.
- The slug table, including unicode collapse, the 32 cap and `root` cases.
- JSON escaping keeps newlines inside one line.

## See also

[min-session](session.md) (where `SessionStart`/`SessionEnd` come from) ·
[min-repl](repl.md) (the display this mirrors) · [plugins.md](../plugins.md)
