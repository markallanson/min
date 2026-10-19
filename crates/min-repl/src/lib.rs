//! min-repl — the terminal UI.
//!
//! Owns stdin/stdout: prompts, line editing (cursor movement, backspace,
//! delete, ↑/↓ history), slash commands, and display of every event the rest
//! of min emits (streaming assistant text, tool activity, info/errors).
//!
//! When stdin is not a tty (piped input) raw mode is skipped and lines are
//! read as-is, so min can be scripted: `echo 'hello' | min`.
//!
//! Ctrl-C exits at any time: at the prompt it injects a `Quit` event like
//! any other exit (ctrl-d, /quit), so plugins see it; mid-turn, where an
//! interrupt cannot unwind through the blocking LLM call, a SIGINT handler
//! restores the terminal and exits immediately. Ctrl-D quits at an empty
//! prompt.

use min_core::{events, Hub, Plugin};
use std::any::Any;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

mod ansi;

pub struct Repl {
    tty: bool,
    history: Vec<String>,
    hist_idx: usize,
    buf: Vec<u8>,  // input so far; always valid UTF-8
    pos: usize,    // edit cursor: byte offset into `buf`, always a char boundary
    at_line_start: bool,
    waiting: bool, // transient LLM working indicator is currently on screen
    spin_idx: usize,
    raw: Option<RawMode>,
}

impl Repl {
    pub fn new() -> Self {
        Repl {
            tty: unsafe { libc::isatty(0) == 1 },
            history: Vec::new(),
            hist_idx: 0,
            buf: Vec::new(),
            pos: 0,
            at_line_start: true,
            waiting: false,
            spin_idx: 0,
            raw: None,
        }
    }
}

impl Default for Repl {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for Repl {
    fn name(&self) -> &'static str {
        "repl"
    }

    fn start(&mut self, _hub: &Hub) {
        install_sigint_restore();
        if self.tty {
            match RawMode::enable() {
                Some(raw) => self.raw = Some(raw),
                None => self.tty = false,
            }
        }
    }

    fn wait(&mut self, _hub: &Hub) -> Option<Box<dyn Any>> {
        let Some(line) = self.read_line() else {
            // EOF — ctrl-d at the prompt, or SIGINT (see `on_sigint_quit`,
            // which interrupts the blocked read and lets this path run).
            return Some(Box::new(events::Quit {}));
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            return None;
        }
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        self.hist_idx = self.history.len();
        match line.as_str() {
            "/help" => {
                self.put_line(&ansi::dim(
                    "/help · /clear · /quit — ctrl-d quits, ↑/↓ history, ←/→ edit",
                ));
                None
            }
            "/clear" => Some(Box::new(events::ResetConversation {})),
            "/quit" => Some(Box::new(events::Quit {})),
            s if s.starts_with('/') => {
                self.put_line(&format!("unknown command {s} — try /help"));
                None
            }
            s => {
                // A turn is starting: from here an interrupt cannot unwind
                // through the blocking LLM call, so ctrl-c restores the
                // terminal and hard-exits. `handle` re-arms the quitting
                // handler when the turn ends.
                install_sigint_exit_handler();
                Some(Box::new(events::UserInput { text: s.to_string() }))
            }
        }
    }

    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if event.downcast_ref::<events::Quit>().is_some() {
            self.clear_waiting();
            hub.quit();
            return;
        }
        if event.downcast_ref::<events::LlmStart>().is_some() {
            self.show_waiting("");
        } else if let Some(e) = event.downcast_ref::<events::Thinking>() {
            self.show_waiting(&e.tool);
        } else if let Some(e) = event.downcast_ref::<events::AssistantDelta>() {
            self.clear_waiting();
            self.write(&e.text);
            self.at_line_start = e.text.ends_with('\n');
        } else if event.downcast_ref::<events::MessageEnd>().is_some() {
            self.clear_waiting();
            if !self.at_line_start {
                self.write("\n");
            }
            self.at_line_start = true;
            // Back at (or heading for) the prompt: ctrl-c may quit through
            // the event bus again rather than kill the process.
            install_sigint_restore();
        } else if let Some(e) = event.downcast_ref::<events::ToolStart>() {
            self.put_line(&format!(
                "  {} {} {}",
                ansi::cyan("▸"),
                e.name,
                ansi::dim(&e.brief)
            ));
        } else if let Some(e) = event.downcast_ref::<events::ToolEnd>() {
            let secs = format!("{:.1}s", e.secs);
            if e.ok {
                self.put_line(&format!("  {} {}", ansi::green("✓"), ansi::dim(&secs)));
            } else {
                self.put_line(&format!(
                    "  {} {} {}",
                    ansi::red("✗"),
                    ansi::dim(&secs),
                    ansi::dim(&e.note)
                ));
            }
            for line in e.preview.lines() {
                self.put_line(&format!("  {}", ansi::dim(line)));
            }
        } else if let Some(e) = event.downcast_ref::<events::ReplayMessage>() {
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&e.json) {
                if msg["role"] == "user" {
                    if let Some(s) = msg["content"].as_str() {
                        let s = s.trim();
                        if !s.is_empty() && self.history.last().map(|h| h.as_str()) != Some(s) {
                            self.history.push(s.to_string());
                            self.hist_idx = self.history.len();
                        }
                    }
                }
            }
        } else if let Some(e) = event.downcast_ref::<events::ReplayExchange>() {
            self.put_line(&format!("{} {}", ansi::cyan("❯"), e.prompt));
            if !e.reply.is_empty() {
                self.put_line(&e.reply);
            }
        } else if let Some(e) = event.downcast_ref::<events::Info>() {
            self.put_line(&ansi::dim(&e.text));
        } else if let Some(e) = event.downcast_ref::<events::Error>() {
            self.put_line(&format!("{} {}", ansi::red("⚠"), e.text));
        }
    }
}

