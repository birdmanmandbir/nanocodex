// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Read-only inspection and explicit deletion of account-owned durable memories.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{
    client::{MemoryKey, MemoryRecord},
    tui::{session::format_age, theme::Theme},
};
use crossterm_tact::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_tact::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const LIST_KEYS: [(&str, &str); 6] = [
    ("↑↓", "move"),
    ("enter", "inspect"),
    ("ctrl+s", "sort"),
    ("ctrl+d", "remove"),
    ("ctrl+r", "refresh"),
    ("esc", "close"),
];
const DETAIL_KEYS: [(&str, &str); 4] = [
    ("↑↓/pgup/pgdn", "scroll"),
    ("d", "delete"),
    ("r", "refresh"),
    ("esc", "back"),
];
const CONFIRM_KEYS: [(&str, &str); 2] = [("d/delete", "confirm"), ("esc", "cancel")];
const DELETING_KEYS: [(&str, &str); 1] = [("", "deleting…")];
const LOAD_ERROR_KEYS: [(&str, &str); 2] = [("r", "retry"), ("esc", "close")];
const DELETE_ERROR_KEYS: [(&str, &str); 3] =
    [("d/delete", "retry"), ("r", "refresh"), ("esc", "back")];
const LOADING_KEYS: [(&str, &str); 2] = [("r", "retry"), ("esc", "close")];
const FILTER_LABEL: &str = " Filter: ";
const MAX_PREVIEW_GRAPHEMES: usize = 160;
const MAX_ERROR_WIDTH: usize = 240;

pub(super) enum MemoryBrowserEvent {
    Terminal(Event),
    Loaded { records: Vec<MemoryRecord> },
    LoadFailed { error: String },
    Deleted { key: MemoryKey },
    DeleteFailed { error: String, conflict: bool },
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum MemoryBrowserEffect {
    Dismiss,
    Refresh,
    Delete(MemoryKey),
}

pub(super) struct MemoryBrowser {
    records: Vec<MemoryRecord>,
    query: String,
    matches: Vec<usize>,
    selected_key: Option<MemoryKey>,
    sort: SortMode,
    state: BrowserState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortMode {
    MostUsed,
    Newest,
    Oldest,
    LeastUsed,
}

impl SortMode {
    const fn next(self) -> Self {
        match self {
            Self::MostUsed => Self::Newest,
            Self::Newest => Self::Oldest,
            Self::Oldest => Self::LeastUsed,
            Self::LeastUsed => Self::MostUsed,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::MostUsed => "Most used",
            Self::Newest => "Newest",
            Self::Oldest => "Oldest",
            Self::LeastUsed => "Least used",
        }
    }

    fn compare(self, left: &MemoryRecord, right: &MemoryRecord) -> std::cmp::Ordering {
        match self {
            Self::MostUsed => right
                .use_count
                .cmp(&left.use_count)
                .then_with(|| compare_newest(left, right)),
            Self::Newest => compare_newest(left, right),
            Self::Oldest => compare_oldest(left, right),
            Self::LeastUsed => left
                .use_count
                .cmp(&right.use_count)
                .then_with(|| compare_oldest(left, right)),
        }
    }
}

#[derive(Clone)]
enum BrowserState {
    Loading,
    Error(BrowserError),
    List,
    Detail {
        key: MemoryKey,
        scroll: u16,
    },
    ConfirmDelete {
        key: MemoryKey,
        return_to: ReturnView,
    },
    Deleting {
        key: MemoryKey,
        return_to: ReturnView,
    },
}

#[derive(Clone)]
struct BrowserError {
    message: String,
    action: ErrorAction,
}

#[derive(Clone)]
enum ErrorAction {
    Load,
    Delete {
        key: MemoryKey,
        return_to: ReturnView,
    },
}

#[derive(Clone, Copy)]
enum ReturnView {
    List,
    Detail { scroll: u16 },
}

impl MemoryBrowser {
    pub(super) const fn new() -> Self {
        Self {
            records: Vec::new(),
            query: String::new(),
            matches: Vec::new(),
            selected_key: None,
            sort: SortMode::MostUsed,
            state: BrowserState::Loading,
        }
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<MemoryBrowserEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }
        match self.state.clone() {
            BrowserState::Loading => match key.code {
                KeyCode::Esc => Self::effect(MemoryBrowserEffect::Dismiss),
                KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self.refresh(),
                _ => ComponentUpdate::none(),
            },
            BrowserState::Error(error) => self.update_error(key, error.action),
            BrowserState::List => self.update_list(key),
            BrowserState::Detail {
                key: memory,
                scroll,
            } => self.update_detail(key, memory, scroll),
            BrowserState::ConfirmDelete {
                key: memory,
                return_to,
            } => self.update_confirmation(key, memory, return_to),
            BrowserState::Deleting { .. } => ComponentUpdate::none(),
        }
    }

