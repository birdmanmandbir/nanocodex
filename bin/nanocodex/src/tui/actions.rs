// Searchable action palette for the interactive TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const ACTIONS: [Action; 11] = [
    Action::Reasoning,
    Action::FastMode,
    Action::Btw,
    Action::Branches,
    Action::ToolDetails,
    Action::Cancel,
    Action::Trace,
    Action::CloseBtw,
    Action::McpLogin,
    Action::McpReload,
    Action::Keybindings,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Action {
    Reasoning,
    FastMode,
    Btw,
    Branches,
    ToolDetails,
    Cancel,
    Trace,
    CloseBtw,
    McpLogin,
    McpReload,
    Keybindings,
}

#[derive(Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent action availability toggles are not one state machine"
)]
pub(super) struct ActionContext {
    pub(super) fast_mode: bool,
    pub(super) btw_open: bool,
    pub(super) btw_busy: bool,
    pub(super) can_browse_branches: bool,
    pub(super) can_cancel: bool,
    pub(super) trace_available: bool,
    pub(super) tool_details_expanded: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ActionMenuResult {
    Handled,
    Dismiss,
    Trigger(Action),
    SubmitLiteral(String),
}

pub(super) struct ActionMenu {
    query: String,
    matches: Vec<usize>,
    selected: usize,
}

impl Default for ActionMenu {
    fn default() -> Self {
        Self {
            query: String::new(),
            matches: (0..ACTIONS.len()).collect(),
            selected: 0,
        }
    }
}

impl ActionMenu {
    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn visible_actions(&self) -> impl Iterator<Item = Action> + '_ {
        self.matches.iter().map(|index| ACTIONS[*index])
    }

    pub(super) fn selected(&self) -> Option<usize> {
        (!self.matches.is_empty()).then_some(self.selected)
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent, context: ActionContext) -> ActionMenuResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ActionMenuResult::Handled;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Enter {
            return ActionMenuResult::SubmitLiteral(format!("/{}", self.query));
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return ActionMenuResult::Handled;
        }

        match key.code {
            KeyCode::Esc => ActionMenuResult::Dismiss,
            KeyCode::Backspace if self.query.is_empty() => ActionMenuResult::Dismiss,
            KeyCode::Backspace => {
                if let Some((index, _)) = self.query.char_indices().next_back() {
                    self.query.truncate(index);
                    self.refresh_matches();
                }
                ActionMenuResult::Handled
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ActionMenuResult::Handled
            }
            KeyCode::Down => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.matches.len().saturating_sub(1));
                ActionMenuResult::Handled
            }
            KeyCode::Enter | KeyCode::Tab => self.trigger_selected(context),
            KeyCode::Char(character) => {
                self.query.push(character);
                self.refresh_matches();
                ActionMenuResult::Handled
            }
            _ => ActionMenuResult::Handled,
        }
    }

    pub(super) fn handle_paste(&mut self, text: &str) {
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.refresh_matches();
    }

    fn trigger_selected(&self, context: ActionContext) -> ActionMenuResult {
        let Some(index) = self.matches.get(self.selected) else {
            return ActionMenuResult::SubmitLiteral(format!("/{}", self.query));
        };
        let action = ACTIONS[*index];
        if action.enabled(context) {
            ActionMenuResult::Trigger(action)
        } else {
            ActionMenuResult::Handled
        }
    }

    fn refresh_matches(&mut self) {
        self.matches.clear();
        self.matches.extend(
            ACTIONS
                .iter()
                .enumerate()
                .filter(|(_, action)| action.matches(&self.query))
                .map(|(index, _)| index),
        );
        self.selected = 0;
    }
}

