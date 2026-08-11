//! Search and keyboard-state primitives for the TUI actions palette.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

const ACTIONS: [Action; 12] = [
    Action::Reasoning,
    Action::FastMode,
    Action::Theme,
    Action::GlobalToolDetails,
    Action::Subagents,
    Action::Keybindings,
    Action::ContextDiagnostics,
    Action::Btw,
    Action::Simplify,
    Action::Trace,
    Action::Voice,
    Action::CloseBtw,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Action {
    Reasoning,
    FastMode,
    Theme,
    GlobalToolDetails,
    Subagents,
    Keybindings,
    ContextDiagnostics,
    Btw,
    Simplify,
    Trace,
    Voice,
    CloseBtw,
}

impl Action {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Reasoning => "Change reasoning effort",
            Self::FastMode => "Toggle fast mode",
            Self::Theme => "Select theme",
            Self::GlobalToolDetails => "Toggle global tool details",
            Self::Subagents => "Open subagents",
            Self::Keybindings => "Show keyboard shortcuts",
            Self::ContextDiagnostics => "Show context diagnostics",
            Self::Btw => "Open BTW branch",
            Self::Simplify => "Simplify changes",
            Self::Trace => "Open trace",
            Self::Voice => "Toggle voice",
            Self::CloseBtw => "Close BTW branch",
        }
    }

    pub(super) const fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Reasoning => &["reasoning", "thinking", "effort"],
            Self::FastMode => &["fast", "priority"],
            Self::Theme => &["theme", "appearance", "color"],
            Self::GlobalToolDetails => &["tools", "tool details", "expand tools"],
            Self::Subagents => &["agents", "delegation", "workers"],
            Self::Keybindings => &["help", "keys", "keyboard", "shortcuts", "reference"],
            Self::ContextDiagnostics => &["context", "tokens", "cache", "usage", "diagnostics"],
            Self::Btw => &["btw", "fork", "side question"],
            Self::Simplify => &["simplify", "cleanup"],
            Self::Trace => &["trace", "tracing", "jaeger", "observability"],
            Self::Voice => &["voice", "speech", "microphone", "audio"],
            Self::CloseBtw => &["close btw", "return to main", "main branch"],
        }
    }

    fn matches(self, query: &str) -> bool {
        contains_ignore_ascii_case(self.label(), query)
            || self
                .aliases()
                .iter()
                .any(|alias| contains_ignore_ascii_case(alias, query))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActionsUpdate {
    Ignored,
    Changed,
    Dismiss,
    Trigger(Action),
}

#[derive(Debug)]
pub(super) struct ActionsPalette {
    query: String,
    selected: usize,
    matches: Vec<Action>,
}

impl Default for ActionsPalette {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionsPalette {
    pub(super) fn new() -> Self {
        Self {
            query: String::new(),
            selected: 0,
            matches: ACTIONS.to_vec(),
        }
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn matched_actions(&self) -> &[Action] {
        &self.matches
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        (!self.matches.is_empty()).then_some(self.selected)
    }

    pub(super) fn selected_action(&self) -> Option<Action> {
        self.matches.get(self.selected).copied()
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> ActionsUpdate {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ActionsUpdate::Ignored;
        }

        match key.code {
            KeyCode::Esc => ActionsUpdate::Dismiss,
            KeyCode::Backspace if self.query.is_empty() => ActionsUpdate::Dismiss,
            KeyCode::Backspace => {
                self.remove_last_grapheme();
                ActionsUpdate::Changed
            }
            KeyCode::Enter | KeyCode::Tab => self
                .selected_action()
                .map_or(ActionsUpdate::Ignored, ActionsUpdate::Trigger),
            KeyCode::Up => {
                self.select_previous();
                ActionsUpdate::Changed
            }
            KeyCode::Down => {
                self.select_next();
                ActionsUpdate::Changed
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.refresh_matches();
                ActionsUpdate::Changed
            }
            _ => ActionsUpdate::Ignored,
        }
    }

    pub(super) fn insert_paste(&mut self, text: &str) -> ActionsUpdate {
        let original_len = self.query.len();
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        if self.query.len() == original_len {
            return ActionsUpdate::Ignored;
        }
        self.refresh_matches();
        ActionsUpdate::Changed
    }

    fn remove_last_grapheme(&mut self) {
        let Some((index, _)) = self.query.grapheme_indices(true).next_back() else {
            return;
        };
        self.query.truncate(index);
        self.refresh_matches();
    }

    fn refresh_matches(&mut self) {
        self.matches.clear();
        self.matches.extend(
            ACTIONS
                .into_iter()
                .filter(|action| action.matches(&self.query)),
        );
        self.selected = 0;
    }

    const fn select_previous(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
        }
    }
}

fn contains_ignore_ascii_case(value: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if query.len() > value.len() {
        return false;
    }
    value
        .as_bytes()
        .windows(query.len())
        .any(|window| window.eq_ignore_ascii_case(query.as_bytes()))
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::{Action, ActionsPalette, ActionsUpdate};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_query(palette: &mut ActionsPalette, query: &str) {
        for character in query.chars() {
            assert_eq!(
                palette.handle_key(key(KeyCode::Char(character))),
                ActionsUpdate::Changed
            );
        }
    }

    #[test]
    fn starts_with_every_action_and_the_first_selected() {
        let palette = ActionsPalette::new();

        assert_eq!(palette.matched_actions().len(), 12);
        assert_eq!(palette.selected_index(), Some(0));
        assert_eq!(palette.selected_action(), Some(Action::Reasoning));
    }

    #[test]
    fn aliases_filter_case_insensitively_and_reset_selection() {
        let mut palette = ActionsPalette::new();
        palette.handle_key(key(KeyCode::Down));
        palette.handle_key(key(KeyCode::Down));
        type_query(&mut palette, "JAEGER");

        assert_eq!(palette.query(), "JAEGER");
        assert_eq!(palette.matched_actions(), [Action::Trace]);
        assert_eq!(palette.selected_index(), Some(0));
    }

    #[test]
    fn arrows_clamp_and_enter_or_tab_trigger_the_selection() {
        let mut palette = ActionsPalette::new();

        palette.handle_key(key(KeyCode::Up));
        assert_eq!(palette.selected_action(), Some(Action::Reasoning));
        palette.handle_key(key(KeyCode::Down));
        assert_eq!(
            palette.handle_key(key(KeyCode::Enter)),
            ActionsUpdate::Trigger(Action::FastMode)
        );
        assert_eq!(
            palette.handle_key(key(KeyCode::Tab)),
            ActionsUpdate::Trigger(Action::FastMode)
        );

        for _ in 0..20 {
            palette.handle_key(key(KeyCode::Down));
        }
        assert_eq!(palette.selected_action(), Some(Action::CloseBtw));
    }

    #[test]
    fn backspace_edits_before_dismissing_and_removes_a_grapheme() {
        let mut palette = ActionsPalette::new();
        palette.insert_paste("e\u{301}");

        assert_eq!(
            palette.handle_key(key(KeyCode::Backspace)),
            ActionsUpdate::Changed
        );
        assert_eq!(palette.query(), "");
        assert_eq!(
            palette.handle_key(key(KeyCode::Backspace)),
            ActionsUpdate::Dismiss
        );
        assert_eq!(
            palette.handle_key(key(KeyCode::Esc)),
            ActionsUpdate::Dismiss
        );
    }

    #[test]
    fn no_match_cannot_be_triggered() {
        let mut palette = ActionsPalette::new();
        type_query(&mut palette, "not an action");

        assert!(palette.matched_actions().is_empty());
        assert_eq!(palette.selected_index(), None);
        assert_eq!(
            palette.handle_key(key(KeyCode::Enter)),
            ActionsUpdate::Ignored
        );
    }

    #[test]
    fn paste_ignores_control_characters() {
        let mut palette = ActionsPalette::new();

        assert_eq!(
            palette.insert_paste("tool\n details\t"),
            ActionsUpdate::Changed
        );
        assert_eq!(palette.query(), "tool details");
        assert_eq!(palette.matched_actions(), [Action::GlobalToolDetails]);
        assert_eq!(palette.insert_paste("\n\t"), ActionsUpdate::Ignored);
    }

    #[test]
    fn control_and_alt_characters_are_not_queries() {
        let mut palette = ActionsPalette::new();

        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            assert_eq!(
                palette.handle_key(KeyEvent::new(KeyCode::Char('x'), modifiers)),
                ActionsUpdate::Ignored
            );
        }
        assert_eq!(palette.query(), "");
    }

    #[test]
    fn key_release_is_ignored() {
        let mut palette = ActionsPalette::new();
        let mut event = key(KeyCode::Down);
        event.kind = KeyEventKind::Release;

        assert_eq!(palette.handle_key(event), ActionsUpdate::Ignored);
        assert_eq!(palette.selected_index(), Some(0));
    }

    #[test]
    fn every_action_has_searchable_aliases() {
        for action in super::ACTIONS {
            for alias in action.aliases() {
                let mut palette = ActionsPalette::new();
                type_query(&mut palette, alias);
                assert!(
                    palette.matched_actions().contains(&action),
                    "{action:?} was not searchable by {alias:?}"
                );
            }
        }
    }
}
