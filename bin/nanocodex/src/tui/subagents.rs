//! Read-only TUI projection of the child-agent update stream.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::subagents::{
    AgentDescriptor, AgentId, AgentMessageUpdate, AgentStatus, AgentUpdate, MessageDeliveryState,
    MessageSender, ScopedAgentUpdate,
};
use crossterm::event::{KeyCode, KeyEvent};
use nanocodex::agent::events::{
    AgentEvent, AgentEventData, AssistantEvent, ReasoningEvent, RunEvent, ToolEvent,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap},
};

const MAX_AGENT_SUMMARIES: usize = 256;
const MAX_SUMMARY_CHARS: usize = 2_000;
const DEFAULT_PAGE_SIZE: usize = 12;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum AgentFilter {
    #[default]
    Active,
    All,
}

struct AgentNode {
    descriptor: AgentDescriptor,
    status: AgentStatus,
    summaries: VecDeque<String>,
}

/// Bounded, presentation-only projection of one root session's child-agent updates.
pub(crate) struct SubagentViewModel {
    root_session_id: String,
    max_concurrency: usize,
    nodes: HashMap<AgentId, AgentNode>,
    selected: Option<AgentId>,
    filter: AgentFilter,
    open: bool,
    inspecting: bool,
    inspect_scroll: usize,
    page_size: usize,
    missed_updates: u64,
}

impl SubagentViewModel {
    pub(crate) fn new(root_session_id: String, max_concurrency: usize) -> Self {
        Self {
            root_session_id,
            max_concurrency,
            nodes: HashMap::new(),
            selected: None,
            filter: AgentFilter::Active,
            open: false,
            inspecting: false,
            inspect_scroll: 0,
            page_size: DEFAULT_PAGE_SIZE,
            missed_updates: 0,
        }
    }

    pub(crate) fn apply_update(&mut self, update: &ScopedAgentUpdate) {
        if update.root_session_id != self.root_session_id {
            return;
        }
        match &update.update {
            AgentUpdate::Added(descriptor) => {
                let previous = self.nodes.remove(&descriptor.id);
                self.nodes.insert(
                    descriptor.id,
                    AgentNode {
                        descriptor: descriptor.clone(),
                        status: previous
                            .as_ref()
                            .map_or(AgentStatus::Pending, |node| node.status.clone()),
                        summaries: previous.map_or_else(VecDeque::new, |node| node.summaries),
                    },
                );
                self.normalize_selection();
            }
            AgentUpdate::Status { id, status } => {
                if let Some(node) = self.nodes.get_mut(id) {
                    node.status = status.clone();
                }
                self.normalize_selection();
            }
            AgentUpdate::Event { id, event } => {
                if let Some(node) = self.nodes.get_mut(id) {
                    push_summary(&mut node.summaries, summarize_event(event));
                }
            }
            AgentUpdate::Message(message) => self.apply_message(message),
        }
    }

    pub(crate) const fn note_missed_updates(&mut self, count: u64) {
        self.missed_updates = self.missed_updates.saturating_add(count);
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc if self.inspecting => {
                self.inspecting = false;
                self.inspect_scroll = 0;
            }
            KeyCode::Esc => {
                self.close();
            }
            KeyCode::Char('f') if !self.inspecting => {
                self.filter = match self.filter {
                    AgentFilter::Active => AgentFilter::All,
                    AgentFilter::All => AgentFilter::Active,
                };
                self.normalize_selection();
            }
            KeyCode::Enter if self.selected.is_some() => {
                self.inspecting = true;
                self.inspect_scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') if self.inspecting => {
                self.inspect_scroll = self.inspect_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.inspecting => {
                self.inspect_scroll = self.inspect_scroll.saturating_add(1);
            }
            KeyCode::PageUp if self.inspecting => {
                self.inspect_scroll = self.inspect_scroll.saturating_sub(self.page_size);
            }
            KeyCode::PageDown if self.inspecting => {
                self.inspect_scroll = self.inspect_scroll.saturating_add(self.page_size);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-saturating_isize(self.page_size)),
            KeyCode::PageDown => self.move_selection(saturating_isize(self.page_size)),
            _ => {}
        }
    }

    pub(crate) fn open(&mut self) {
        self.open = true;
        self.normalize_selection();
    }

