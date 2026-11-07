//! A small multi-line text editor used for the prompt, comments and the minibuffer.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, Default)]
pub struct Input {
    pub text: String,
    /// Byte index on a char boundary.
    pub cursor: usize,
}

/// What a key did to the input.
#[derive(Debug, PartialEq, Eq)]
pub enum InputAction {
    Submit,
    Cancel,
    Edited,
    /// Up/Down on the first/last line (used for history).
    HistoryPrev,
    HistoryNext,
    Ignored,
}

impl Input {
    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self { cursor: text.len(), text }
    }

    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..].chars().next().map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map_or(self.text.len(), |i| self.cursor + i)
    }

    fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }

    fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }

    fn move_vertical(&mut self, down: bool) {
        let col = self.text[self.line_start()..self.cursor].chars().count();
        let target_start = if down {
            self.line_end() + 1
        } else {
            let ls = self.line_start();
            self.text[..ls.saturating_sub(1)].rfind('\n').map_or(0, |i| i + 1)
        };
        let line_end = self.text[target_start..].find('\n').map_or(self.text.len(), |i| target_start + i);
        let line = &self.text[target_start..line_end];
        self.cursor = target_start + line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
    }

    /// Applies one key. Enter submits; Alt/Shift+Enter and Ctrl-J insert newlines.
    pub fn handle(&mut self, key: KeyEvent) -> InputAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if alt || shift => self.insert_str("\n"),
            KeyCode::Char('j') if ctrl => self.insert_str("\n"),
            KeyCode::Enter => return InputAction::Submit,
            KeyCode::Esc => return InputAction::Cancel,
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Char('u') if ctrl => {
                let s = self.line_start();
                self.text.replace_range(s..self.cursor, "");
                self.cursor = s;
            }
            KeyCode::Char('k') if ctrl => {
                let e = self.line_end();
                self.text.replace_range(self.cursor..e, "");
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if alt || ctrl => self.delete_word(),
            KeyCode::Char(c) if !ctrl => {
                let mut buf = [0u8; 4];
                self.insert_str(c.encode_utf8(&mut buf));
            }
            KeyCode::Backspace => {
                let p = self.prev_boundary();
                self.text.replace_range(p..self.cursor, "");
                self.cursor = p;
            }
            KeyCode::Delete => {
                let n = self.next_boundary();
                self.text.replace_range(self.cursor..n, "");
            }
            KeyCode::Left => self.cursor = self.prev_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Up if self.on_first_line() => return InputAction::HistoryPrev,
            KeyCode::Down if self.on_last_line() => return InputAction::HistoryNext,
            KeyCode::Up => self.move_vertical(false),
            KeyCode::Down => self.move_vertical(true),
            _ => return InputAction::Ignored,
        }
        InputAction::Edited
    }

    fn delete_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| c.is_whitespace() && c != '\n');
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Hard-wraps the text to `width` columns.
    ///
    /// @returns wrapped lines and the cursor's (x, y) within them.
    pub fn layout(&self, width: usize) -> (Vec<String>, (u16, u16)) {
        let width = width.max(2);
        let mut lines = vec![String::new()];
        let mut w = 0;
        let mut cur = (0u16, 0u16);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cur = (w as u16, (lines.len() - 1) as u16);
            }
            if c == '\n' {
                lines.push(String::new());
                w = 0;
                continue;
            }
            let cw = c.width().unwrap_or(0);
            if w + cw > width {
                lines.push(String::new());
                w = 0;
                if i == self.cursor {
                    cur = (0, (lines.len() - 1) as u16);
                }
            }
            lines.last_mut().unwrap().push(c);
            w += cw;
        }
        if self.cursor >= self.text.len() {
            if w >= width {
                lines.push(String::new());
                w = 0;
            }
            cur = (w as u16, (lines.len() - 1) as u16);
        }
        (lines, cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn editing_and_layout() {
        let mut i = Input::default();
        for c in "héllo wörld".chars() {
            i.handle(key(KeyCode::Char(c)));
        }
        i.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(i.text, "héllo ");
        i.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        i.insert_str("x");
        assert_eq!(i.text, "héllo \nx");
        assert_eq!(i.handle(key(KeyCode::Enter)), InputAction::Submit);
        let (lines, cur) = i.layout(4);
        assert_eq!(lines, vec!["héll", "o ", "x"]);
        assert_eq!(cur, (1, 2));
        i.handle(key(KeyCode::Up));
        assert_eq!(&i.text[i.cursor..], "éllo \nx");
        assert_eq!(i.handle(key(KeyCode::Up)), InputAction::HistoryPrev);
    }
}