impl Repl {
    fn write(&self, s: &str) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }

    fn put_line(&mut self, s: &str) {
        self.clear_waiting();
        if !self.at_line_start {
            self.write("\n");
        }
        self.write(s);
        self.write("\n");
        self.at_line_start = true;
    }

    /// Draw or advance the transient LLM progress indicator on the current
    /// line (tty only; piped output stays clean). Cleared in-place with
    /// `\r\x1b[K` before any real output is written.
    fn show_waiting(&mut self, tool: &str) {
        let line = format_waiting(self.spin_idx, tool);
        self.spin_idx = self.spin_idx.wrapping_add(1);
        self.waiting = true;
        if !self.tty {
            return;
        }
        if !self.at_line_start {
            self.write("\n");
            self.at_line_start = true;
        }
        self.write(&format!("\r\x1b[K{line}"));
    }

    fn clear_waiting(&mut self) {
        if !self.waiting {
            return;
        }
        self.waiting = false;
        if self.tty {
            self.write("\r\x1b[K");
        }
    }

    fn prompt(&self) {
        if self.tty {
            self.write(&format!("{} ", ansi::cyan("❯")));
        }
    }

    fn read_line(&mut self) -> Option<String> {
        if !self.tty {
            let mut s = String::new();
            return match std::io::stdin().read_line(&mut s) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(s),
            };
        }
        self.prompt();
        loop {
            let b = read_byte()?;
            match b {
                b'\r' | b'\n' => {
                    self.write("\n");
                    self.at_line_start = true;
                    let s = String::from_utf8_lossy(&self.buf).into_owned();
                    self.buf.clear();
                    self.pos = 0;
                    return Some(s);
                }
                0x7f | 0x08 => self.backspace(),
                0x04 => {
                    // ctrl-d quits only on an empty line; otherwise it deletes
                    // forward like a normal shell.
                    if self.buf.is_empty() {
                        self.write("\n");
                        return None;
                    }
                    self.delete_forward();
                }
                0x15 => {
                    // ctrl-u: clear the line
                    self.pos = 0;
                    self.buf.clear();
                    self.repaint();
                }
                0x01 => {
                    // ctrl-a: start of line
                    self.pos = 0;
                    self.repaint();
                }
                0x05 => {
                    // ctrl-e: end of line
                    self.pos = self.buf.len();
                    self.repaint();
                }
                0x17 => {
                    // ctrl-w: delete the word before the cursor
                    let before = word_start(&self.buf, self.pos);
                    self.buf.drain(before..self.pos);
                    self.pos = before;
                    self.repaint();
                }
                0x0b => {
                    // ctrl-k: kill from cursor to end of line
                    self.buf.truncate(self.pos);
                    self.repaint();
                }
                0x1b => {
                    self.escape()?;
                }
                0x00..=0x1f => {}
                _ => {
                    if self.pos == self.buf.len() {
                        // Fast path at end-of-line: append and just echo.
                        self.buf.push(b);
                        self.pos = self.buf.len();
                        let mut out = std::io::stdout().lock();
                        let _ = out.write_all(&[b]);
                        let _ = out.flush();
                    } else {
                        // Inserting before a continuation byte would tear a
                        // multi-byte character; snap to the char it belongs to.
                        while self.pos < self.buf.len() && self.buf[self.pos] & 0xC0 == 0x80 {
                            self.pos += 1;
                        }
                        self.buf.insert(self.pos, b);
                        self.pos += 1;
                        self.repaint();
                    }
                }
            }
        }
    }

    /// Remove the character before the cursor (backspace).
    fn backspace(&mut self) {
        if self.pos == 0 {
            return;
        }
        // Step back to the start of the preceding UTF-8 character.
        let mut start = self.pos - 1;
        while start > 0 && self.buf[start] & 0xC0 == 0x80 {
            start -= 1;
        }
        let at_end = self.pos == self.buf.len();
        self.buf.drain(start..self.pos);
        self.pos = start;
        if at_end {
            self.write("\u{8} \u{8}");
        } else {
            self.repaint();
        }
    }

    /// Remove the character at the cursor (delete key).
    fn delete_forward(&mut self) {
        if self.pos >= self.buf.len() {
            return;
        }
        let mut end = self.pos + 1;
        while end < self.buf.len() && self.buf[end] & 0xC0 == 0x80 {
            end += 1;
        }
        self.buf.drain(self.pos..end);
        self.repaint();
    }

    /// Redraw the input line and place the terminal cursor at the edit
    /// position. Column math uses displayed characters, not bytes, and the
    /// prompt is a fixed two columns ("❯ ").
    fn repaint(&mut self) {
        self.write("\r\x1b[K");
        self.prompt();
        self.write(&String::from_utf8_lossy(&self.buf));
        self.write(&format!("\r\x1b[{}G", self.cursor_col(self.pos)));
    }

    /// Terminal column (1-based — `CSI G` counts from 1) of the edit cursor
    /// after `pos` bytes of `buf`: prompt columns plus displayed characters,
    /// plus one to move *past* the last character.
    fn cursor_col(&self, pos: usize) -> usize {
        PROMPT_COLS + self.char_count_before(pos) + 1
    }

    /// Displayed characters (not bytes) in `buf[..pos]`.
    fn char_count_before(&self, pos: usize) -> usize {
        String::from_utf8_lossy(&self.buf[..pos]).chars().count()
    }

    /// After ESC: CSI cursor movement, Home/End/Delete, history; swallow the rest.
    fn escape(&mut self) -> Option<()> {
        let b1 = read_byte()?;
        if b1 != b'[' {
            return Some(());
        }
        // Accumulate parameter bytes (and intermediates), then the final byte.
        let mut params = Vec::new();
        let final_byte;
        loop {
            let b = read_byte()?;
            if (0x20..=0x3f).contains(&b) {
                params.push(b);
            } else {
                final_byte = b;
                break;
            }
        }
        // Numeric parameters, e.g. "3" in `ESC [ 3 ~` (Delete).
        let nums: Vec<u32> = params
            .iter()
            .filter(|c| c.is_ascii_digit())
            .fold((Vec::new(), false), |(mut acc, in_num), &c| {
                if let (Some(last), true) = (acc.last_mut(), in_num) {
                    if *last < 10_000 {
                        *last = *last * 10 + (c - b'0') as u32;
                    }
                } else {
                    acc.push((c - b'0') as u32);
                }
                (acc, true)
            })
            .0;
        match final_byte {
            b'A' => self.recall(-1),
            b'B' => self.recall(1),
            b'C' => self.right(),
            b'D' => self.left(),
            b'H' => {
                self.pos = 0;
                self.repaint();
            }
            b'F' => {
                self.pos = self.buf.len();
                self.repaint();
            }
            b'~' if !params.contains(&b'?') => match nums.first() {
                Some(1 | 7) => {
                    self.pos = 0;
                    self.repaint();
                }
                Some(3) => self.delete_forward(),
                Some(4 | 8) => {
                    self.pos = self.buf.len();
                    self.repaint();
                }
                _ => {}
            },
            _ => {}
        }
        Some(())
    }

    /// Move the cursor one character left (`←`).
    fn left(&mut self) {
        if self.pos == 0 {
            return;
        }
        let mut p = self.pos - 1;
        while p > 0 && self.buf[p] & 0xC0 == 0x80 {
            p -= 1;
        }
        self.pos = p;
        self.write("\x1b[1D");
    }

    /// Move the cursor one character right (`→`).
    fn right(&mut self) {
        if self.pos >= self.buf.len() {
            return;
        }
        let mut p = self.pos + 1;
        while p < self.buf.len() && self.buf[p] & 0xC0 == 0x80 {
            p += 1;
        }
        self.pos = p;
        self.write("\x1b[1C");
    }

    fn recall(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len() as i32;
        self.hist_idx = (self.hist_idx as i32 + dir).clamp(0, n) as usize;
        let text = if self.hist_idx == self.history.len() {
            String::new()
        } else {
            self.history[self.hist_idx].clone()
        };
        self.buf = text.into_bytes();
        self.pos = self.buf.len();
        self.repaint();
    }
}