impl Action {
    pub(super) const fn command(self) -> Option<&'static str> {
        match self {
            Self::Reasoning => Some("/model"),
            Self::FastMode => Some("/fast"),
            Self::Btw => Some("/btw"),
            Self::Cancel => Some("/cancel"),
            Self::Trace => Some("/trace"),
            Self::CloseBtw => Some("/close"),
            Self::McpLogin => Some("/mcp login "),
            Self::McpReload => Some("/mcp reload "),
            Self::Branches | Self::ToolDetails | Self::Keybindings => None,
        }
    }

    pub(super) const fn label(self, context: ActionContext) -> &'static str {
        match self {
            Self::Reasoning => "Change reasoning",
            Self::FastMode if context.fast_mode => "Disable fast mode",
            Self::FastMode => "Enable fast mode",
            Self::Btw if context.btw_open => "Focus side question",
            Self::Btw => "Open side question",
            Self::Branches if !context.can_browse_branches => {
                "Browse branches · no alternate branches"
            }
            Self::Branches => "Browse branches",
            Self::ToolDetails if context.tool_details_expanded => "Collapse tool details",
            Self::ToolDetails => "Expand tool details",
            Self::Cancel if !context.can_cancel => "Cancel focused turn · idle",
            Self::Cancel => "Cancel focused turn",
            Self::Trace if !context.trace_available => "Open session trace · fork not ready",
            Self::Trace => "Open session trace",
            Self::CloseBtw if !context.btw_open => "Close side pane · not open",
            Self::CloseBtw if context.btw_busy => "Close side pane · finish active work first",
            Self::CloseBtw => "Close side pane",
            Self::McpLogin => "Log in to MCP server…",
            Self::McpReload => "Reload MCP server…",
            Self::Keybindings => "Keyboard shortcuts",
        }
    }

    pub(super) const fn enabled(self, context: ActionContext) -> bool {
        match self {
            Self::Branches => context.can_browse_branches,
            Self::Cancel => context.can_cancel,
            Self::Trace => context.trace_available,
            Self::CloseBtw => context.btw_open && !context.btw_busy,
            Self::Reasoning
            | Self::FastMode
            | Self::Btw
            | Self::ToolDetails
            | Self::McpLogin
            | Self::McpReload
            | Self::Keybindings => true,
        }
    }

    fn matches(self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        self.search_terms()
            .iter()
            .any(|term| contains_ignore_ascii_case(term, query))
    }

    const fn search_terms(self) -> &'static [&'static str] {
        match self {
            Self::Reasoning => &["change reasoning", "model", "thinking", "/model"],
            Self::FastMode => &["fast mode", "priority", "/fast"],
            Self::Btw => &["side question", "fork", "btw", "/btw"],
            Self::Branches => &["browse branches", "history", "tree"],
            Self::ToolDetails => &["tool details", "fold", "expand", "collapse"],
            Self::Cancel => &["cancel focused turn", "stop", "interrupt", "/cancel"],
            Self::Trace => &["open session trace", "jaeger", "telemetry", "/trace"],
            Self::CloseBtw => &["close side pane", "dismiss", "/close"],
            Self::McpLogin => &["mcp login", "authenticate", "/mcp login"],
            Self::McpReload => &["mcp reload", "refresh", "/mcp reload"],
            Self::Keybindings => &["keyboard shortcuts", "keys", "help"],
        }
    }
}

fn contains_ignore_ascii_case(value: &str, query: &str) -> bool {
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Action, ActionContext, ActionMenu, ActionMenuResult};

    fn context() -> ActionContext {
        ActionContext {
            fast_mode: false,
            btw_open: false,
            btw_busy: false,
            can_browse_branches: false,
            can_cancel: false,
            trace_available: true,
            tool_details_expanded: true,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn search_matches_labels_commands_and_aliases() {
        let mut menu = ActionMenu::default();
        for character in "thinking".chars() {
            assert_eq!(
                menu.handle_key(key(KeyCode::Char(character)), context()),
                ActionMenuResult::Handled
            );
        }
        assert_eq!(
            menu.handle_key(key(KeyCode::Enter), context()),
            ActionMenuResult::Trigger(Action::Reasoning)
        );
    }

    #[test]
    fn unavailable_actions_are_visible_but_cannot_trigger() {
        let mut menu = ActionMenu::default();
        for character in "branches".chars() {
            let _ = menu.handle_key(key(KeyCode::Char(character)), context());
        }
        assert_eq!(
            menu.handle_key(key(KeyCode::Enter), context()),
            ActionMenuResult::Handled
        );
    }

    #[test]
    fn unknown_slash_input_can_still_be_submitted_to_the_model() {
        let mut menu = ActionMenu::default();
        for character in "review-pr".chars() {
            let _ = menu.handle_key(key(KeyCode::Char(character)), context());
        }
        assert_eq!(
            menu.handle_key(key(KeyCode::Enter), context()),
            ActionMenuResult::SubmitLiteral("/review-pr".to_owned())
        );
    }

    #[test]
    fn control_enter_forces_the_search_text_through_the_normal_submission_path() {
        let mut menu = ActionMenu::default();
        for character in "trace notes".chars() {
            let _ = menu.handle_key(key(KeyCode::Char(character)), context());
        }
        assert_eq!(
            menu.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
                context()
            ),
            ActionMenuResult::SubmitLiteral("/trace notes".to_owned())
        );
    }
}
