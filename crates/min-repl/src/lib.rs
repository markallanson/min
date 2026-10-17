//! min-repl — the terminal UI.
//!
//! Owns stdin/stdout: prompts, line editing (backspace, ↑/↓ history),
//! slash commands, and display of every event the rest of min emits
//! (streaming assistant text, tool activity, info/errors).
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
    buf: Vec<u8>,
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
                self.put_line(&ansi::dim("/help · /clear · /quit — ctrl-d quits, ↑/↓ history"));
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
                    return Some(s);
                }
                0x7f | 0x08 => {
                    if !self.buf.is_empty() {
                        self.buf.pop();
                        // keep UTF-8 characters whole
                        while self.buf.last().is_some_and(|c| c & 0xC0 == 0x80) {
                            self.buf.pop();
                        }
                        self.write("\u{8} \u{8}");
                    }
                }
                0x04 => {
                    if self.buf.is_empty() {
                        self.write("\n");
                        return None;
                    }
                }
                0x15 => {
                    // ctrl-u: clear the line
                    for _ in 0..self.buf.len() {
                        self.write("\u{8} \u{8}");
                    }
                    self.buf.clear();
                }
                0x1b => {
                    self.escape()?;
                }
                0x00..=0x1f => {}
                _ => {
                    self.buf.push(b);
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&[b]);
                    let _ = out.flush();
                }
            }
        }
    }

    /// After ESC: handle `ESC [ A/B` (up/down history); swallow the rest.
    fn escape(&mut self) -> Option<()> {
        let b1 = read_byte()?;
        let b2 = read_byte()?;
        if b1 == b'[' {
            match b2 {
                b'A' => self.recall(-1),
                b'B' => self.recall(1),
                _ => {}
            }
        }
        Some(())
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
        self.write("\r\x1b[K"); // clear line
        self.prompt();
        self.write(&text);
        self.buf = text.into_bytes();
    }
}

fn read_byte() -> Option<u8> {
    let mut b = [0u8];
    match std::io::stdin().read(&mut b) {
        Ok(1) => Some(b[0]),
        _ => None,
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