    pub(crate) const fn close(&mut self) {
        self.open = false;
        self.inspecting = false;
        self.inspect_scroll = 0;
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn active_count(&self) -> usize {
        self.nodes
            .values()
            .filter(|node| node.status.is_active())
            .count()
    }

    #[cfg(test)]
    pub(crate) const fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    #[cfg(test)]
    pub(crate) const fn filter(&self) -> AgentFilter {
        self.filter
    }

    #[cfg(test)]
    pub(crate) const fn is_inspecting(&self) -> bool {
        self.inspecting
    }

    #[cfg(test)]
    pub(crate) const fn selected_agent(&self) -> Option<AgentId> {
        self.selected
    }

    #[cfg(test)]
    pub(crate) fn visible_len(&self) -> usize {
        self.visible_rows().len()
    }

    fn apply_message(&mut self, message: &AgentMessageUpdate) {
        let delivery = match &message.delivery {
            MessageDeliveryState::Admitted { disposition } => format!("admitted ({disposition:?})"),
            MessageDeliveryState::Delivered { disposition } => {
                format!("delivered ({disposition:?})")
            }
            MessageDeliveryState::Failed { error } => format!("failed: {error}"),
        };
        let routed = message
            .thread
            .messages
            .iter()
            .find(|candidate| candidate.id == message.message_id);
        let Some(routed) = routed else {
            return;
        };
        let body = bounded_text(&routed.body);
        if let Some(node) = self.nodes.get_mut(&routed.to) {
            push_summary(
                &mut node.summaries,
                format!(
                    "message in [{}] {delivery}: {body}",
                    routed.purpose.as_str()
                ),
            );
        }
        if let MessageSender::Agent { agent_id } = routed.from
            && agent_id != routed.to
            && let Some(node) = self.nodes.get_mut(&agent_id)
        {
            push_summary(
                &mut node.summaries,
                format!(
                    "message out [{}] {delivery}: {body}",
                    routed.purpose.as_str()
                ),
            );
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let ids = self
            .visible_rows()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| ids.iter().position(|id| *id == selected))
            .unwrap_or_default();
        let next = current
            .saturating_add_signed(delta)
            .min(ids.len().saturating_sub(1));
        self.selected = Some(ids[next]);
    }

    fn normalize_selection(&mut self) {
        let rows = self.visible_rows();
        if !rows.iter().any(|(id, _)| Some(*id) == self.selected) {
            self.selected = rows.first().map(|(id, _)| *id);
            self.inspecting &= self.selected.is_some();
            self.inspect_scroll = 0;
        }
    }

    fn visible_rows(&self) -> Vec<(AgentId, usize)> {
        let shown = self.shown_ids();
        let mut roots = shown
            .iter()
            .copied()
            .filter(|id| {
                self.nodes
                    .get(id)
                    .and_then(|node| node.descriptor.parent)
                    .is_none_or(|parent| !shown.contains(&parent))
            })
            .collect::<Vec<_>>();
        roots.sort_unstable();

        let mut rows = Vec::with_capacity(shown.len());
        let mut visited = HashSet::with_capacity(shown.len());
        for root in roots {
            self.append_visible(root, 0, &shown, &mut visited, &mut rows);
        }
        // Malformed cycles should not make agents disappear from the projection.
        let mut remaining = shown.difference(&visited).copied().collect::<Vec<_>>();
        remaining.sort_unstable();
        for id in remaining {
            self.append_visible(id, 0, &shown, &mut visited, &mut rows);
        }
        rows
    }

    fn shown_ids(&self) -> HashSet<AgentId> {
        if self.filter == AgentFilter::All {
            return self.nodes.keys().copied().collect();
        }
        let mut shown = HashSet::new();
        for (&id, node) in &self.nodes {
            if !node.status.is_active() {
                continue;
            }
            let mut cursor = Some(id);
            while let Some(current) = cursor {
                if !shown.insert(current) {
                    break;
                }
                cursor = self
                    .nodes
                    .get(&current)
                    .and_then(|current| current.descriptor.parent);
            }
        }
        shown
    }

    fn append_visible(
        &self,
        id: AgentId,
        depth: usize,
        shown: &HashSet<AgentId>,
        visited: &mut HashSet<AgentId>,
        rows: &mut Vec<(AgentId, usize)>,
    ) {
        if !visited.insert(id) {
            return;
        }
        rows.push((id, depth));
        let mut children = shown
            .iter()
            .copied()
            .filter(|child| {
                self.nodes
                    .get(child)
                    .is_some_and(|node| node.descriptor.parent == Some(id))
            })
            .collect::<Vec<_>>();
        children.sort_unstable();
        for child in children {
            self.append_visible(child, depth.saturating_add(1), shown, visited, rows);
        }
    }

    fn transcript_lines(&self) -> Vec<Line<'_>> {
        let Some(node) = self.selected.and_then(|id| self.nodes.get(&id)) else {
            return vec![Line::raw("No child agent selected.")];
        };
        let mut lines = vec![
            Line::raw(format!("Role: {}", bounded_text(&node.descriptor.role))),
            Line::raw(format!("Task: {}", bounded_text(&node.descriptor.task))),
            Line::raw(format!("Status: {}", status_label(&node.status))),
            Line::raw(""),
        ];
        if node.summaries.is_empty() {
            lines.push(Line::raw("No child events yet."));
        } else {
            lines.extend(
                node.summaries
                    .iter()
                    .map(|summary| Line::raw(summary.as_str())),
            );
        }
        lines
    }
}

