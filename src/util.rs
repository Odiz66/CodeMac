//! Small shared helpers.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

pub fn init_clock() {
    START.get_or_init(Instant::now);
}

/// Time since start as `hh:mm:ss`.
pub fn stamp() -> String {
    let s = START.get_or_init(Instant::now).elapsed().as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// `println!` with an elapsed-time prefix.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        println!("[{}] {}", $crate::util::stamp(), format!($($arg)*))
    };
}

/// ANSI colour helpers that turn themselves off when stdout is not a terminal.
pub fn paint(text: &str, code: &str) -> String {
    use std::io::IsTerminal;
    static TTY: OnceLock<bool> = OnceLock::new();
    if *TTY.get_or_init(|| {
        std::io::stdout().is_terminal() || std::env::var_os("FORCE_COLOR").is_some()
    }) {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}
