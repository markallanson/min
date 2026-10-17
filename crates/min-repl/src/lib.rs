//! min-repl — the terminal UI.
//!
//! Owns stdin/stdout: prompts, line editing (cursor movement, backspace,
//! delete, ↑/↓ history), slash commands, and display of every event the rest
//! of min emits (streaming assistant text, tool activity, info/errors).
//!
//! When stdin is not a tty (piped input) raw mode is skipped and lines are
//! read as-is, so min can be scripted: `echo 'hello' | min`.
//!
//! Ctrl-C exits at any time (a SIGINT handler restores the terminal first);
//! Ctrl-D quits at an empty prompt.

use min_core::{events, Hub, Plugin};
use std::any::Any;
use std::io::{Read, Write};

mod ansi;

pub struct Repl {
    tty: bool,
    history: Vec<String>,
    hist_idx: usize,
    buf: Vec<u8>,  // input so far; always valid UTF-8
    pos: usize,    // edit cursor: byte offset into `buf`, always a char boundary
    at_line_start: bool,
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
            s => Some(Box::new(events::UserInput { text: s.to_string() })),
        }
    }

    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if event.downcast_ref::<events::Quit>().is_some() {
            hub.quit();
            return;
        }
        if let Some(e) = event.downcast_ref::<events::AssistantDelta>() {
            self.write(&e.text);
            self.at_line_start = e.text.ends_with('\n');
        } else if event.downcast_ref::<events::MessageEnd>().is_some() {
            if !self.at_line_start {
                self.write("\n");
            }
            self.at_line_start = true;
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
        if !self.at_line_start {
            self.write("\n");
        }
        self.write(s);
        self.write("\n");
        self.at_line_start = true;
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
        let col = PROMPT_COLS + self.char_count_before(self.pos);
        self.write(&format!("\r\x1b[{col}G")); // column (1-based)
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

#[cfg(test)]
mod tests {
    use super::*;

    // No stdin/stdout here: these exercise the pure editing helpers.

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
    fn char_count_multibyte() {
        let mut r = Repl::new();
        r.buf = "héllo".to_string().into_bytes(); // é is 2 bytes
        assert_eq!(r.char_count_before(r.buf.len()), 5);
        // cursor in the middle, on a char boundary after 'é'
        assert_eq!(r.char_count_before(3), 2);
    }
}

// --- raw mode ---------------------------------------------------------------
//
// Minimal cooked→cbreak switch: turn off line buffering and echo, keep signal
// generation (Ctrl-C still works, even mid-LLM-call). Restored on Drop, and
// by the SIGINT handler if we die by interrupt.

struct RawMode {
    orig: libc::termios,
}

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
            Some(RawMode { orig })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.orig);
        }
    }
}

static mut ORIG_TERMIOS: Option<libc::termios> = None;

unsafe extern "C" fn on_sigint(_sig: libc::c_int) {
    unsafe {
        let orig = &*std::ptr::addr_of!(ORIG_TERMIOS);
        if let Some(t) = orig.as_ref() {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
        libc::_exit(130);
    }
}

fn install_sigint_restore() {
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
}
