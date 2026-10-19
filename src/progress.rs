//! In-place status line on stderr, shared by every step that makes the user wait.

use std::fmt::Display;
use std::io::{IsTerminal, Write};

/// One redrawn stderr line. Inert when disabled or when stderr is not a terminal:
/// the `\r` redraw only makes sense on a TTY and would otherwise log one line per tick.
pub struct Progress {
    active: bool,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        Self {
            active: enabled && std::io::stderr().is_terminal(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn show(&self, text: impl Display) {
        if self.active {
            eprint!("\x1b[2K\r{text}");
            let _ = std::io::stderr().flush();
        }
    }

    /// `{phase}... current/total`, or `{phase}... current` when the total is unknown (0).
    pub fn count(&self, phase: &str, current: usize, total: usize) {
        if total == 0 {
            self.show(format_args!("{phase}... {current}"));
        } else {
            self.show(format_args!("{phase}... {current}/{total}"));
        }
    }

    pub fn clear(&self) {
        self.show("");
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.clear();
    }
}