pub(crate) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &mut SubagentViewModel,
    theme: &super::theme::Theme,
) {
    let block = Block::default()
        .title(" Subagents ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [header, content, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let filter = match model.filter {
        AgentFilter::Active => "active",
        AgentFilter::All => "all",
    };
    let lag = if model.missed_updates > 0 {
        format!(" · {} updates skipped", model.missed_updates)
    } else {
        String::new()
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(
                    "  {}/{} active · {} total",
                    model.active_count(),
                    model.max_concurrency,
                    model.nodes.len()
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("  showing {filter}{lag}"),
                Style::default().fg(if model.missed_updates > 0 {
                    theme.warning()
                } else {
                    theme.muted()
                }),
            ),
        ]),
        header,
    );

    if model.inspecting {
        model.page_size = usize::from(content.height).max(1);
        frame.render_widget(
            Paragraph::new(model.transcript_lines())
                .scroll((saturating_u16(model.inspect_scroll), 0))
                .wrap(Wrap { trim: false }),
            content,
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  ↑/↓ pgup/pgdn", Style::default().fg(theme.accent())),
                Span::raw(" scroll · "),
                Span::styled("esc", Style::default().fg(theme.accent())),
                Span::raw(" tree"),
            ])),
            footer,
        );
        return;
    }

    model.page_size = usize::from(content.height).max(1);
    let items = model
        .visible_rows()
        .into_iter()
        .filter_map(|(id, depth)| {
            let node = model.nodes.get(&id)?;
            let selected = model.selected == Some(id);
            let marker = if selected { "›" } else { " " };
            let branch = if depth == 0 { "" } else { "└ " };
            let indent = "  ".repeat(depth);
            let style = if selected {
                Style::default().fg(theme.accent())
            } else if node.status.is_active() {
                Style::default().fg(theme.text())
            } else {
                Style::default().fg(theme.muted())
            };
            Some(ListItem::new(Line::styled(
                format!(
                    "{marker} {indent}{branch}#{} {} · {}",
                    id,
                    bounded_text(&node.descriptor.role),
                    status_label(&node.status)
                ),
                style,
            )))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), content);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ↑/↓", Style::default().fg(theme.accent())),
            Span::raw(" select · "),
            Span::styled("enter", Style::default().fg(theme.accent())),
            Span::raw(" inspect · "),
            Span::styled("f", Style::default().fg(theme.accent())),
            Span::raw(" filter · "),
            Span::styled("esc", Style::default().fg(theme.accent())),
            Span::raw(" close"),
        ])),
        footer,
    );
}

fn push_summary(summaries: &mut VecDeque<String>, summary: String) {
    if summaries.len() == MAX_AGENT_SUMMARIES {
        summaries.pop_front();
    }
    summaries.push_back(bounded_text(&summary));
}

fn summarize_event(event: &AgentEvent) -> String {
    match event.data() {
        Ok(AgentEventData::Assistant(AssistantEvent::Delta(delta))) => {
            format!("assistant: {}", delta.text)
        }
        Ok(AgentEventData::Assistant(AssistantEvent::Message(message))) => {
            format!("assistant (complete): {}", message.text)
        }
        Ok(AgentEventData::Reasoning(ReasoningEvent::SummaryDelta(delta))) => {
            format!("reasoning: {}", delta.text)
        }
        Ok(AgentEventData::Run(RunEvent::Error(error))) => format!("run error: {}", error.message),
        Ok(AgentEventData::Tool(ToolEvent::Call(call))) => {
            format!("tool call: {} {}", call.tool, call.arguments.get())
        }
        Ok(AgentEventData::Tool(ToolEvent::Result(result))) => {
            format!("tool result: {} ({:?})", result.tool, result.status)
        }
        Ok(_) => format!("event: {:?}", event.kind),
        Err(error) => format!("event: {:?} (invalid payload: {error})", event.kind),
    }
}

fn bounded_text(value: &str) -> String {
    let mut text = String::with_capacity(value.len().min(MAX_SUMMARY_CHARS));
    let mut chars = value.chars();
    for character in chars.by_ref().take(MAX_SUMMARY_CHARS) {
        match character {
            '\r' => text.push('\n'),
            '\n' | '\t' => text.push(character),
            character if character.is_control() => text.push('�'),
            character => text.push(character),
        }
    }
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

const fn status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Pending => "pending",
        AgentStatus::Running => "running",
        AgentStatus::Completed { .. } => "completed",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Failed { .. } => "failed",
        AgentStatus::Closing => "closing",
        AgentStatus::Closed => "closed",
    }
}

