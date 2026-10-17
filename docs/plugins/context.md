# min-context — project context for the system prompt

Finds instruction files on disk and hands them to the agent as system-prompt
sections. It looks for a systemwide file first — `~/.config/min/AGENTS.md` —
then walks the directory tree under the current directory for project files
(currently `AGENTS.md`), root-most file first so a subdirectory's file
arrives after (and can refine) its ancestors'. No files → no sections → the
agent is unaffected.

The round runs at startup and again whenever the conversation is cleared.

```
 boot ──▶ start():
            emit ClearPromptSections {}
            emit SystemPromptSection { "~/.config/min/AGENTS.md", … }   (if present)
            emit SystemPromptSection { "AGENTS.md", … }                 (walk, ancestors first)
            emit SystemPromptSection { "crates/foo/AGENTS.md", … }
            …

 /clear ─▶ ResetConversation dispatched:
            │
            ├─ session  (loads first)  → SessionEnd + SessionStart
            ├─ …
            ├─ context  (9th)          → the same round as boot, re-walked
            │                             from disk *now*
            └─ agent    (loads last)   → clears history; sections it just
                                         collected are the fresh ones
```

The same dispatch both refreshes instructions and resets the conversation —
so `/clear` picks up on-disk edits to AGENTS.md for free.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-context` / `"context"` |
| shape | source at `start`, listener for `ResetConversation` |
| tools offered | none |
| services published / consumed | none |
| deps | `min-core` only |

## How the walk works

`collect(".")` does a depth-first walk that reads each directory's entries
sorted by name, takes this directory's own `AGENTS.md` **before** descending
(ancestors always precede descendants), and skips:

- hidden directories (`.`-prefixed),
- `node_modules` and `target` (`SKIP_DIRS`),
- symlinks are never followed, so there are no cycles.

Files that are not valid UTF-8 are skipped silently. Display paths are
relative (`AGENTS.md`, `crates/min-agent/AGENTS.md`); the systemwide file is
shown as `~/.config/min/AGENTS.md`.

Section bodies become `# <title>\n\n<body>` in the agent's system prompt,
with titles like `Project instructions (AGENTS.md)`.

## Events

**Listens:**

| event | reaction |
|---|---|
| `ResetConversation` | run the full round again: `ClearPromptSections`, then every file as it exists on disk *right now* |

**Emits:**

| event | when |
|---|---|
| `ClearPromptSections` | first in every round — the agent drops its previous copy so nothing stale survives an edit or deletion |
| `SystemPromptSection` | once per instruction file found, in the order described above |

## Why emitting from `start` works here

The general rule is *emit from `wait`, not `start`*, because events emitted
during a plugin's execution are missed by that plugin and anything executing
concurrently. But `start` hooks run strictly one at a time, in load order,
and a `start`-time emission reaches **every other plugin — including ones
whose `start` has not run yet** (only the emitter is busy). Context loads
just before the agent, so its boot-time sections land in the agent either
way. This is pinned by a min-core test
(`emissions_from_start_reach_plugins_not_yet_started`). What `start`
emissions *cannot* do is reach plugins loaded **earlier** that have already
finished starting and are now idle — `wait`-time emissions reach everyone,
which is min-session's reason for announcing from `wait` instead.

## Worked example

Tree:

```
proj/AGENTS.md                    "root rules"
proj/crates/web/AGENTS.md         "web rules"
proj/crates/web/node_modules/x/…  (skipped)
proj/.hidden/AGENTS.md            (skipped)
~/.config/min/AGENTS.md           "systemwide rules"
```

Round → the agent's system prompt gains, in order:

```
# Project instructions (~/.config/min/AGENTS.md)
systemwide rules

# Project instructions (AGENTS.md)
root rules

# Project instructions (crates/web/AGENTS.md)
web rules
```

## What the tests pin

- Walk order (ancestors first) and the skip rules (hidden, `node_modules`,
  non-UTF-8, empty trees).
- The systemwide file is found exactly once and precedes the tree, even
  when `HOME` points at the searched directory (hidden dirs are skipped).
- `ResetConversation` → clear **then** re-emit current contents, proven
  with a recorder plugin wired exactly as the core dispatches.

## See also

[min-agent](agent.md) (collects these sections) ·
[min-session](session.md) (why `/clear` re-rolls the session in the same
dispatch) · [plugins.md](../plugins.md)
