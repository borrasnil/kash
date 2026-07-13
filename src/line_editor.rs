use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What the session loop should do after a keypress.
pub enum LineAction {
    /// Buffer changed or cursor moved — caller should redraw the input line.
    Continue,
    /// User pressed Enter. The owned string is the complete input line.
    Submit(String),
    /// Send raw bytes to the remote shell (CTRL+C → 0x03, CTRL+Z → 0x1a).
    SendRaw(Vec<u8>),
    /// CTRL+L — caller should clear the screen and redraw.
    ClearScreen,
    /// CTRL+D on an empty buffer — caller should disconnect.
    Disconnect,
}

pub struct LineEditor {
    buffer: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    /// Saved in-progress text while navigating history.
    saved_buffer: String,
}

impl LineEditor {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            saved_buffer: String::new(),
        }
    }

    /// Process one keypress and return the resulting action.
    pub fn handle_key(&mut self, key: KeyEvent) -> LineAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            // Alt+Enter: insert literal newline for multiline commands.
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                self.buffer.insert(self.cursor, '\n');
                self.cursor += 1;
                LineAction::Continue
            }

            // Submit
            KeyCode::Enter => {
                let line: String = self.buffer.iter().collect();
                self.push_history(&line);
                self.buffer.clear();
                self.cursor = 0;
                self.history_pos = None;
                self.saved_buffer.clear();
                LineAction::Submit(line)
            }

            // Shell signals
            KeyCode::Char('c') if ctrl => {
                self.buffer.clear();
                self.cursor = 0;
                self.history_pos = None;
                self.saved_buffer.clear();
                LineAction::SendRaw(vec![0x03])
            }
            KeyCode::Char('d') if ctrl => {
                if self.buffer.is_empty() {
                    LineAction::Disconnect
                } else {
                    if self.cursor < self.buffer.len() {
                        self.buffer.remove(self.cursor);
                    }
                    LineAction::Continue
                }
            }
            KeyCode::Char('l') if ctrl => LineAction::ClearScreen,
            KeyCode::Char('z') if ctrl => LineAction::SendRaw(vec![0x1a]),

            // Cursor to start: CTRL+A or Home
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                LineAction::Continue
            }
            KeyCode::Home => {
                self.cursor = 0;
                LineAction::Continue
            }

            // Cursor to end: CTRL+E or End
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.buffer.len();
                LineAction::Continue
            }
            KeyCode::End => {
                self.cursor = self.buffer.len();
                LineAction::Continue
            }

            // Cursor movement
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                LineAction::Continue
            }
            KeyCode::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor += 1;
                }
                LineAction::Continue
            }

            // Kill commands
            KeyCode::Char('u') if ctrl => {
                self.buffer.drain(..self.cursor);
                self.cursor = 0;
                LineAction::Continue
            }
            KeyCode::Char('k') if ctrl => {
                self.buffer.truncate(self.cursor);
                LineAction::Continue
            }
            KeyCode::Char('w') if ctrl => {
                let mut pos = self.cursor;
                while pos > 0 && self.buffer[pos - 1] == ' ' {
                    pos -= 1;
                }
                while pos > 0 && self.buffer[pos - 1] != ' ' {
                    pos -= 1;
                }
                self.buffer.drain(pos..self.cursor);
                self.cursor = pos;
                LineAction::Continue
            }

            // Delete
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.buffer.remove(self.cursor);
                }
                LineAction::Continue
            }
            KeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    self.buffer.remove(self.cursor);
                }
                LineAction::Continue
            }

            // History
            KeyCode::Up => {
                self.history_prev();
                LineAction::Continue
            }
            KeyCode::Down => {
                self.history_next();
                LineAction::Continue
            }

            // Insert character
            KeyCode::Char(c) => {
                self.buffer.insert(self.cursor, c);
                self.cursor += 1;
                LineAction::Continue
            }

            _ => LineAction::Continue,
        }
    }

    /// Bulk-insert `s` at the cursor (called on `Event::Paste`).
    /// Newlines in `s` are inserted literally so multiline pastes work.
    /// Caller should call `redraw_input` once after this returns.
    pub fn insert_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.buffer.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    /// Current buffer contents as a `String`.
    pub fn buffer_str(&self) -> String {
        self.buffer.iter().collect()
    }

    /// Current cursor position (char index, not byte offset).
    pub fn cursor_pos(&self) -> usize {
        self.cursor
    }

    fn push_history(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_string());
            if self.history.len() > 1000 {
                self.history.remove(0);
            }
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_pos {
            None => {
                self.saved_buffer = self.buffer_str();
                self.history_pos = Some(self.history.len() - 1);
            }
            Some(0) => return,
            Some(pos) => self.history_pos = Some(pos - 1),
        }
        let entry = self.history[self.history_pos.unwrap()].clone();
        self.buffer = entry.chars().collect();
        self.cursor = self.buffer.len();
    }

    fn history_next(&mut self) {
        match self.history_pos {
            None => return,
            Some(pos) if pos + 1 >= self.history.len() => {
                self.history_pos = None;
                let saved = std::mem::take(&mut self.saved_buffer);
                self.buffer = saved.chars().collect();
                self.cursor = self.buffer.len();
            }
            Some(pos) => {
                self.history_pos = Some(pos + 1);
                let entry = self.history[pos + 1].clone();
                self.buffer = entry.chars().collect();
                self.cursor = self.buffer.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn type_str(ed: &mut LineEditor, s: &str) {
        for c in s.chars() {
            ed.handle_key(press(KeyCode::Char(c)));
        }
    }

    fn submit(ed: &mut LineEditor) -> String {
        match ed.handle_key(press(KeyCode::Enter)) {
            LineAction::Submit(s) => s,
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn type_and_submit() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "ls -la");
        assert_eq!(submit(&mut ed), "ls -la");
        assert!(ed.buffer_str().is_empty());
    }

    #[test]
    fn backspace_removes_char() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "ab");
        ed.handle_key(press(KeyCode::Backspace));
        assert_eq!(ed.buffer_str(), "a");
        assert_eq!(ed.cursor_pos(), 1);
    }

    #[test]
    fn delete_at_cursor() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "abc");
        ed.handle_key(press(KeyCode::Home));
        ed.handle_key(press(KeyCode::Delete));
        assert_eq!(ed.buffer_str(), "bc");
    }

    #[test]
    fn insert_in_middle() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "ac");
        ed.handle_key(press(KeyCode::Left));
        ed.handle_key(press(KeyCode::Char('b')));
        assert_eq!(ed.buffer_str(), "abc");
        assert_eq!(ed.cursor_pos(), 2);
    }

    #[test]
    fn ctrl_c_clears_and_sends_etx() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "some text");
        match ed.handle_key(ctrl('c')) {
            LineAction::SendRaw(b) => assert_eq!(b, vec![0x03]),
            _ => panic!("expected SendRaw"),
        }
        assert!(ed.buffer_str().is_empty());
        assert_eq!(ed.cursor_pos(), 0);
    }

    #[test]
    fn ctrl_z_sends_sub() {
        let mut ed = LineEditor::new();
        match ed.handle_key(ctrl('z')) {
            LineAction::SendRaw(b) => assert_eq!(b, vec![0x1a]),
            _ => panic!("expected SendRaw"),
        }
    }

    #[test]
    fn ctrl_l_returns_clear_screen() {
        let mut ed = LineEditor::new();
        assert!(matches!(ed.handle_key(ctrl('l')), LineAction::ClearScreen));
    }

    #[test]
    fn ctrl_d_empty_disconnects() {
        let mut ed = LineEditor::new();
        assert!(matches!(ed.handle_key(ctrl('d')), LineAction::Disconnect));
    }

    #[test]
    fn ctrl_d_nonempty_deletes_forward() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "ab");
        ed.handle_key(press(KeyCode::Home));
        ed.handle_key(ctrl('d'));
        assert_eq!(ed.buffer_str(), "b");
    }

    #[test]
    fn ctrl_a_and_home_move_to_start() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "hello");
        ed.handle_key(ctrl('a'));
        assert_eq!(ed.cursor_pos(), 0);

        type_str(&mut ed, "hello");
        ed.handle_key(press(KeyCode::Home));
        assert_eq!(ed.cursor_pos(), 0);
    }

    #[test]
    fn ctrl_e_and_end_move_to_end() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "hello");
        ed.handle_key(ctrl('a'));
        ed.handle_key(ctrl('e'));
        assert_eq!(ed.cursor_pos(), 5);

        ed.handle_key(ctrl('a'));
        ed.handle_key(press(KeyCode::End));
        assert_eq!(ed.cursor_pos(), 5);
    }

    #[test]
    fn ctrl_u_kills_to_start() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "hello");
        ed.handle_key(ctrl('u'));
        assert!(ed.buffer_str().is_empty());
        assert_eq!(ed.cursor_pos(), 0);
    }

    #[test]
    fn ctrl_k_kills_to_end() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "hello");
        ed.handle_key(ctrl('a'));
        ed.handle_key(ctrl('k'));
        assert!(ed.buffer_str().is_empty());
    }

    #[test]
    fn ctrl_w_kills_word() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "hello world");
        ed.handle_key(ctrl('w'));
        assert_eq!(ed.buffer_str(), "hello ");
    }

    #[test]
    fn ctrl_w_skips_trailing_spaces() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "hello   ");
        ed.handle_key(ctrl('w'));
        assert_eq!(ed.buffer_str(), "");
    }

    #[test]
    fn history_up_down() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "ls");
        submit(&mut ed);
        type_str(&mut ed, "pwd");
        submit(&mut ed);

        ed.handle_key(press(KeyCode::Up));
        assert_eq!(ed.buffer_str(), "pwd");
        ed.handle_key(press(KeyCode::Up));
        assert_eq!(ed.buffer_str(), "ls");
        ed.handle_key(press(KeyCode::Up)); // already at start
        assert_eq!(ed.buffer_str(), "ls");
        ed.handle_key(press(KeyCode::Down));
        assert_eq!(ed.buffer_str(), "pwd");
        ed.handle_key(press(KeyCode::Down)); // back to fresh buffer
        assert_eq!(ed.buffer_str(), "");
    }

    #[test]
    fn history_preserves_in_progress_text() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "ls");
        submit(&mut ed);

        type_str(&mut ed, "partial");
        ed.handle_key(press(KeyCode::Up));
        assert_eq!(ed.buffer_str(), "ls");
        ed.handle_key(press(KeyCode::Down));
        assert_eq!(ed.buffer_str(), "partial");
    }

    #[test]
    fn history_no_duplicate_consecutive() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "ls");
        submit(&mut ed);
        type_str(&mut ed, "ls");
        submit(&mut ed);

        ed.handle_key(press(KeyCode::Up));
        assert_eq!(ed.buffer_str(), "ls");
        ed.handle_key(press(KeyCode::Up)); // stays at oldest entry
        assert_eq!(ed.buffer_str(), "ls");
    }

    #[test]
    fn cursor_left_right_bounds() {
        let mut ed = LineEditor::new();
        ed.handle_key(press(KeyCode::Left)); // no crash at 0
        assert_eq!(ed.cursor_pos(), 0);

        type_str(&mut ed, "a");
        ed.handle_key(press(KeyCode::Right)); // at end already
        assert_eq!(ed.cursor_pos(), 1);
        ed.handle_key(press(KeyCode::Right)); // no crash past end
        assert_eq!(ed.cursor_pos(), 1);
    }

    #[test]
    fn submit_clears_history_nav_state() {
        let mut ed = LineEditor::new();
        type_str(&mut ed, "ls");
        submit(&mut ed);

        ed.handle_key(press(KeyCode::Up));
        assert_eq!(ed.buffer_str(), "ls");
        submit(&mut ed); // submit from history

        // Should be able to navigate again cleanly
        ed.handle_key(press(KeyCode::Up));
        assert_eq!(ed.buffer_str(), "ls");
    }
}
