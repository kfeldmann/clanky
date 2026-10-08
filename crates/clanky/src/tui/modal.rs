//! Modal dialogs: a small overlay that takes the keyboard until it is
//! confirmed or cancelled. Distinct from the picker (M5), which selects
//! from a list of known items — a modal collects free-form text.
//!
//! The first (and currently only) user is `/md`, whose filename can be
//! given on the command line or, when omitted, typed here.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What the user did with an open modal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModalOutcome {
    /// Still open.
    None,
    /// Cancelled (Esc): the value is discarded.
    Cancelled,
    /// Confirmed (Enter): the value was accepted.
    Accepted,
}

/// A single-field text dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct Modal {
    /// Caption on the top border, e.g. `export markdown`.
    title: String,
    /// Explanatory line above the input field.
    prompt: String,
    /// The text being typed.
    pub input: String,
    /// Byte offset of the caret into `input` (always on a char boundary).
    pub cursor: usize,
}

impl Modal {
    /// A new modal with `initial` text already in the field.
    pub fn new(title: impl Into<String>, prompt: impl Into<String>, initial: &str) -> Self {
        Self {
            title: title.into(),
            prompt: prompt.into(),
            input: initial.to_string(),
            cursor: initial.len(),
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// The trimmed value, or `None` when the field is blank.
    pub fn value(&self) -> Option<String> {
        let value = self.input.trim();
        (!value.is_empty()).then(|| value.to_string())
    }

    /// Handle one key press: Esc cancels, Enter accepts (only a non-blank
    /// value), the editing keys mirror the main input line.
    pub fn key(&mut self, key: KeyEvent) -> ModalOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (ctrl, key.code) {
            (_, KeyCode::Esc) => ModalOutcome::Cancelled,
            (_, KeyCode::Enter) => match self.value() {
                Some(_) => ModalOutcome::Accepted,
                None => ModalOutcome::None, // nothing to accept yet
            },
            (true, KeyCode::Char('u')) => {
                self.input.clear();
                self.cursor = 0;
                ModalOutcome::None
            }
            (false, KeyCode::Backspace) => {
                if let Some(prev) = self.prev_boundary() {
                    self.input.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                }
                ModalOutcome::None
            }
            (false, KeyCode::Delete) => {
                if self.cursor < self.input.len() {
                    let next = self.cursor
                        + self.input[self.cursor..]
                            .chars()
                            .next()
                            .map_or(0, char::len_utf8);
                    self.input.replace_range(self.cursor..next, "");
                }
                ModalOutcome::None
            }
            (false, KeyCode::Left) => {
                if let Some(prev) = self.prev_boundary() {
                    self.cursor = prev;
                }
                ModalOutcome::None
            }
            (false, KeyCode::Right) => {
                self.cursor = self
                    .input
                    .char_indices()
                    .skip_while(|(i, _)| *i <= self.cursor)
                    .map(|(i, _)| i)
                    .next()
                    .unwrap_or(self.input.len());
                ModalOutcome::None
            }
            (false, KeyCode::Home) => {
                self.cursor = 0;
                ModalOutcome::None
            }
            (false, KeyCode::End) => {
                self.cursor = self.input.len();
                ModalOutcome::None
            }
            (false, KeyCode::Char(c)) => {
                self.input.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                ModalOutcome::None
            }
            _ => ModalOutcome::None,
        }
    }

    /// Byte offset of the previous char boundary before the caret.
    fn prev_boundary(&self) -> Option<usize> {
        self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_edits_and_enter_accepts() {
        let mut modal = Modal::new("export markdown", "file name:", "");
        assert_eq!(modal.key(key(KeyCode::Char('a'))), ModalOutcome::None);
        assert_eq!(modal.key(key(KeyCode::Char('b'))), ModalOutcome::None);
        assert_eq!(modal.value().as_deref(), Some("ab"));
        assert_eq!(modal.cursor, 2);
        assert_eq!(modal.key(key(KeyCode::Enter)), ModalOutcome::Accepted);
        assert_eq!(modal.key(key(KeyCode::Esc)), ModalOutcome::Cancelled);
    }

    #[test]
    fn enter_on_a_blank_field_stays_open() {
        let mut modal = Modal::new("t", "p", "   ");
        assert_eq!(modal.key(key(KeyCode::Enter)), ModalOutcome::None);
        assert_eq!(modal.value(), None);
    }

    #[test]
    fn editing_keys_move_and_delete() {
        let mut modal = Modal::new("t", "p", "abc");
        assert_eq!(modal.cursor, 3);
        modal.key(key(KeyCode::Left));
        modal.key(key(KeyCode::Left));
        assert_eq!(modal.cursor, 1);
        modal.key(key(KeyCode::Backspace));
        assert_eq!(modal.input, "bc");
        modal.key(key(KeyCode::Delete));
        assert_eq!(modal.input, "c");
        modal.key(key(KeyCode::Home));
        modal.key(key(KeyCode::Char('x')));
        assert_eq!(modal.input, "xc");
        modal.key(key(KeyCode::End));
        modal.key(key(KeyCode::Char('y')));
        assert_eq!(modal.input, "xcy");

        // ctrl+u clears the field.
        modal.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(modal.input, "");
        assert_eq!(modal.value(), None);
    }

    #[test]
    fn value_is_trimmed() {
        let modal = Modal::new("t", "p", "  spaced  ");
        assert_eq!(modal.value().as_deref(), Some("spaced"));
        assert_eq!(modal.input, "  spaced  ", "the field itself is untouched");
    }

    #[test]
    fn cursor_movement_is_char_safe() {
        let mut modal = Modal::new("t", "p", "é");
        modal.key(key(KeyCode::Left));
        assert_eq!(modal.cursor, 0, "caret lands on the char boundary");
        modal.key(key(KeyCode::Delete));
        assert_eq!(modal.input, "");
    }
}
