//! Reusable picker (M4): a small overlay list with type-to-filter,
//! reused by `/resume` now and by `/model` and friends in M5.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// One selectable option: a primary label and a dim detail line.
#[derive(Debug, Clone, PartialEq)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
}

/// An open picker. Filtering is a case-insensitive substring match on the
/// label; the selection survives filter changes.
#[derive(Debug)]
pub struct Picker {
    title: String,
    items: Vec<PickerItem>,
    /// Indices into `items` that match the current query.
    filtered: Vec<usize>,
    /// Index into `filtered`.
    selected: usize,
    query: String,
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            title: title.into(),
            items,
            filtered,
            selected: 0,
            query: String::new(),
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// The currently selected item, if any.
    // Test-only for now; M5's `/model` picker will use it too.
    #[allow(dead_code)]
    pub fn selected_item(&self) -> Option<&PickerItem> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.items.get(i))
    }

    /// Take the selection and close: returns the item's index into the
    /// original item list (a stable handle for the caller), if any.
    pub fn confirm(&mut self) -> Option<usize> {
        self.filtered.get(self.selected).copied()
    }

    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    pub fn pop_char(&mut self) {
        if self.query.pop().is_some() {
            self.refilter();
        }
    }

    fn refilter(&mut self) {
        let query = self.query.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.label.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
    }

    /// Pre-wrapped display lines: query row, then one per filtered item.
    /// `visible` limits how many list rows are produced (the caller
    /// scrolls by offsetting `selected`).
    pub fn lines(&self, visible: usize) -> Vec<Line<'static>> {
        let query_style = Style::new().fg(Color::DarkGray);
        let mut lines = vec![Line::from(vec![
            Span::styled("search: ", query_style),
            Span::raw(self.query.clone()),
        ])];

        if self.filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "no matches",
                Style::new().fg(Color::DarkGray),
            )));
            return lines;
        }

        // Keep the selection in a sliding window of `visible` rows.
        let start = self.selected.saturating_sub(visible.saturating_sub(1));
        for (row, &item_index) in self.filtered[start..].iter().take(visible).enumerate() {
            let item = &self.items[item_index];
            let is_selected = start + row == self.selected;
            let label_style = if is_selected {
                Style::new()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::new()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", if is_selected { '▸' } else { ' ' }),
                    label_style,
                ),
                Span::styled(item.label.clone(), label_style),
                Span::styled(
                    format!("  {}", item.detail),
                    Style::new().fg(Color::DarkGray),
                ),
            ]));
        }
        if self.filtered.len() > visible {
            lines.push(Line::from(Span::styled(
                format!("… {} more", self.filtered.len() - visible),
                Style::new().fg(Color::DarkGray),
            )));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    fn picker() -> Picker {
        Picker::new(
            "test",
            vec![
                PickerItem {
                    label: "alpha".into(),
                    detail: "a".into(),
                },
                PickerItem {
                    label: "beta".into(),
                    detail: "b".into(),
                },
                PickerItem {
                    label: "alphabet".into(),
                    detail: "c".into(),
                },
            ],
        )
    }

    #[test]
    fn navigation_moves_selection() {
        let mut picker = picker();
        assert_eq!(picker.selected_item().unwrap().label, "alpha");
        picker.down();
        assert_eq!(picker.selected_item().unwrap().label, "beta");
        picker.down();
        picker.down();
        assert_eq!(picker.selected_item().unwrap().label, "alphabet", "clamped");
        picker.up();
        assert_eq!(picker.selected_item().unwrap().label, "beta");
        picker.up();
        picker.up();
        picker.up();
        assert_eq!(picker.selected_item().unwrap().label, "alpha", "clamped");
    }

    #[test]
    fn filtering_matches_and_confirms_returns_stable_index() {
        let mut picker = picker();
        picker.push_char('p');
        picker.push_char('h');
        let texts = line_texts(&picker.lines(10));
        assert!(texts.iter().any(|t| t.contains("alpha")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("alphabet")));
        assert!(!texts.iter().any(|t| t.contains("beta")));

        // Selection survives refiltering; `confirm` returns a stable
        // index into the full item list.
        picker.pop_char();
        picker.pop_char(); // clear the earlier `ph` query
        picker.push_char('b');
        picker.push_char('e');
        assert_eq!(picker.confirm(), Some(1), "only `beta` matches `be`");

        picker.pop_char();
        picker.pop_char();
        assert_eq!(picker.confirm(), Some(0), "empty query selects the first");
    }

    #[test]
    fn no_matches_shows_hint_and_confirm_is_none() {
        let mut picker = picker();
        picker.push_char('z');
        picker.push_char('z');
        assert!(picker.selected_item().is_none());
        assert_eq!(picker.confirm(), None);
        let lines = picker.lines(10);
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.content.contains("no matches")))
        );
    }

    #[test]
    fn long_lists_scroll_around_the_selection() {
        let mut picker = Picker::new(
            "test",
            (0..30)
                .map(|i| PickerItem {
                    label: format!("item-{i:02}"),
                    detail: String::new(),
                })
                .collect(),
        );
        for _ in 0..20 {
            picker.down();
        }
        let lines = picker.lines(8);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(texts.iter().any(|t| t.contains("item-20")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("more")));
    }

    #[test]
    fn filter_change_resets_selection_to_first_match() {
        let mut picker = picker();
        picker.down();
        picker.push_char('a');
        assert_eq!(picker.selected_item().unwrap().label, "alpha");
    }
}