fn read_byte() -> Option<u8> {
    // Set by the SIGINT handler at the prompt; the same interrupt also
    // breaks the read below with EINTR, so this only matters for reads that
    // raced the flag.
    if INTERRUPTED.load(Ordering::SeqCst) {
        return None;
    }
    let mut b = [0u8];
    match std::io::stdin().read(&mut b) {
        Ok(1) => Some(b[0]),
        _ => None,
    }
}

/// Byte index of the start of the word before `pos` (for ctrl-w).
/// A word is a run of non-whitespace; trailing whitespace before the cursor
/// is deleted too, like every other shell.
fn word_start(buf: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i > 0 && buf[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    while i > 0 && !buf[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

/// Width in columns of the "❯ " prompt (glyph + trailing space).
const PROMPT_COLS: usize = 2;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One frame of the transient working indicator (`  ⠋ thinking…` or
/// `  ⠙ calling edit…`).
fn format_waiting(spin_idx: usize, tool: &str) -> String {
    let frame = SPINNER[spin_idx % SPINNER.len()];
    let label = if tool.is_empty() {
        "thinking…".to_string()
    } else {
        format!("calling {tool}…")
    };
    format!("  {} {}", ansi::cyan(frame), ansi::dim(&label))
}

#[cfg(test)]
mod tests {
    use super::*;

    // No stdin/stdout here: these exercise the pure editing helpers.

    #[test]
    fn waiting_indicator_formats_and_cycles_frames() {
        let f0 = format_waiting(0, "");
        let f1 = format_waiting(1, "edit");
        assert!(f0.contains("⠋") && f0.contains("thinking…"), "{f0}");
        assert!(f1.contains("⠙") && f1.contains("calling edit…"), "{f1}");
        assert_eq!(format_waiting(SPINNER.len(), ""), f0);
    }

    #[test]
    fn waiting_state_tracks_llm_start_thinking_and_clear() {
        let mut r = Repl::new();
        r.tty = false;
        let hub = Hub::new(vec![]);

        assert!(!r.waiting);
        r.handle(
            &hub,
            &events::LlmStart {
                round: 1,
                model: "m".into(),
                messages: 1,
                tools: 0,
            },
        );
        assert!(r.waiting);
        assert_eq!(r.spin_idx, 1);

        r.handle(
            &hub,
            &events::Thinking {
                text: "hmm".into(),
                tool: String::new(),
            },
        );
        assert!(r.waiting);
        assert_eq!(r.spin_idx, 2);

        r.handle(&hub, &events::MessageEnd {});
        assert!(!r.waiting);
    }

    #[test]
    fn word_start_basic() {
        let b = b"hello world";
        assert_eq!(word_start(b, b.len()), 6); // start of "world"
        assert_eq!(word_start(b, 5), 0); // start of "hello"
    }

    #[test]
    fn word_start_eats_trailing_whitespace() {
        let b = b"foo   ";
        assert_eq!(word_start(b, b.len()), 0);
    }

    #[test]
    fn word_start_boundaries() {
        assert_eq!(word_start(b"", 0), 0);
        assert_eq!(word_start(b"abc", 0), 0);
        assert_eq!(word_start(b"a b", 2), 0); // "a " + trailing space deleted
    }

    #[test]
    fn cursor_col_is_one_based_and_char_counted() {
        let mut r = Repl::new();
        r.buf = b"abc".to_vec();
        assert_eq!(r.cursor_col(3), 6); // "❯ abc" — one past 'c'
        r.buf = "hé".to_string().into_bytes(); // é is 2 bytes, 1 char
        assert_eq!(r.cursor_col(3), 5); // "❯ hé" — one past 'é'
        assert_eq!(r.cursor_col(0), 3); // "❯ " — one past the prompt
    }

    #[test]
    fn char_count_multibyte() {
        let mut r = Repl::new();
        r.buf = "héllo".to_string().into_bytes(); // é is 2 bytes
        assert_eq!(r.char_count_before(r.buf.len()), 5);
        // cursor in the middle, on a char boundary after 'é'
        assert_eq!(r.char_count_before(3), 2);
    }

    #[test]
    fn replay_message_populates_history_ignoring_tool_results_and_duplicates() {
        let mut r = Repl::new();
        r.tty = false;
        let hub = Hub::new(vec![]);

        r.handle(
            &hub,
            &events::ReplayMessage {
                json: r#"{"role":"user","content":"first prompt"}"#.into(),
            },
        );
        // Duplicate consecutive user prompt is deduped.
        r.handle(
            &hub,
            &events::ReplayMessage {
                json: r#"{"role":"user","content":"first prompt"}"#.into(),
            },
        );
        // Assistant message is ignored.
        r.handle(
            &hub,
            &events::ReplayMessage {
                json: r#"{"role":"assistant","content":[{"type":"text","text":"answer"}]}"#.into(),
            },
        );
        // User tool_result array message is ignored.
        r.handle(
            &hub,
            &events::ReplayMessage {
                json: r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"1","content":"ok"}]}"#.into(),
            },
        );
        r.handle(
            &hub,
            &events::ReplayMessage {
                json: r#"{"role":"user","content":"second prompt"}"#.into(),
            },
        );

        assert_eq!(r.history, vec!["first prompt", "second prompt"]);
        assert_eq!(r.hist_idx, 2);
    }
}

// --- raw mode ---------------------------------------------------------------
//
// Minimal cooked→cbreak switch: turn off line buffering and echo, keep signal
// generation (Ctrl-C still generates SIGINT, even mid-LLM-call). Restored on
// Drop, and by the SIGINT handlers.

struct RawMode;

impl RawMode {
    fn enable() -> Option<RawMode> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::IEXTEN);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return None;
            }
            *std::ptr::addr_of_mut!(ORIG_TERMIOS) = Some(orig);
            Some(RawMode)
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { restore_termios() };
    }
}

