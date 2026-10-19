# min-repl — the terminal UI

Owns stdin/stdout: the `❯` prompt, line editing (cursor movement,
backspace/delete, ↑/↓ history, ctrl-a/e/k/u/w), slash commands, and the
display of every event the rest of min emits — streaming assistant text,
tool activity, info and errors. The repl is why plugins never print: they
emit `Info`/`Error`/streaming events, and the repl renders them.

When stdin is not a tty (piped input), raw mode is skipped and lines are
read as-is, so min can be scripted: `echo 'hello' | min`.

## At a glance

| | |
|---|---|
| crate / plugin name | `crates/min-repl` / `"repl"` |
| shape | **source** — the heartbeat: blocks in `wait` on the keyboard |
| tools offered | none |
| services published / consumed | none |
| deps | `min-core`, `libc`, `serde_json` |

## The two responsibilities

```
        ┌───────────────────────── the loop, from the repl's side ─────────────┐
        │                                                                      │
 IDLE   │  wait(): blocked on stdin (raw-mode byte reads, or cooked lines)     │
        │     │  a full line arrives                                           │
        │     ▼                                                                │
        │  ┌─ /help  → print help (no event)                                   │
        │  ├─ /clear → return ResetConversation {} ───────────────▶ the bus    │
        │  ├─ /quit  → return Quit {} ───────────────────────────▶ the bus    │
        │  ├─ /other → "unknown command" (no event)                          │
        │  └─ text   → install SIGINT-exit handler                           │
        │              return UserInput { text } ─────────────────▶ the bus   │
        │                                                                      │
 BUSY   │  handle(): render every event someone else emitted (table below)     │
        └──────────────────────────────────────────────────────────────────────┘
```

Returned events are dispatched *after* `wait` returns, so the repl also
sees its own `Quit` — which is the hook that ends the process:

```rust
fn handle(…) {
    if event.downcast_ref::<events::Quit>().is_some() { hub.quit(); }
    …
}
```

## Events

**Listens:**

| event | rendering |
|---|---|
| `LlmStart` | draw transient `  ⠋ thinking…` on the current line (tty only) |
| `Thinking` | advance the transient spinner (`  ⠙ thinking…`, or `  ⠙ calling <tool>…` while tool-call JSON streams) |
| `AssistantDelta` | clear any transient spinner (`\r\x1b[K`), then write the chunk verbatim (streaming output) |
| `MessageEnd` | clear any transient spinner; newline if the stream didn't end at one; re-arm the quit-style SIGINT handler |
| `ToolStart` | `  ▸ <name> <dim brief>` |
| `ToolEnd` | `  ✓ <secs>` on success; `  ✗ <secs> <note>` plus dimmed `preview` lines on failure |
| `ReplayMessage` | extract `role == "user"` text messages into `history` so `↑/↓` recall works across resumes |
| `ReplayExchange` | print `❯ <prompt>` and `<reply>` for the last turn of a resumed session |
| `Info` | dim line |
| `Error` | `⚠ <text>` in red |
| `Quit` | clear any transient spinner; `hub.quit()` — stops the loop after this event finishes propagating |

**Emits:**

| event | when |
|---|---|
| `UserInput` | a non-command line was submitted (also swaps in the SIGINT-exit handler for the turn's duration) |
| `ResetConversation` | `/clear` |
| `Quit` | `/quit`, ctrl-d at an empty prompt, or EOF/SIGINT while blocked on stdin |

## Line editing

Raw mode is cbreak-ish: line buffering and echo off, signal generation kept
(ctrl-c still raises SIGINT even mid-LLM-call). The edit buffer is raw bytes
that are always valid UTF-8; the cursor is a byte offset that is always a
character boundary. Inserts and deletes step over continuation bytes
(`0b10xxxxxx`) instead of tearing multi-byte characters, and repaint column
math counts *characters*, not bytes.

Recognized keys: backspace, delete, ←/→, Home/End (and CSI `1~`/`4~`/`7~`/`8~`),
↑/↓ history (consecutive duplicates recorded once), ctrl-a/e (home/end),
ctrl-k (kill to end), ctrl-u (clear line), ctrl-w (delete word before
cursor), ctrl-d (delete forward; quit only on an empty line).

## Interrupts: two SIGINT handlers

```
 at the prompt            install_sigint_restore()  ─┐
   ctrl-c: handler restores termios, sets a flag,   │ interrupted read fails
           returns → blocked read wakes (EINTR)     │ with EINTR (no SA_RESTART)
           → read_byte → None → EOF path → Quit ────┘   (plugins see the exit)
                              → repl.handle(Quit) → hub.quit()

 mid-turn (a turn started) install_sigint_exit_handler()
   ctrl-c: an interrupt cannot unwind through the blocking LLM call, so the
   handler restores termios and hard-exits with the conventional 130.
   MessageEnd swaps the quit-style handler back in.
```

This is why ctrl-c at the prompt goes through the bus (herdr gets to
release, the session gets to close) and ctrl-c mid-turn does not.

## Worked example

`echo 'hi' | min` (non-tty) and a tty session side by side:

```
 piped                                   tty
 ────                                    ─────────────────────────────────
 wait: read_line (cooked)                wait: prompt "❯ ", raw byte loop
 "hi\n" → not a command                  user types hi ⏎
 → UserInput("hi")                       → history += "hi" → UserInput("hi")
 handle: deltas stream out               handle: deltas stream out as typed
 … until Quit (EOF on stdin)             … /quit ⏎ → Quit → hub.quit()
```

## What the tests pin

- `waiting_indicator_formats_and_cycles_frames` and `waiting_state_tracks_llm_start_thinking_and_clear` (spinner frames, `thinking…` vs `calling <tool>…`, and state reset on `MessageEnd`).
- `replay_message_populates_history_ignoring_tool_results_and_duplicates`.
- `word_start` (ctrl-w) basics, trailing-whitespace eating, boundaries.
- `cursor_col` is 1-based and character-counted across multi-byte input.
- The SIGINT handler swap: mid-turn installs the exit handler, `MessageEnd`
  restores the quit handler (pinning both directions without actually
  raising SIGINT in tests).

## See also

[min-core](../architecture.md) (the loop this plugin feeds) ·
[min-herdr](herdr.md) and [min-session](session.md) (what `Quit` lets them
do) · [plugins.md](../plugins.md)
