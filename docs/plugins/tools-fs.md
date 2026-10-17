# min-tools-fs — the file tools: `read`, `edit`, `write`

Registers three tools the model calls to see and change files. `edit` is
the workhorse for self-modification: exact-match replacement where each
`old_text` must match **exactly one** place. That constraint keeps the
model honest (it must read first) and makes edits atomic.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-tools-fs` / `"fs"` |
| shape | **capability** — three `Tool` impls, registered in `start` |
| tools offered | `read` · `edit` · `write` |
| services published / consumed | none |
| deps | `min-core`, `serde_json` |

## The tools

```
 model ──▶ tools().run("read"|"edit"|"write", input)
             │
   read  ──▶ parse {path, offset?, limit?} ──▶ numbered lines (cat -n style)
                                               + "more below" nudge
   edit  ──▶ read file (UTF-8 only)
             └▶ for each edit in order: validate old_text (== new? 0 hits?
                n hits? → Err, whole call aborts, file untouched)
                all pass ──▶ write file ──▶ "applied n edit(s)"
   write ──▶ mkdir -p parent ──▶ replace whole file ──▶ "wrote n bytes"
```

### `read`

`{"path", "offset"?, "limit"?}` → numbered lines, `cat -n` style
(`%5d| text`). Defaults: `offset 1`, `limit 2000` (clamped 1–5000). Past
the end → a friendly count instead of an error; stopping early → a
`…more lines below — call again with offset=N` nudge. Missing file → `Err`.
Non-UTF-8 bytes are lossily converted, so *reading* never fails on encoding.

### `edit`

`{"path", "edits": [{"old_text", "new_text"}, …]}` → apply in order,
validate-then-write:

```
 read file (must be UTF-8)
 for each edit, in order (later edits see earlier results):
     old == new            → Err (pointless edit)
     0 matches             → Err "not found — re-read and copy exactly"
     1 match               → replace first occurrence
     n matches             → Err "matches n places — include more context"
 ONLY NOW write the file        ← if any edit fails, nothing changes
```

The file is only written after every edit has validated against the
*evolving* content, so a rejected edit leaves the file byte-identical.

### `write`

`{"path", "content"}` → create parent directories, then replace the whole
file. Returns the byte count. For targeted changes, `edit` is preferred.

## Events

**Listens / emits:** nothing. A pure capability: the plugin touches the bus
only once, registering its tools in `start`; from then on it speaks only
when the model calls a tool through the registry, and `Err` results become
tool errors for the model (is_error tool_results, usually provoking a
retry).

## Worked example

A surgical rename, as the model would do it:

```json
{"path": "src/lib.rs",
 "edits": [{"old_text": "fn parse_args() -> Result<(Option<String>, Option<String>), String> {",
            "new_text": "fn parse_args() -> Result<Args, String> {"}]}
```

```
 the file contains the old_text exactly once
   → replaced, file written
   → Ok("src/lib.rs: applied 1 edit(s)")
   → ToolEnd { ok: true, … } on the bus → repl prints "✓ 0.0s"
```

And the two rejections the design exists to produce:

```
 old_text "x" in "x x x\n"  → Err("…: old_text matches 3 places — include
                                   more context to make it unique")
 old_text "nope"            → Err("…: old_text not found — re-read the
                                   file and copy the text exactly")
 …in both cases the file on disk is untouched.
```

## What the tests pin

- A unique replacement applies and writes the file.
- Ambiguous (3 matches) and missing `old_text` are rejected **without
  modifying the file**.
- `read` numbers lines and appends the truncation nudge.

## See also

[min-agent](agent.md) (dispatches model calls here) ·
[min-tools-shell](tools-shell.md) (verification: build and test after
editing) · [plugins.md](../plugins.md)