static mut ORIG_TERMIOS: Option<libc::termios> = None;

/// Set when SIGINT arrives at the prompt; the repl turns it into `Quit`.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

unsafe fn restore_termios() {
    unsafe {
        let orig = &*std::ptr::addr_of!(ORIG_TERMIOS);
        if let Some(t) = orig.as_ref() {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
    }
}

/// SIGINT at the prompt: restore the terminal, set the flag, and *return*.
/// The blocked raw `read()` then fails with `EINTR` (the handler is
/// installed without `SA_RESTART` on purpose), `read_byte` maps that to
/// `None`, and the repl's EOF path emits `Quit` — so plugins (min-herdr's
/// release, anything later) see the exit instead of the process vanishing.
unsafe extern "C" fn on_sigint_quit(_sig: libc::c_int) {
    unsafe {
        restore_termios();
        INTERRUPTED.store(true, Ordering::SeqCst);
    }
}

/// SIGINT mid-turn: the repl swapped this in when the turn started. An
/// interrupt here cannot unwind through the blocking LLM call (std swallows
/// the `EINTR`), so restore the terminal and hard-exit with the conventional
/// 130 — the same visible outcome, just without events.
unsafe extern "C" fn on_sigint_exit(_sig: libc::c_int) {
    unsafe {
        restore_termios();
        libc::_exit(130);
    }
}

fn install_sigint_restore() {
    install_sigint_handler(on_sigint_quit);
}

fn install_sigint_exit_handler() {
    install_sigint_handler(on_sigint_exit);
}

/// `signal`-equivalent registration, but without `SA_RESTART`: the point of
/// the flag-and-return handler is that a blocked `read` wakes up.
fn install_sigint_handler(handler: unsafe extern "C" fn(libc::c_int)) {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as usize;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

#[cfg(all(test, unix))]
mod sigint_tests {
    use super::*;

    /// The handler is swapped for the hard-exit one during a turn (where an
    /// interrupt cannot unwind through the blocking LLM call) and back
    /// afterwards. Pin both directions; we cannot actually take SIGINT in a
    /// test without killing the test runner.
    #[test]
    fn mid_turn_ctrl_c_swaps_handlers_and_back() {
        install_sigint_restore();
        install_sigint_exit_handler();
        assert_eq!(sigint_handler(), on_sigint_exit as *const () as usize);
        install_sigint_restore();
        assert_eq!(sigint_handler(), on_sigint_quit as *const () as usize);
    }

    fn sigint_handler() -> usize {
        unsafe {
            let mut got: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(libc::SIGINT, std::ptr::null(), &mut got), 0);
            got.sa_sigaction
        }
    }
}