    fn update_error(
        &mut self,
        event: KeyEvent,
        action: ErrorAction,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        match (action, event.code) {
            (_, KeyCode::Char('r')) if event.modifiers == KeyModifiers::NONE => self.refresh(),
            (ErrorAction::Load, KeyCode::Esc) => Self::effect(MemoryBrowserEffect::Dismiss),
            (ErrorAction::Delete { return_to, .. }, KeyCode::Esc) => {
                self.restore(return_to);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            (ErrorAction::Delete { key, return_to }, KeyCode::Char('d') | KeyCode::Delete)
                if key_event_is_press(event.kind) =>
            {
                self.delete(key, return_to)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn update_list(&mut self, key: KeyEvent) -> ComponentUpdate<MemoryBrowserEffect> {
        match key.code {
            KeyCode::Esc => Self::effect(MemoryBrowserEffect::Dismiss),
            KeyCode::Up => self.move_selection(false),
            KeyCode::Down => self.move_selection(true),
            KeyCode::Enter | KeyCode::Tab => self.inspect_selected(),
            KeyCode::Backspace if !self.query.is_empty() => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                    self.refresh_matches();
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('s') if key.modifiers == KeyModifiers::CONTROL => self.cycle_sort(),
            KeyCode::Char('r') if key.modifiers == KeyModifiers::CONTROL => self.refresh(),
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                self.confirm_selected(ReturnView::List)
            }
            KeyCode::Delete if key_event_is_press(key.kind) => {
                self.confirm_selected(ReturnView::List)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.query.push(character);
                self.refresh_matches();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn update_detail(
        &mut self,
        key: KeyEvent,
        memory: MemoryKey,
        scroll: u16,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        let next_scroll = match key.code {
            KeyCode::Up => Some(scroll.saturating_sub(1)),
            KeyCode::Down => Some(scroll.saturating_add(1)),
            KeyCode::PageUp => Some(scroll.saturating_sub(10)),
            KeyCode::PageDown => Some(scroll.saturating_add(10)),
            _ => None,
        };
        if let Some(scroll) = next_scroll {
            self.state = BrowserState::Detail {
                key: memory,
                scroll,
            };
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        match key.code {
            KeyCode::Esc => {
                self.state = BrowserState::List;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self.refresh(),
            KeyCode::Char('d') | KeyCode::Delete if key_event_is_press(key.kind) => {
                self.state = BrowserState::ConfirmDelete {
                    key: memory,
                    return_to: ReturnView::Detail { scroll },
                };
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn update_confirmation(
        &mut self,
        key: KeyEvent,
        memory: MemoryKey,
        return_to: ReturnView,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        match key.code {
            KeyCode::Esc => {
                self.restore(return_to);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('d') | KeyCode::Delete if key_event_is_press(key.kind) => {
                self.delete(memory, return_to)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn insert_paste(&mut self, text: &str) -> ComponentUpdate<MemoryBrowserEffect> {
        if !matches!(self.state, BrowserState::List) {
            return ComponentUpdate::none();
        }
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.refresh_matches();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn refresh(&mut self) -> ComponentUpdate<MemoryBrowserEffect> {
        self.state = BrowserState::Loading;
        Self::effect(MemoryBrowserEffect::Refresh)
    }

    fn inspect_selected(&mut self) -> ComponentUpdate<MemoryBrowserEffect> {
        let Some(key) = self.selected_key else {
            return ComponentUpdate::none();
        };
        self.state = BrowserState::Detail { key, scroll: 0 };
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn confirm_selected(&mut self, return_to: ReturnView) -> ComponentUpdate<MemoryBrowserEffect> {
        let Some(key) = self.selected_key else {
            return ComponentUpdate::none();
        };
        self.state = BrowserState::ConfirmDelete { key, return_to };
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn delete(
        &mut self,
        key: MemoryKey,
        return_to: ReturnView,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        if !self.records.iter().any(|record| record.key == key) {
            self.state = BrowserState::List;
            self.refresh_matches();
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        self.state = BrowserState::Deleting { key, return_to };
        Self::effect(MemoryBrowserEffect::Delete(key))
    }

    fn restore(&mut self, return_to: ReturnView) {
        self.state = match return_to {
            ReturnView::List => BrowserState::List,
            ReturnView::Detail { scroll } => {
                self.selected_key
                    .map_or(BrowserState::List, |key| BrowserState::Detail {
                        key,
                        scroll,
                    })
            }
        };
    }

    fn replace_records(&mut self, records: Vec<MemoryRecord>) {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.records = records;
        self.rebuild_matches(fallback);
        self.state = BrowserState::List;
    }

    fn remove_record(&mut self, key: MemoryKey) {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.records.retain(|record| record.key != key);
        self.rebuild_matches(fallback);
        self.state = BrowserState::List;
    }

    fn refresh_matches(&mut self) {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.rebuild_matches(fallback);
    }

    fn cycle_sort(&mut self) -> ComponentUpdate<MemoryBrowserEffect> {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.sort = self.sort.next();
        self.rebuild_matches(fallback);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn rebuild_matches(&mut self, fallback: usize) {
        let query = self.query.to_lowercase();
        self.matches = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| record_matches(record, &query))
            .map(|(index, _)| index)
            .collect();
        self.matches.sort_by(|left, right| {
            self.sort
                .compare(&self.records[*left], &self.records[*right])
        });
        if self.selected_match_index().is_none() {
            self.selected_key = self
                .matches
                .get(fallback.min(self.matches.len().saturating_sub(1)))
                .map(|index| self.records[*index].key);
        }
    }

    fn selected_match_index(&self) -> Option<usize> {
        let selected = self.selected_key?;
        self.matches
            .iter()
            .position(|index| self.records[*index].key == selected)
    }

    fn move_selection(&mut self, down: bool) -> ComponentUpdate<MemoryBrowserEffect> {
        if self.matches.is_empty() {
            return ComponentUpdate::none();
        }
        let current = self.selected_match_index().unwrap_or_default();
        let next = if down {
            current.saturating_add(1).min(self.matches.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        self.selected_key = Some(self.records[self.matches[next]].key);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn effect(effect: MemoryBrowserEffect) -> ComponentUpdate<MemoryBrowserEffect> {
        ComponentUpdate {
            effects: vec![effect],
            render: RenderRequest::Immediate,
        }
    }

    fn footer(&self) -> &'static [(&'static str, &'static str)] {
        match &self.state {
            BrowserState::Loading => &LOADING_KEYS,
            BrowserState::Error(error) => match error.action {
                ErrorAction::Load => &LOAD_ERROR_KEYS,
                ErrorAction::Delete { .. } => &DELETE_ERROR_KEYS,
            },
            BrowserState::List => &LIST_KEYS,
            BrowserState::Detail { .. } => &DETAIL_KEYS,
            BrowserState::ConfirmDelete { .. } => &CONFIRM_KEYS,
            BrowserState::Deleting { .. } => &DELETING_KEYS,
        }
    }

    fn render_list(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        status: Option<String>,
    ) {
        if area.is_empty() {
            return;
        }
        let filter = Rect { height: 1, ..area };
        let sort = format!("  Sort: {}", self.sort.label());
        let query_width = usize::from(filter.width)
            .saturating_sub(FILTER_LABEL.width())
            .saturating_sub(sort.width());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(FILTER_LABEL, Style::default().fg(theme.muted())),
                Span::styled(
                    visible_tail(&self.query, query_width),
                    Style::default().fg(theme.text()),
                ),
                Span::styled(sort, Style::default().fg(theme.muted())),
            ])),
            filter,
        );
        let mut list = Rect {
            y: area.y.saturating_add(1),
            height: area.height.saturating_sub(1),
            ..area
        };
        if let Some(status) = status {
            if list.is_empty() {
                return;
            }
            let status_area = Rect { height: 1, ..list };
            frame.render_widget(
                Paragraph::new(fit_width(&status, usize::from(status_area.width)))
                    .style(Style::default().fg(theme.accent())),
                status_area,
            );
            list.y = list.y.saturating_add(1);
            list.height = list.height.saturating_sub(1);
        }
        self.render_records(frame, list, theme);
    }

    fn render_records(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        if self.records.is_empty() {
            frame.render_widget(
                Paragraph::new(" Account memory is empty. Press r to refresh.")
                    .style(Style::default().fg(theme.muted())),
                area,
            );
            return;
        }
        if self.matches.is_empty() {
            frame.render_widget(
                Paragraph::new(format!(" No memories match “{}”.", self.query))
                    .style(Style::default().fg(theme.muted())),
                area,
            );
            return;
        }
        let width = usize::from(area.width).saturating_sub(2);
        let items = self.matches.iter().map(|index| {
            let record = &self.records[*index];
            ListItem::new(vec![
                Line::from(Span::styled(
                    bounded_preview(&record.content, width),
                    Style::default()
                        .fg(theme.text())
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    fit_width(&list_metadata(record), width),
                    Style::default().fg(theme.muted()),
                )),
            ])
        });
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(theme.accent()));
        let mut state = ListState::default().with_selected(self.selected_match_index());
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_detail(
        &mut self,
        frame: &mut Frame<'_>,
        mut area: Rect,
        theme: &Theme,
        key: MemoryKey,
        requested_scroll: u16,
        status: Option<String>,
        update_scroll: bool,
    ) {
        if let Some(status) = status {
            let status_area = Rect { height: 1, ..area };
            frame.render_widget(
                Paragraph::new(fit_width(&status, usize::from(status_area.width)))
                    .style(Style::default().fg(theme.accent())),
                status_area,
            );
            area.y = area.y.saturating_add(1);
            area.height = area.height.saturating_sub(1);
        }
        let Some(record) = self.records.iter().find(|record| record.key == key) else {
            self.state = BrowserState::List;
            self.render_list(frame, area, theme, None);
            return;
        };
        let lines = detail_lines(record, theme);
        let max_scroll = wrapped_line_count(&lines, area.width)
            .saturating_sub(usize::from(area.height))
            .min(usize::from(u16::MAX)) as u16;
        let scroll = requested_scroll.min(max_scroll);
        if update_scroll {
            self.state = BrowserState::Detail { key, scroll };
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }

    fn render_error(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, error: &BrowserError) {
        let message = sanitize_single_line(&error.message, MAX_ERROR_WIDTH);
        let text = match error.action {
            ErrorAction::Load => {
                format!("Could not load memories: {message}\n\nPress r to retry or Esc to close.")
            }
            ErrorAction::Delete { key, .. } => format!(
                "Could not delete {}: {message}\n\nPress d/Delete to retry, r to reload, or Esc to return.",
                memory_label(key)
            ),
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(theme.text()))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
}

impl Component for MemoryBrowser {
    type Event = MemoryBrowserEvent;
    type Effect = MemoryBrowserEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            MemoryBrowserEvent::Terminal(Event::Key(key)) => self.update_key(key),
            MemoryBrowserEvent::Terminal(Event::Paste(text)) => self.insert_paste(&text),
            MemoryBrowserEvent::Terminal(_) => ComponentUpdate::none(),
            MemoryBrowserEvent::Loaded { records } => {
                self.replace_records(records);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::LoadFailed { error } => {
                self.state = BrowserState::Error(BrowserError {
                    message: error,
                    action: ErrorAction::Load,
                });
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::Deleted { key } => {
                self.remove_record(key);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::DeleteFailed { error, conflict } => {
                let BrowserState::Deleting { key, return_to } = self.state.clone() else {
                    return ComponentUpdate::none();
                };
                if conflict {
                    return self.refresh();
                }
                self.state = BrowserState::Error(BrowserError {
                    message: error,
                    action: ErrorAction::Delete { key, return_to },
                });
                ComponentUpdate::render(RenderRequest::Immediate)
            }
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let state = self.state.clone();
        let layout =
            Floating::new("Account memory", 88, 28, self.footer()).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        match state {
            BrowserState::Loading => frame.render_widget(
                Paragraph::new("Loading account memory…\n\nPress r to retry or Esc to close.")
                    .style(Style::default().fg(theme.muted()))
                    .wrap(Wrap { trim: false }),
                layout.body,
            ),
            BrowserState::Error(error) => self.render_error(frame, layout.body, theme, &error),
            BrowserState::List => self.render_list(frame, layout.body, theme, None),
            BrowserState::Detail { key, scroll } => {
                self.render_detail(frame, layout.body, theme, key, scroll, None, true)
            }
            BrowserState::ConfirmDelete { key, return_to } => {
                let status = format!(
                    " Delete {}? Press d/Delete again to confirm.",
                    memory_label(key)
                );
                match return_to {
                    ReturnView::List => self.render_list(frame, layout.body, theme, Some(status)),
                    ReturnView::Detail { scroll } => self.render_detail(
                        frame,
                        layout.body,
                        theme,
                        key,
                        scroll,
                        Some(status),
                        false,
                    ),
                }
            }
            BrowserState::Deleting { key, return_to } => {
                let status = format!(" Deleting {}…", memory_label(key));
                match return_to {
                    ReturnView::List => self.render_list(frame, layout.body, theme, Some(status)),
                    ReturnView::Detail { scroll } => self.render_detail(
                        frame,
                        layout.body,
                        theme,
                        key,
                        scroll,
                        Some(status),
                        false,
                    ),
                }
            }
        }
    }
}

fn key_event_is_press(kind: KeyEventKind) -> bool {
    kind == KeyEventKind::Press
}

fn record_matches(record: &MemoryRecord, query: &str) -> bool {
    query.is_empty()
        || record.key.id.to_string().contains(query)
        || record.content.to_lowercase().contains(query)
}

fn compare_newest(left: &MemoryRecord, right: &MemoryRecord) -> std::cmp::Ordering {
    right
        .updated_at_ms
        .cmp(&left.updated_at_ms)
        .then_with(|| right.key.id.cmp(&left.key.id))
}

fn compare_oldest(left: &MemoryRecord, right: &MemoryRecord) -> std::cmp::Ordering {
    left.updated_at_ms
        .cmp(&right.updated_at_ms)
        .then_with(|| left.key.id.cmp(&right.key.id))
}

fn memory_label(key: MemoryKey) -> String {
    format!("memory {}@{}", key.id, key.version)
}

fn list_metadata(record: &MemoryRecord) -> String {
    format!(
        "{} · used {}× · scanned {}× · {}",
        memory_label(record.key),
        record.use_count,
        record.scan_count,
        age(record.updated_at_ms)
    )
}

fn detail_lines(record: &MemoryRecord, theme: &Theme) -> Vec<Line<'static>> {
    let muted = Style::default().fg(theme.muted());
    let text = Style::default().fg(theme.text());
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Key: ", muted),
            Span::styled(memory_label(record.key), text),
        ]),
        Line::from(vec![
            Span::styled("Updated: ", muted),
            Span::styled(age(record.updated_at_ms), text),
            Span::styled(" · Created: ", muted),
            Span::styled(age(record.created_at_ms), text),
        ]),
        Line::from(vec![
            Span::styled("Used: ", muted),
            Span::styled(record.use_count.to_string(), text),
            Span::styled(" · Scanned: ", muted),
            Span::styled(record.scan_count.to_string(), text),
        ]),
    ];
    if let Some(until) = record.probation_until_ms {
        lines.push(Line::from(vec![
            Span::styled("Probation until: ", muted),
            Span::styled(until.to_string(), text),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        sanitize_detail(&record.content),
        Style::default().fg(theme.text()),
    ));
    lines
}

fn age(timestamp: i64) -> String {
    format_age(u64::try_from(timestamp).unwrap_or_default())
}

fn bounded_preview(content: &str, width: usize) -> String {
    let single_line = content
        .graphemes(true)
        .take(MAX_PREVIEW_GRAPHEMES)
        .flat_map(str::chars)
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    fit_width(
        &single_line.split_whitespace().collect::<Vec<_>>().join(" "),
        width,
    )
}

fn sanitize_detail(content: &str) -> String {
    content
        .chars()
        .flat_map(|character| match character {
            '\t' => "    ".chars().collect::<Vec<_>>(),
            '\n' => vec!['\n'],
            character if character.is_control() => vec!['�'],
            character => vec![character],
        })
        .collect()
}

fn sanitize_single_line(text: &str, width: usize) -> String {
    let sanitized = text
        .chars()
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    fit_width(
        &sanitized.split_whitespace().collect::<Vec<_>>().join(" "),
        width,
    )
}

fn fit_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used: usize = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme.width();
        if used.saturating_add(grapheme_width) > width.saturating_sub(1) {
            break;
        }
        result.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    result.push('…');
    result
}

fn visible_tail(query: &str, width: usize) -> &str {
    let mut used = 0;
    for (index, grapheme) in query.grapheme_indices(true).rev() {
        used += grapheme.width();
        if used > width {
            return &query[index + grapheme.len()..];
        }
    }
    query
}

fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width);
    if width == 0 {
        return 0;
    }
    lines
        .iter()
        .map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            text.lines()
                .map(|part| part.width().max(1).div_ceil(width))
                .sum::<usize>()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::{BrowserState, Component, MemoryBrowser, MemoryBrowserEffect, MemoryBrowserEvent};
    use crate::client::{MemoryKey, MemoryRecord};
    use crossterm_tact::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn record(id: u64, version: u64, content: &str) -> MemoryRecord {
        MemoryRecord {
            key: MemoryKey { id, version },
            content: content.to_owned(),
            created_at_ms: 1,
            updated_at_ms: i64::try_from(id).unwrap(),
            last_scanned_at_ms: None,
            scan_count: 0,
            last_used_at_ms: None,
            use_count: id,
            probation_until_ms: None,
        }
    }

    #[test]
    fn starts_without_a_fake_query_and_filters_after_loading() {
        let mut browser = MemoryBrowser::new();
        assert!(browser.query.is_empty());
        browser.update(MemoryBrowserEvent::Loaded {
            records: vec![record(1, 1, "copper notes"), record(2, 1, "other")],
        });
        browser.update(MemoryBrowserEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        ))));
        assert_eq!(browser.matches, vec![0]);
    }

    #[test]
    fn delete_requires_confirmation_and_keeps_versioned_key() {
        let mut browser = MemoryBrowser::new();
        browser.update(MemoryBrowserEvent::Loaded {
            records: vec![record(7, 3, "remember this")],
        });
        browser.update(MemoryBrowserEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::NONE,
        ))));
        assert!(matches!(browser.state, BrowserState::ConfirmDelete { .. }));
        let update = browser.update(MemoryBrowserEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::NONE,
        ))));
        assert_eq!(
            update.effects,
            vec![MemoryBrowserEffect::Delete(MemoryKey { id: 7, version: 3 })]
        );
    }

    #[test]
    fn optimistic_conflict_reloads_instead_of_removing_stale_record() {
        let mut browser = MemoryBrowser::new();
        browser.update(MemoryBrowserEvent::Loaded {
            records: vec![record(7, 3, "remember this")],
        });
        browser.state = BrowserState::Deleting {
            key: MemoryKey { id: 7, version: 3 },
            return_to: super::ReturnView::List,
        };
        let update = browser.update(MemoryBrowserEvent::DeleteFailed {
            error: "conflict".to_owned(),
            conflict: true,
        });
        assert_eq!(update.effects, vec![MemoryBrowserEffect::Refresh]);
        assert_eq!(browser.records.len(), 1);
    }
}
