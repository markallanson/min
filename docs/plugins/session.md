# min-session — session identity

A session is nothing but an id: minted at startup, announced to every
plugin, and replaced whenever the conversation is cleared. Sessions have an
optional parent — the session that, say, asked another to do some work —
supplied on the command line.

**The events *are* the interface.** There is deliberately no session
service; a plugin that wants to track the current session keeps the last
`id` it saw in its own state (min-session-log works exactly this way).

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-session` / `"session"` |
| shape | source (announces from `wait`) + listener (`ResetConversation`, `Quit`) |
| tools offered | none |
| services published / consumed | none |
| deps | `min-core` only |

## Ids

```
<epoch seconds>-<4 random alphanumeric characters>
      │                  │
      │                  └─ /dev/urandom → 4 bytes → base-36 alphabet
      │                     (no /dev: the clock's sub-second nanos — the
      │                      suffix only has to separate sessions born
      │                      together, and no two of those share nanos)
      └─ keeps ids sorting chronologically
```

The entropy suffix is the whole uniqueness story: sessions minted in the
same second cannot collide, so ids never need stepping past a taken name.
36⁴ ≈ 1.7M combinations per second — more than could ever be minted.

Command line (parsed in `src/main.rs`, passed to `SessionPlugin::new`):

| invocation | first session |
|---|---|
| `min` | minted |
| `min --session 1729180000` | the pinned id, verbatim, once |
| `min --parent-session 1729179990` | minted, carrying `parent = 1729179990` |
| both switches together | both dropped (the caller could not have meant both) |

The pin and the parent are one-shot: they apply to the startup session only.

## Lifecycle

```
 boot    start()  — silent. Anything emitted here would be missed by every
                    plugin loaded later: events are never queued or replayed.
         │
         ▼
         first wait() — every plugin has now started, so an event returned
         │              here reaches everyone, this plugin included.
         ▼
         SessionStart { id, parent } ──▶ the bus
         subsequent wait() calls: None (never spins the bus)

 /clear  ResetConversation in ──▶ SessionEnd { old id }   (close first…)
                                  SessionStart { new id } (…then open)

 exit    Quit in ──▶ SessionEnd { id }
```

Mint-before-close on `/clear` is a leftover from the stepped-id days; with
the suffix it no longer matters, only the order *end-then-start* does.

## Events

**Listens:**

| event | reaction |
|---|---|
| `ResetConversation` | close the open session, mint and announce a fresh one — both inside the one dispatch |
| `Quit` | close the open session (`SessionEnd`) |

**Emits:**

| event | when |
|---|---|
| `SessionStart` | first `wait` after boot; again on every `/clear` |
| `SessionEnd` | immediately before each replacement `SessionStart`; on `Quit` |

## Why the first announcement comes from `wait`

`hub.emit` is synchronous: a plugin executing `start`/`wait`/`handle` is
busy and misses what is emitted during its own execution, and anything
emitted in `start` also misses every plugin loaded *later* — their `start`
has not run, so nothing could have subscribed them anyway. The core only
calls `wait` after *every* plugin has started, so an event returned from the
first `wait` is the first event that can reach the entire system. (A
`start`-time emission does reach plugins loaded *after* the emitter — that
is min-context's trick, pinned by a core test — but it can never reach
plugins loaded *before*, and the session announcement must reach everyone.)

## Worked example

```
 $ min --parent-session 1729179990
   … every plugin started, bus idle …
   core: session.wait() → SessionStart { id: "1729180001-k3f9",
                                         parent: "1729179990" }
   core: dispatch → session-log opens 1729180001-k3f9.log (parent in header)
                 → herdr re-reports idle with --agent-session-id 1729180001-k3f9, …

 $ /clear
   ResetConversation dispatched in load order:
     session (1st)  → SessionEnd {1729180001-k3f9}, SessionStart {1729180002-x8q2}
     context        → ClearPromptSections + fresh AGENTS.md sections
     agent (last)   → messages cleared
```
## What the tests pin

- The startup path, core-style: silent through `start`; the first `wait`
  announces to everyone (recorder included); later `wait`s return `None`.
- A pinned id is used verbatim for the first session only; later ids are
  `<epoch>-<suffix>`.
- Both switches together is an error; the parent is dropped.
- Ids carry four characters of base-36 entropy and never repeat the
  previous id, even within the same second.
- The parent is taken by the first session only.
- `/clear` closes then opens; `Quit` closes.
- Every byte value maps into the 36-character alphabet (`entropy_from`).

## See also

[min-session-log](session-log.md) (the model consumer of these events) ·
[min-repl](repl.md) (`/clear`, `Quit`) · [plugins.md](../plugins.md)