fn saturating_isize(value: usize) -> isize {
    isize::try_from(value).unwrap_or(isize::MAX)
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use serde_json::{json, value::to_raw_value};

    use super::{AgentFilter, DEFAULT_PAGE_SIZE, MAX_AGENT_SUMMARIES, SubagentViewModel};
    use crate::subagents::{AgentDescriptor, AgentId, AgentStatus, AgentUpdate, ScopedAgentUpdate};

    fn descriptor(id: u64, parent: Option<u64>) -> AgentDescriptor {
        AgentDescriptor {
            id: AgentId::new(id),
            session_id: format!("session-{id}"),
            role: format!("role-{id}"),
            task: format!("task-{id}"),
            parent: parent.map(AgentId::new),
        }
    }

    fn apply(model: &mut SubagentViewModel, update: AgentUpdate) {
        model.apply_update(&ScopedAgentUpdate {
            root_session_id: "root".to_owned(),
            update,
        });
    }

    fn assistant_delta(seq: u64, text: String) -> AgentEvent {
        AgentEvent {
            protocol_version: 1,
            request_id: Arc::from("child"),
            seq,
            kind: AgentEventKind::AssistantDelta,
            payload: Arc::from(
                to_raw_value(&json!({
                    "model_call_index": 1,
                    "item_id": null,
                    "phase": null,
                    "text": text,
                }))
                .unwrap(),
            ),
        }
    }

    #[test]
    fn active_filter_retains_inactive_ancestors_for_tree_context() {
        let mut model = SubagentViewModel::new("root".to_owned(), 8);
        apply(&mut model, AgentUpdate::Added(descriptor(1, None)));
        apply(&mut model, AgentUpdate::Added(descriptor(2, Some(1))));
        apply(&mut model, AgentUpdate::Added(descriptor(3, None)));
        apply(
            &mut model,
            AgentUpdate::Status {
                id: AgentId::new(1),
                status: AgentStatus::Completed {
                    output: json!(null),
                },
            },
        );
        apply(
            &mut model,
            AgentUpdate::Status {
                id: AgentId::new(3),
                status: AgentStatus::Closed,
            },
        );

        assert_eq!(
            model.visible_rows(),
            [(AgentId::new(1), 0), (AgentId::new(2), 1)]
        );
        assert_eq!(model.active_count(), 1);
        assert_eq!(model.max_concurrency(), 8);

        model.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(model.filter(), AgentFilter::All);
        assert_eq!(model.visible_len(), 3);
    }

    #[test]
    fn navigation_and_inspection_keys_are_mode_specific() {
        let mut model = SubagentViewModel::new("root".to_owned(), 2);
        apply(&mut model, AgentUpdate::Added(descriptor(1, None)));
        apply(&mut model, AgentUpdate::Added(descriptor(2, None)));
        assert!(!model.is_open());
        model.open();
        assert!(model.is_open());

        model.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(model.selected_agent(), Some(AgentId::new(2)));
        model.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(model.is_inspecting());
        model.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(model.inspect_scroll, DEFAULT_PAGE_SIZE);
        model.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!model.is_inspecting());
        model.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!model.is_open());
    }

    #[test]
    fn child_transcript_is_readable_and_strictly_bounded() {
        let mut model = SubagentViewModel::new("root".to_owned(), 1);
        apply(&mut model, AgentUpdate::Added(descriptor(1, None)));
        for seq in 0..MAX_AGENT_SUMMARIES + 4 {
            apply(
                &mut model,
                AgentUpdate::Event {
                    id: AgentId::new(1),
                    event: assistant_delta(seq as u64, format!("answer-{seq}")),
                },
            );
        }

        let node = &model.nodes[&AgentId::new(1)];
        assert_eq!(node.summaries.len(), MAX_AGENT_SUMMARIES);
        let transcript = model
            .transcript_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!transcript.contains("answer-0\n"));
        assert!(transcript.contains("assistant: answer-259"));
        assert!(transcript.contains("Role: role-1"));
    }

    #[test]
    fn updates_from_other_root_sessions_are_ignored() {
        let mut model = SubagentViewModel::new("root".to_owned(), 1);
        model.apply_update(&ScopedAgentUpdate {
            root_session_id: "other".to_owned(),
            update: AgentUpdate::Added(descriptor(1, None)),
        });
        assert_eq!(model.visible_len(), 0);
    }
}
