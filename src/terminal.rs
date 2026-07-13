use std::io::{self, Write};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;

/// RAII guard that enables raw mode and bracketed paste on creation, and
/// restores the terminal on drop (including panics).
pub struct RawModeGuard;

impl RawModeGuard {
    pub fn enable() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        // Bracketed paste causes the terminal to wrap pastes in
        // `\x1b[200~ ... \x1b[201~`, which crossterm surfaces as
        // `Event::Paste(String)`.  This lets us send an entire paste in one
        // write rather than character-by-character.
        let _ = execute!(io::stdout(), EnableBracketedPaste);
        Ok(RawModeGuard)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
        // Move to a clean line so the calling shell's prompt isn't garbled.
        let _ = write!(stdout, "\r\n");
        let _ = stdout.flush();
    }
}
