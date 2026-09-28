//! Display helpers for the handler-mode line editor.
//!
//! Handles terminal-width wrapping, cursor tracking, and redrawing the
//! prompt + input buffer.

use std::io::{self, Write};

use crossterm::{
    cursor,
    queue,
    terminal::{self, Clear, ClearType},
};

use crate::terminal::LineEditor;

use super::state::DisplayState;

/// Compute `(total_rows, cursor_row, cursor_col)` for the input display.
///
/// Uses the *visible* (ANSI-stripped) prompt for column maths.
/// Handles terminal-width wrapping and explicit `\n` in the buffer.
pub(super) fn display_geometry(
    prompt_vis: &str,
    buf: &str,
    cursor_pos: usize,
    width: usize,
) -> (usize, usize, usize) {
    let width = width.max(1);
    let mut col = 0usize;
    let mut row = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;

    for _ in prompt_vis.chars() {
        col += 1;
        if col >= width {
            col = 0;
            row += 1;
        }
    }

    for (i, ch) in buf.chars().enumerate() {
        if i == cursor_pos {
            cursor_row = row;
            cursor_col = col;
        }
        if ch == '\n' {
            col = 0;
            row += 1;
        } else {
            col += 1;
            if col >= width {
                col = 0;
                row += 1;
            }
        }
    }
    if cursor_pos == buf.chars().count() {
        cursor_row = row;
        cursor_col = col;
    }

    (row + 1, cursor_row, cursor_col)
}

/// Write `buf` character-by-character starting at column `start_col`, emitting
/// explicit `\r\n` every time the column counter reaches `tw`.  This prevents
/// the "pending-wrap" terminal state that occurs when text fills the last column
/// exactly — without explicit wraps, `MoveToColumn(0)` would stay on the same
/// row instead of advancing to the next.
fn write_buf_with_explicit_wraps(
    stdout: &mut io::Stdout,
    start_col: usize,
    buf: &str,
    tw: usize,
) -> io::Result<()> {
    let mut col = start_col;
    for ch in buf.chars() {
        if ch == '\n' {
            write!(stdout, "\r\n")?;
            col = 0;
        } else {
            write!(stdout, "{}", ch)?;
            col += 1;
            if col >= tw {
                write!(stdout, "\r\n")?;
                col = 0;
            }
        }
    }
    Ok(())
}

/// Erase the previous input display and redraw prompt + buffer.
///
/// `prev = Some(s)` — cursor is within the old display; go up `s.cursor_row`
///                    rows to the top before clearing.
/// `prev = None`   — cursor is already at column 0 of a fresh line.
pub(super) fn redraw_input(
    stdout: &mut io::Stdout,
    prompt_display: &str,
    prompt_vis: &str,
    editor: &LineEditor,
    prev: Option<DisplayState>,
) -> io::Result<DisplayState> {
    let (tw, _) = terminal::size().unwrap_or((80, 24));
    let tw = tw.max(1) as usize;

    if let Some(p) = prev {
        if p.cursor_row > 0 {
            queue!(stdout, cursor::MoveUp(p.cursor_row))?;
        }
    }
    queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::FromCursorDown))?;

    let buf = editor.buffer_str();

    // Write the prompt (contains ANSI colour codes) then the buffer with
    // explicit wrap-boundary \r\n so the terminal never enters pending-wrap.
    write!(stdout, "{}", prompt_display)?;
    let start_col = prompt_vis.chars().count() % tw;
    write_buf_with_explicit_wraps(stdout, start_col, &buf, tw)?;

    let (total_rows, cursor_row, cursor_col) =
        display_geometry(prompt_vis, &buf, editor.cursor_pos(), tw);

    let rows_below = (total_rows as u16).saturating_sub(cursor_row as u16 + 1);
    if rows_below > 0 {
        queue!(stdout, cursor::MoveUp(rows_below))?;
    }
    queue!(stdout, cursor::MoveToColumn(cursor_col as u16))?;

    stdout.flush()?;
    Ok(DisplayState {
        cursor_row: cursor_row as u16,
    })
}

/// Erase the current input display — call before writing remote output.
pub(super) fn clear_input(stdout: &mut io::Stdout, state: DisplayState) -> io::Result<()> {
    if state.cursor_row > 0 {
        queue!(stdout, cursor::MoveUp(state.cursor_row))?;
    }
    queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_geometry_single_row() {
        let (rows, crow, ccol) = display_geometry("$ ", "abc", 1, 80);
        assert_eq!(rows, 1);
        assert_eq!(crow, 0);
        assert_eq!(ccol, 3); // 2 (prompt) + 1 (cursor after first char)
    }

    #[test]
    fn display_geometry_wraps() {
        // 5 chars in width-4: "hell" on row 0, "o" on row 1.
        let (rows, crow, ccol) = display_geometry("", "hello", 5, 4);
        assert_eq!(rows, 2);
        assert_eq!(crow, 1);
        assert_eq!(ccol, 1);
    }

    #[test]
    fn display_geometry_newline_in_buf() {
        let (rows, crow, ccol) = display_geometry("$ ", "a\nb", 3, 80);
        assert_eq!(rows, 2);
        assert_eq!(crow, 1);
        assert_eq!(ccol, 1);
    }

    #[test]
    fn display_geometry_cursor_at_start() {
        let (rows, crow, ccol) = display_geometry("$ ", "abc", 0, 80);
        assert_eq!(rows, 1);
        assert_eq!(crow, 0);
        assert_eq!(ccol, 2); // cursor right after prompt
    }
}
