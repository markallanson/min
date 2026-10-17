//! Tiny ANSI helpers. Disabled automatically when stdout is not a tty or
//! `NO_COLOR` is set.

use std::sync::OnceLock;

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("NO_COLOR").is_err() && unsafe { libc::isatty(1) == 1 }
    })
}

fn wrap(code: &str, s: &str) -> String {
    if enabled() {
        format!("{code}{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    wrap("\x1b[2m", s)
}
pub fn cyan(s: &str) -> String {
    wrap("\x1b[36m", s)
}
pub fn green(s: &str) -> String {
    wrap("\x1b[32m", s)
}
pub fn red(s: &str) -> String {
    wrap("\x1b[31m", s)
}
