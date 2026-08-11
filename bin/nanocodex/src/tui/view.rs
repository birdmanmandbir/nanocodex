use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph},
};
use std::time::Instant;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    app::{App, Conversation, InfoOverlay, PaneId, ReasoningPicker, STANDARD_THINKING_OPTIONS},
    composer::ComposerLayout,
    context_diagnostics::{ContextDiagnostics, ContextDiagnosticsPanel},
    floating::{Floating, FooterShortcut},
    transcript::InlineEdit,
};

const ACTIONS_FOOTER: [FooterShortcut<'static>; 3] = [
    FooterShortcut::new("↑/↓", "move"),
    FooterShortcut::new("enter", "open"),
    FooterShortcut::new("esc", "close"),
];
const REASONING_FOOTER: [FooterShortcut<'static>; 2] = [
    FooterShortcut::new("enter", "confirm"),
    FooterShortcut::new("esc", "back"),
];

pub(super) fn render(frame: &mut Frame<'_>, app: &mut App) {
    let layout = view_layout(frame.area(), app);

    render_header(frame, app, layout.header);
    let mut selectable_areas = SelectableAreas::default();
    render_transcripts(frame, app, layout.transcript, &mut selectable_areas);
    render_pending(frame, app, layout.pending);
    selectable_areas.push(render_composer(
        frame,
        app,
        layout.composer,
        &layout.composer_layout,
    ));
    render_footer(frame, app, layout.footer);
    app.render_mouse_selection(frame.buffer_mut(), selectable_areas.as_slice());
    render_overlays(frame, app);
}

pub(super) fn render_animation(frame: &mut Frame<'_>, app: &mut App) {
    let layout = view_layout(frame.area(), app);
    render_composer(frame, app, layout.composer, &layout.composer_layout);
    render_footer(frame, app, layout.footer);
    render_overlays(frame, app);
}

fn render_overlays(frame: &mut Frame<'_>, app: &mut App) {
    render_reasoning_picker(frame, app);
    render_actions_palette(frame, app);
    render_subagents(frame, app);
    render_info_overlay(frame, app);
}

fn render_actions_palette(frame: &mut Frame<'_>, app: &App) {
    let Some(palette) = app.actions_palette() else {
        return;
    };
    let height = (palette.matched_actions().len() as u16).saturating_add(4);
    let layout = Floating::new("Actions", 68, height, &ACTIONS_FOOTER).render(
        frame,
        frame.area(),
        app.theme(),
    );
    let [search, actions] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(layout.body);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Search: ", Style::default().fg(app.theme().muted())),
            Span::styled(palette.query(), Style::default().fg(app.theme().text())),
        ])),
        search,
    );
    let items = palette
        .matched_actions()
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let marker = if palette.selected_index() == Some(index) {
                "› "
            } else {
                "  "
            };
            let color = if palette.selected_index() == Some(index) {
                app.theme().accent()
            } else {
                app.theme().text()
            };
            ListItem::new(Line::styled(
                format!("{marker}{}", action.label()),
                Style::default().fg(color),
            ))
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("  No matching actions").style(Style::default().fg(app.theme().error())),
            actions,
        );
    } else {
        frame.render_widget(
            List::new(items).style(Style::default().bg(app.theme().code_background())),
            actions,
        );
    }
}

fn render_subagents(frame: &mut Frame<'_>, app: &mut App) {
    if !app.subagents_open() {
        return;
    }
    let area = frame.area();
    let popup = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    frame.render_widget(Clear, popup);
    let theme = *app.theme();
    if let Some(model) = app.subagent_view_mut() {
        super::subagents::render(frame, popup, model, &theme);
    }
}

fn render_info_overlay(frame: &mut Frame<'_>, app: &App) {
    let Some(overlay) = app.info_overlay() else {
        return;
    };
    match overlay {
        InfoOverlay::Keybindings => {
            super::keybindings::KeybindingsHelp.render(frame, frame.area(), app.theme());
        }
        InfoOverlay::ContextDiagnostics => {
            let now = Instant::now();
            let conversation = app.active_conversation();
            let retry = conversation.retry_status(now);
            let cost = conversation
                .last_cost_usd
                .as_deref()
                .map(|value| format!("${value}"));
            let last_run = conversation.last_run_diagnostics();
            let diagnostics = ContextDiagnostics {
                model: app.model().as_str(),
                reasoning: app.thinking().as_str(),
                fast_mode: app.fast_mode(),
                context_used_tokens: None,
                context_limit_tokens: Some(nanocodex::oai::CONTEXT_WINDOW_TOKENS),
                input_tokens: last_run.map(|run| run.usage.input_tokens),
                cached_input_tokens: last_run.map(|run| run.usage.cached_input_tokens),
                model_calls: last_run.map(|run| run.model_calls),
                compactions: last_run.map(|run| run.compactions),
                response_retries: last_run.map(|run| run.response_retries),
                retry: retry.as_deref(),
                turn_status: &conversation.status,
                queued_turns: conversation.pending_turns,
                turn_elapsed: conversation.running.then(|| conversation.run_elapsed(now)),
                last_cost: cost.as_deref(),
            };
            ContextDiagnosticsPanel::new(diagnostics).render(frame, frame.area(), app.theme());
        }
    }
}

struct ViewLayout {
    header: Rect,
    transcript: Rect,
    pending: Rect,
    composer: Rect,
    footer: Rect,
    composer_layout: ComposerLayout,
}

fn view_layout(area: Rect, app: &mut App) -> ViewLayout {
    let composer_width = if app.historical_editor_active() {
        area.width.saturating_sub(4).max(1)
    } else {
        area.width.saturating_sub(2).max(1)
    };
    app.set_composer_width(composer_width);
    let composer_layout = ComposerLayout::new(&app.input, composer_width);
    let composer_height = if app.historical_editor_active() || app.branch_navigator_active() {
        3
    } else {
        composer_height(&composer_layout)
    };
    let cursor = composer_layout.cursor_position(&app.input, app.cursor);
    app.settle_composer_viewport(
        cursor.row,
        composer_layout.row_count(),
        usize::from(composer_height.saturating_sub(2)),
    );
    let pending_height = pending_height(app);
    let [
        header_area,
        transcript_area,
        pending_area,
        composer_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(pending_height),
        Constraint::Length(composer_height),
        Constraint::Length(1),
    ])
    .areas(area);

    ViewLayout {
        header: header_area,
        transcript: transcript_area,
        pending: pending_area,
        composer: composer_area,
        footer: footer_area,
        composer_layout,
    }
}

fn render_reasoning_picker(frame: &mut Frame<'_>, app: &App) {
    let Some(picker) = app.reasoning_picker() else {
        return;
    };
    let popup_height = match picker {
        ReasoningPicker::Standard { .. } => 10,
        ReasoningPicker::Advanced => 7,
    };
    let popup = Floating::new("Reasoning", 80, popup_height, &REASONING_FOOTER)
        .render(frame, frame.area(), app.theme())
        .body;

    let mut lines = Vec::new();
    match picker {
        ReasoningPicker::Standard { selected } => {
            lines.push(Line::styled(
                format!("  Select Reasoning Level for {}", app.model()),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::default());
            for (index, (thinking, label, description)) in
                STANDARD_THINKING_OPTIONS.iter().enumerate()
            {
                let mut label = (*label).to_owned();
                if *thinking == nanocodex::Thinking::default() {
                    label.push_str(" (default)");
                }
                if *thinking == app.thinking() {
                    label.push_str(" (current)");
                }
                lines.push(reasoning_option_line(
                    index == selected,
                    index + 1,
                    &label,
                    description,
                    app.theme().accent(),
                ));
            }
            lines.push(reasoning_option_line(
                selected == STANDARD_THINKING_OPTIONS.len(),
                STANDARD_THINKING_OPTIONS.len() + 1,
                "More reasoning…",
                "Max consumes usage limits faster",
                app.theme().accent(),
            ));
        }
        ReasoningPicker::Advanced => {
            lines.push(Line::styled(
                "  Advanced Reasoning",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::styled(
                "  ⚠ Consumes usage limits faster",
                Style::default().fg(app.theme().accent()),
            ));
            lines.push(Line::default());
            let label = if app.thinking() == nanocodex::Thinking::Max {
                "Max (current)"
            } else {
                "Max"
            };
            lines.push(reasoning_option_line(
                true,
                1,
                label,
                "For difficult problems when quality matters more than speed · higher usage",
                app.theme().accent(),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), popup);
}

fn reasoning_option_line(
    selected: bool,
    number: usize,
    label: &str,
    description: &str,
    accent: Color,
) -> Line<'static> {
    let marker = if selected { "›" } else { " " };
    let style = if selected {
        Style::default().fg(accent)
    } else {
        Style::default()
    };
    Line::styled(
        format!("{marker} {number}. {label:<19} {description}"),
        style,
    )
}

#[derive(Default)]
struct SelectableAreas {
    areas: [Rect; 3],
    count: usize,
}

impl SelectableAreas {
    fn push(&mut self, area: Rect) {
        if let Some(slot) = self.areas.get_mut(self.count) {
            *slot = area;
            self.count += 1;
        }
    }

    fn as_slice(&self) -> &[Rect] {
        &self.areas[..self.count]
    }
}

fn render_transcripts(
    frame: &mut Frame<'_>,
    app: &mut App,
    transcript_area: Rect,
    selectable_areas: &mut SelectableAreas,
) {
    let theme = *app.theme();
    let historical_editor_index = app.historical_editor_index();
    let inline_edit = historical_editor_index.map(|index| InlineEdit {
        index,
        input: app.input.as_str(),
        cursor: app.cursor,
    });
    if app.btw.is_some() {
        let [main_area, btw_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(transcript_area);
        let preserve_main = app.mouse_selection_intersects(main_area);
        let preserve_btw = app.mouse_selection_intersects(btw_area);
        if let Some(btw) = app.btw.as_mut() {
            selectable_areas.push(render_transcript(
                frame,
                &mut app.main,
                main_area,
                &theme,
                TranscriptRenderOptions {
                    title: " Main ",
                    focused: app.focus == PaneId::Main,
                    inline_edit,
                    empty_message:
                        "Ask Nanocodex to inspect, edit, run, or explain this workspace.",
                    preserve_view: preserve_main,
                },
            ));
            selectable_areas.push(render_transcript(
                frame,
                &mut btw.conversation,
                btw_area,
                &theme,
                TranscriptRenderOptions {
                    title: " BTW · forked context ",
                    focused: app.focus == PaneId::Btw(btw.id),
                    inline_edit: None,
                    empty_message: "Ask a quick side question without interrupting the main thread.",
                    preserve_view: preserve_btw,
                },
            ));
        }
    } else if app.branch_navigator_active() {
        let [main_area, navigator_area] =
            Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)])
                .areas(transcript_area);
        let selected = app.branch_navigator_selected_id().unwrap_or_default();
        let title = format!(" Branch {selected} preview ");
        {
            let conversation = app.branch_navigator_conversation_mut();
            selectable_areas.push(render_transcript(
                frame,
                conversation,
                main_area,
                &theme,
                TranscriptRenderOptions {
                    title: &title,
                    focused: true,
                    inline_edit: None,
                    empty_message:
                        "Ask Nanocodex to inspect, edit, run, or explain this workspace.",
                    preserve_view: false,
                },
            ));
        }
        render_branch_navigator(frame, app, navigator_area);
    } else {
        let preserve_main = app.mouse_selection_intersects(transcript_area);
        selectable_areas.push(render_transcript(
            frame,
            &mut app.main,
            transcript_area,
            &theme,
            TranscriptRenderOptions {
                title: " Main ",
                focused: true,
                inline_edit,
                empty_message: "Ask Nanocodex to inspect, edit, run, or explain this workspace.",
                preserve_view: preserve_main,
            },
        ));
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled(
            " nanocodex ",
            Style::default()
                .fg(
                    if app.theme().resolved_mode() == super::theme::ThemeMode::Light {
                        Color::White
                    } else {
                        Color::Black
                    },
                )
                .bg(app.theme().accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            app.cwd.display().to_string(),
            Style::default().fg(app.theme().muted()),
        ),
    ];
    let graph = app.main_branch_graph();
    if graph != "0*" {
        spans.push(Span::styled(
            format!("  branches {graph} · Ctrl+Alt+B browse · Ctrl+Alt+↑/↓ cycle"),
            Style::default().fg(app.theme().warning()),
        ));
    }
    let title = Line::from(spans);
    frame.render_widget(Paragraph::new(title), area);
}

fn render_branch_navigator(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = if app.main.running || app.main.pending_turns > 0 {
        " Branch tree · live preview; switch when idle "
    } else {
        " Branch tree · moving switches "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let previews = app.branch_previews();
    let capacity = (usize::from(inner.height) / 3).max(1);
    let selected = previews
        .iter()
        .position(|preview| preview.selected)
        .unwrap_or(0);
    let start = selected
        .saturating_sub(capacity / 2)
        .min(previews.len().saturating_sub(capacity));
    let mut lines = Vec::new();
    for preview in previews.iter().skip(start).take(capacity) {
        let active = if preview.active { " current" } else { "" };
        let marker = if preview.selected { "›" } else { " " };
        let node = if preview.active { "●" } else { "○" };
        let header_style = if preview.selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if preview.active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::styled(
            format!(
                "{marker} {}{node} branch {}{active}",
                preview.tree_prefix, preview.id
            ),
            header_style,
        ));
        lines.push(Line::styled(
            format!(
                "  {}{}",
                "  ".repeat(preview.depth),
                preview
                    .prompt
                    .map_or("(branch point)".to_owned(), prompt_preview)
            ),
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::raw(""));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[derive(Clone, Copy)]
struct TranscriptRenderOptions<'a> {
    title: &'a str,
    focused: bool,
    inline_edit: Option<InlineEdit<'a>>,
    empty_message: &'static str,
    preserve_view: bool,
}

fn render_transcript(
    frame: &mut Frame<'_>,
    conversation: &mut Conversation,
    area: Rect,
    theme: &super::theme::Theme,
    options: TranscriptRenderOptions<'_>,
) -> Rect {
    let TranscriptRenderOptions {
        title,
        focused,
        inline_edit,
        empty_message,
        preserve_view,
    } = options;
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            theme.accent()
        } else {
            theme.border()
        }));
    let inner = block.inner(area);
    let show_updates = conversation.has_unseen_output();
    let [available_transcript, updates_area] = if show_updates && inner.height > 1 {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner)
    } else {
        [inner, Rect::default()]
    };
    conversation.settle_viewport_with_selection(
        available_transcript.width,
        available_transcript.height,
        preserve_view,
    );
    let mut scroll_from_bottom = conversation.display_scroll_from_bottom();
    let pinned = conversation
        .transcript
        .pinned_user_prompt(
            scroll_from_bottom,
            available_transcript.width,
            available_transcript.height,
        )
        .map(|(_, message)| message.lines().next().unwrap_or_default().to_owned());
    let [pinned_area, transcript_inner] = if pinned.is_some() && available_transcript.height > 1 {
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(available_transcript)
    } else {
        [Rect::default(), available_transcript]
    };
    if pinned.is_some() {
        conversation.settle_viewport_with_selection(
            transcript_inner.width,
            transcript_inner.height,
            preserve_view,
        );
        scroll_from_bottom = conversation.display_scroll_from_bottom();
    }
    frame.render_widget(block, area);

    if let Some(message) = pinned
        && !pinned_area.is_empty()
    {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " › You · ",
                    Style::default()
                        .fg(theme.accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(message, Style::default().fg(theme.text())),
            ]))
            .style(Style::default().bg(theme.selection())),
            pinned_area,
        );
    }

    frame.render_widget(
        conversation
            .transcript
            .widget(
                scroll_from_bottom,
                conversation.selected_entry(),
                inline_edit,
                empty_message,
            )
            .math_fallback(preserve_view),
        transcript_inner,
    );
    if show_updates && !updates_area.is_empty() {
        let unseen_updates = conversation.unseen_updates;
        let noun = if unseen_updates == 1 {
            "update"
        } else {
            "updates"
        };
        frame.render_widget(
            Paragraph::new(format!("↓ {} {noun} · Ctrl+End to follow", unseen_updates))
                .alignment(Alignment::Center)
                .style(Style::default().fg(theme.accent())),
            updates_area,
        );
    }
    if let Some(edit) = inline_edit
        && let Some(position) = conversation.transcript.inline_edit_cursor(
            transcript_inner,
            scroll_from_bottom,
            conversation.selected_entry(),
            edit,
        )
    {
        frame.set_cursor_position(position);
    }
    transcript_inner
}

fn render_composer(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    layout: &ComposerLayout,
) -> Rect {
    let active_subagents = app
        .subagent_view_mut()
        .map_or(0, |subagents| subagents.active_count());
    let conversation_running = app.active_conversation().running;
    let target = match app.focus {
        PaneId::Main => "Main",
        PaneId::Btw(_) => "BTW",
    };
    let chrome = composer_chrome(app, active_subagents, target, area.width, Instant::now());
    // Visual behavior derived from clabby/tact's Pi-style composer at
    // 4df68c820427643216d6f2d61c58af89acc27a30 (Apache-2.0).
    let top_titles_width = chrome.top_left.width() + chrome.top_right.width();
    let bottom_titles_width = chrome.bottom_left.width() + chrome.bottom_right.width();
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if conversation_running {
            app.theme().warning()
        } else {
            app.theme().accent()
        }))
        .title_top(chrome.top_right.right_aligned())
        .title_bottom(chrome.bottom_right.right_aligned());
    let title_width = usize::from(area.width.saturating_sub(2));
    if top_titles_width < title_width {
        block = block.title_top(chrome.top_left);
    }
    if bottom_titles_width < title_width {
        block = block.title_bottom(chrome.bottom_left);
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.historical_editor_active() || app.branch_navigator_active() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                " draft preserved ",
                Style::default().fg(app.theme().muted()),
            )),
            inner,
        );
        return inner;
    }
    let cursor = layout.cursor_position(&app.input, app.cursor);
    let vertical_scroll = app.composer_scroll();
    let visible_end = vertical_scroll.saturating_add(usize::from(inner.height));
    let lines = (vertical_scroll..visible_end)
        .filter_map(|row| layout.row(row))
        .map(|range| Line::raw(&app.input[range.clone()]))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    if app.transcript_selection_active() || app.branch_navigator_active() {
        return inner;
    }

    let x = inner
        .x
        .saturating_add(saturating_u16(cursor.column).min(inner.width.saturating_sub(1)));
    let y = inner
        .y
        .saturating_add(saturating_u16(cursor.row.saturating_sub(vertical_scroll)));
    frame.set_cursor_position(Position::new(x, y));
    inner
}

struct ComposerChrome {
    top_left: Line<'static>,
    top_right: Line<'static>,
    bottom_left: Line<'static>,
    bottom_right: Line<'static>,
}

fn composer_chrome(
    app: &mut App,
    active_subagents: usize,
    target: &str,
    width: u16,
    now: Instant,
) -> ComposerChrome {
    let capacity = usize::from(width.saturating_sub(2));
    let top_right = composer_identity(app, capacity, now);
    let left_budget = capacity.saturating_sub(top_right.width()).saturating_sub(1);
    let top_left = composer_activity(app, active_subagents, target, left_budget, now);

    let workspace_budget = capacity.min(if width >= 72 { 32 } else { 20 });
    let workspace = truncate_left(
        &app.cwd.display().to_string(),
        workspace_budget.saturating_sub(2),
    );
    let bottom_right = Line::styled(
        format!(" {workspace} "),
        Style::default().fg(app.theme().muted()),
    );
    let hint_budget = capacity
        .saturating_sub(bottom_right.width())
        .saturating_sub(1);
    let bottom_left = composer_input_hint(app, hint_budget);

    ComposerChrome {
        top_left,
        top_right,
        bottom_left,
        bottom_right,
    }
}

fn composer_identity(app: &App, budget: usize, now: Instant) -> Line<'static> {
    let model = format!(" {} ", app.model().as_str());
    if model.width() > budget {
        return Line::styled(
            truncate_right(&model, budget),
            Style::default().fg(app.theme().accent()),
        );
    }

    let effort = if app.thinking() == nanocodex::Thinking::None {
        " default ".to_owned()
    } else {
        format!(" {} ", app.thinking().as_str())
    };
    let timer = app.active_conversation().running.then(|| {
        format!(
            " {} ",
            format_elapsed(app.active_conversation().run_elapsed(now))
        )
    });
    let fast = app.fast_mode().then_some(" ⚡ ");

    let mut used = model.width();
    let show_effort = used + effort.width() <= budget;
    used += usize::from(show_effort) * effort.width();
    let show_timer = timer
        .as_deref()
        .is_some_and(|timer| used + timer.width() <= budget);
    if show_timer {
        used += timer.as_deref().map_or(0, UnicodeWidthStr::width);
    }
    let show_fast = fast.is_some_and(|fast| used + fast.width() <= budget);

    let mut spans = Vec::new();
    if show_timer {
        spans.push(Span::styled(
            timer.unwrap_or_default(),
            Style::default().fg(app.theme().muted()),
        ));
    }
    spans.push(Span::styled(
        model,
        Style::default()
            .fg(app.theme().accent())
            .add_modifier(Modifier::BOLD),
    ));
    if show_effort {
        let effort_color = app.theme().effort(app.thinking());
        spans.push(Span::styled(
            effort,
            Style::default()
                .fg(effort_color)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if show_fast {
        spans.push(Span::styled(
            fast.unwrap_or_default(),
            Style::default()
                .fg(app.theme().warning())
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn composer_activity(
    app: &mut App,
    active_subagents: usize,
    target: &str,
    budget: usize,
    now: Instant,
) -> Line<'static> {
    if budget < 3 {
        return Line::default();
    }
    let conversation = app.active_conversation();
    let running = conversation.running;
    let status = conversation.status.clone();
    let retry = conversation.retry_status(now);
    let last_cost = conversation.last_cost_usd.clone();
    let pending_steers = conversation.pending_steers.len();
    let queued = conversation
        .pending_turns
        .saturating_sub(usize::from(conversation.running));
    let state = if app.branch_navigator_active() {
        "Browsing branches".to_owned()
    } else if app.historical_editor_active() {
        format!(
            "Draft parked · branch {}",
            app.historical_editor_source_branch().unwrap_or_default()
        )
    } else if running {
        retry.unwrap_or(status.clone())
    } else {
        status.clone()
    };
    let color = if running {
        app.theme().warning()
    } else if status == "Ready" {
        app.theme().success()
    } else {
        app.theme().muted()
    };
    let spinner_width = usize::from(running) * 2;
    let base = truncate_right(
        &format!(" {state} → {target} "),
        budget.saturating_sub(spinner_width),
    );
    let (spinner, waved) = app.activity_visual(&base, color, running, now);
    let mut spans = Vec::with_capacity(waved.len().saturating_add(5));
    if let Some(spinner) = spinner {
        spans.push(Span::styled(
            format!(" {spinner}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
    spans.extend(waved);
    let mut used: usize = spans.iter().map(Span::width).sum();
    let mut push = |text: String, style: Style| {
        if used + text.width() <= budget {
            used += text.width();
            spans.push(Span::styled(text, style));
        }
    };
    if active_subagents > 0 {
        let noun = if active_subagents == 1 {
            "subagent"
        } else {
            "subagents"
        };
        push(
            format!("· {active_subagents} {noun} "),
            Style::default()
                .fg(app.theme().warning())
                .add_modifier(Modifier::BOLD),
        );
    }
    if let Some(cost) = last_cost {
        push(
            format!("· ${cost} "),
            Style::default().fg(app.theme().muted()),
        );
    }
    let queue = footer_queue(pending_steers, queued);
    if !queue.is_empty() {
        push(
            format!("{} ", queue.trim_start()),
            Style::default().fg(app.theme().muted()),
        );
    }
    Line::from(spans)
}

fn composer_input_hint(app: &App, budget: usize) -> Line<'static> {
    if !app.input.is_empty() || app.historical_editor_active() || app.branch_navigator_active() {
        return Line::default();
    }
    let action = if app.active_conversation().running {
        "steer"
    } else {
        "send"
    };
    let hints = [
        format!(" / actions · Enter {action} · Tab queue · Ctrl+V image "),
        format!(" / actions · Enter {action} "),
        " / actions ".to_owned(),
    ];
    let hint = hints
        .into_iter()
        .find(|hint| hint.width() <= budget)
        .unwrap_or_default();
    Line::styled(
        hint,
        Style::default()
            .fg(app.theme().muted())
            .add_modifier(Modifier::DIM),
    )
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let help = if app.branch_navigator_active() {
        "Branches — ↑/↓ or j/k switch + preview · Esc close".to_owned()
    } else if app.historical_editor_active() {
        let branch = app.historical_editor_source_branch().unwrap_or_default();
        if app.main.running || app.main.pending_turns > 0 {
            format!(
                "Editing branch {branch} — Enter stops live turn + forks · Shift+Enter newline · Esc cancel"
            )
        } else {
            format!(
                "Editing branch {branch} — Enter forks here · Shift+Enter newline · Esc cancel · Ctrl+G $EDITOR"
            )
        }
    } else if app.tool_focus_active() {
        "Tools — ↑/↓ select · Enter expand/collapse · Ctrl+O all · Esc return".to_owned()
    } else if app.transcript_selection_active() {
        "History — ↑/↓ select · e edit/fork · Esc return".to_owned()
    } else if app.cancel_confirmation_active() {
        "Stop Agent Turn — Esc again to confirm".to_owned()
    } else if area.width >= 100 && app.btw.is_some() {
        "BackTab switch · / actions · Alt+O tools · Ctrl+O details · Ctrl+V image · Shift+Enter newline · Esc Esc stop · Ctrl+C quit".to_owned()
    } else if area.width >= 100 {
        "/ actions · Alt+O tools · Ctrl+O details · /btw side fork · Ctrl+V image · Shift+Enter newline · Esc Esc stop · Ctrl+C quit".to_owned()
    } else if area.width >= 58 {
        "/ actions · Alt+O tools · Ctrl+O details · Shift+Enter newline · Esc Esc stop".to_owned()
    } else if area.width >= 36 {
        "/ actions · Alt+O tools · Esc Esc stop".to_owned()
    } else {
        "/ actions · Esc Esc stop".to_owned()
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!(" {help}"),
            Style::default().fg(app.theme().muted()),
        )),
        area,
    );
}

fn footer_queue(steers: usize, queued: usize) -> String {
    match (steers, queued) {
        (0, 0) => String::new(),
        (0, queued) => format!(" · {queued} queued"),
        (1, 0) => " · 1 steer".to_owned(),
        (steers, 0) => format!(" · {steers} steers"),
        (1, queued) => format!(" · 1 steer · {queued} queued"),
        (steers, queued) => format!(" · {steers} steers · {queued} queued"),
    }
}

fn format_elapsed(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3_600 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!(
        "{}h {:02}m {:02}s",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

fn conversation_pending_count(conversation: &Conversation) -> usize {
    conversation.pending_steers.len() + conversation.queued_prompts.len()
}

fn pending_height(app: &App) -> u16 {
    let main_count = conversation_pending_count(&app.main);
    let count = app.btw.as_ref().map_or(main_count, |btw| {
        main_count.max(conversation_pending_count(&btw.conversation))
    });
    if count == 0 {
        0
    } else {
        saturating_u16(count.min(3) + 2)
    }
}

fn render_pending(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    if let Some(btw) = &app.btw {
        let [main_area, btw_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(area);
        render_conversation_pending(
            frame,
            &app.main,
            main_area,
            " Main pending input ",
            app.focus == PaneId::Main,
        );
        render_conversation_pending(
            frame,
            &btw.conversation,
            btw_area,
            " BTW pending input ",
            app.focus == PaneId::Btw(btw.id),
        );
    } else {
        render_conversation_pending(frame, &app.main, area, " Pending input ", true);
    }
}

fn render_conversation_pending(
    frame: &mut Frame<'_>,
    conversation: &Conversation,
    area: Rect,
    title: &'static str,
    focused: bool,
) {
    let mut lines = Vec::new();
    for steer in &conversation.pending_steers {
        let (label, color) = if steer.is_admitted() {
            ("↳ steer   ", Color::Yellow)
        } else {
            ("… steer   ", Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::styled(label, Style::default().fg(color)),
            Span::raw(prompt_preview(steer.prompt())),
        ]));
    }
    for prompt in &conversation.queued_prompts {
        lines.push(Line::from(vec![
            Span::styled("⏳ queued ", Style::default().fg(Color::DarkGray)),
            Span::raw(prompt_preview(prompt)),
        ]));
    }
    lines.truncate(3);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn prompt_preview(prompt: &str) -> String {
    const MAX_CHARS: usize = 96;
    let mut preview = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if preview.chars().count() > MAX_CHARS {
        preview = preview.chars().take(MAX_CHARS - 1).collect();
        preview.push('…');
    }
    preview
}

fn composer_height(layout: &ComposerLayout) -> u16 {
    saturating_u16(layout.row_count())
        .clamp(1, 7)
        .saturating_add(2)
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn truncate_right(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let content_width = width.saturating_sub(1);
    let mut used = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

fn truncate_left(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let content_width = width.saturating_sub(1);
    let mut suffix = Vec::new();
    let mut used = 0;
    for character in value.chars().rev() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        suffix.push(character);
        used += character_width;
    }
    suffix.reverse();
    format!("…{}", suffix.into_iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::mpsc,
        time::{Duration, Instant},
    };

    use ratatex::{PixelSize, Ratatex, TerminalProfile};
    use ratatui::{
        Terminal,
        backend::{Backend, ClearType, TestBackend, WindowSize},
        buffer::Cell,
        layout::{Position, Rect, Size},
        style::{Color, Modifier},
    };

    use super::{render, render_animation};
    use crate::tui::{app::App, transcript::TranscriptItem};

    #[test]
    fn btw_renders_as_a_side_by_side_focused_pane() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new("/workspace".into());
        app.begin_btw();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Main"));
        assert!(rendered.contains("BTW · forked context"));
        assert!(rendered.contains("→ BTW"));
        assert!(rendered.contains("BackTab switch"));
    }

    #[test]
    fn active_turn_renders_steers_separately_from_queued_follow_ups() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main.running = true;
        let steer_id = app
            .queue_steer(
                crate::tui::app::PaneId::Main,
                "use the database implementation".to_owned(),
            )
            .unwrap();
        app.steer_admitted(crate::tui::app::PaneId::Main, steer_id);
        assert!(
            app.queue_prompt(
                crate::tui::app::PaneId::Main,
                "write a final benchmark summary".to_owned()
            )
            .is_some()
        );

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Enter steer · Tab queue"));
        assert!(rendered.contains("Pending input"));
        assert!(rendered.contains("↳ steer"));
        assert!(rendered.contains("use the database implementation"));
        assert!(rendered.contains("⏳ queued"));
        assert!(rendered.contains("write a final benchmark summary"));
    }

    #[test]
    fn running_composer_shows_activity_and_one_elapsed_timer() {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main.running = true;
        let now = Instant::now();
        app.main.set_run_started_at(
            now.checked_sub(std::time::Duration::from_secs(65))
                .unwrap_or(now),
        );
        app.main.status = "Running exec_command".to_owned();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Running exec_command → Main"));
        assert!(rendered.contains("1m 05s"));
        assert_eq!(rendered.matches("1m 05s").count(), 1);
    }

    #[test]
    fn animation_render_matches_a_full_frame() {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main.running = true;
        app.main
            .transcript
            .push(TranscriptItem::Assistant("static transcript".to_owned()));
        let cached = terminal
            .draw(|frame| render(frame, &mut app))
            .unwrap()
            .buffer
            .clone();

        app.on_tick();
        terminal.current_buffer_mut().clone_from(&cached);
        terminal
            .draw(|frame| render_animation(frame, &mut app))
            .unwrap();
        let animation_frame = terminal.backend().buffer().clone();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(animation_frame, *terminal.backend().buffer());
    }

    #[test]
    fn composer_keeps_model_on_the_top_right_and_marks_fast_mode() {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut app = App::new("/workspace".into());
        app.fast_mode_changed(true);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("gpt-5.6-sol  high  ⚡"));
    }

    #[test]
    fn completed_turn_cost_is_visible_without_displacing_model_identity() {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main.last_cost_usd = Some("0.012345".to_owned());

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Ready → Main · $0.012345"));
        assert!(rendered.contains("gpt-5.6-sol  high"));
    }

    #[test]
    fn composer_reports_active_subagents_in_the_activity_chrome() {
        use crate::subagents::{AgentDescriptor, AgentId, AgentUpdate, ScopedAgentUpdate};

        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let mut app = App::new("/workspace".into());
        app.configure_subagent_view("root".to_owned(), 4);
        app.subagent_view_mut()
            .unwrap()
            .apply_update(&ScopedAgentUpdate {
                root_session_id: "root".to_owned(),
                update: AgentUpdate::Added(AgentDescriptor {
                    id: AgentId::new(1),
                    session_id: "child".to_owned(),
                    role: "reviewer".to_owned(),
                    task: "review composer".to_owned(),
                    parent: None,
                }),
            });

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert!(terminal.backend().to_string().contains("1 subagent"));
    }

    #[test]
    fn reasoning_picker_matches_codex_labels_and_advanced_flow() {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut app = App::new("/workspace".into());
        app.open_reasoning_picker();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Select Reasoning Level for gpt-5.6-sol"));
        assert!(rendered.contains("Low"));
        assert!(rendered.contains("High (default) (current)"));
        assert!(rendered.contains("Extra high"));
        assert!(rendered.contains("More reasoning…"));
        assert!(!rendered.contains("Maximum reasoning depth"));

        app.move_reasoning_picker(3);
        assert!(matches!(
            app.confirm_reasoning_picker(),
            Some(crate::tui::app::ReasoningPickerAction::OpenedAdvanced)
        ));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Advanced Reasoning"));
        assert!(rendered.contains("For difficult problems when quality"));
    }

    #[test]
    fn narrow_composer_preserves_the_model_before_help() {
        let mut terminal = Terminal::new(TestBackend::new(24, 10)).unwrap();
        let mut app = App::new("/workspace".into());

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("gpt-5.6-sol"));
    }

    #[test]
    fn mouse_selection_copies_composer_and_transcript_text() {
        let mut terminal = Terminal::new(TestBackend::new(48, 12)).unwrap();
        let mut app = App::new("/workspace".into());
        app.input = "copy composer".to_owned();
        app.cursor = app.input.len();
        app.main
            .transcript
            .push(TranscriptItem::User("transcript copy".to_owned()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert!(app.begin_mouse_selection((1, 9).into()));
        assert!(app.finish_mouse_selection((13, 9).into()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.take_pending_copy().as_deref(), Some("copy composer"));
        assert_eq!(
            terminal.backend().buffer().cell((1, 9)).unwrap().bg,
            Color::Indexed(8)
        );

        let _ = app.clear_mouse_selection();
        assert!(app.begin_mouse_selection((3, 3).into()));
        assert!(app.finish_mouse_selection((17, 3).into()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.take_pending_copy().as_deref(), Some("transcript copy"));
    }

    #[test]
    fn plain_composer_click_places_the_cursor() {
        let mut terminal = Terminal::new(TestBackend::new(48, 12)).unwrap();
        let mut app = App::new("/workspace".into());
        app.input = "click the composer".to_owned();
        app.cursor = app.input.len();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert!(app.begin_mouse_selection((7, 9).into()));
        assert!(app.finish_mouse_selection((7, 9).into()));
        assert_eq!(app.cursor, 6);
    }

    #[test]
    fn transcript_edge_drag_auto_scrolls_without_ending_the_selection() {
        let mut terminal = Terminal::new(TestBackend::new(48, 12)).unwrap();
        let mut app = App::new("/workspace".into());
        for index in 0..12 {
            app.main
                .transcript
                .push(TranscriptItem::User(format!("message {index}")));
        }
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.main.scroll_from_bottom = 5;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert!(app.begin_mouse_selection((2, 3).into()));
        assert!(app.drag_mouse_selection((20, 6).into()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let before = app.main.scroll_from_bottom;
        app.on_tick();

        assert_eq!(app.main.scroll_from_bottom, before - 1);
        assert!(app.mouse_selection_needs_redraw());
    }

    #[test]
    fn rendered_markdown_uses_the_same_selection_and_copy_path() {
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main.transcript.push(TranscriptItem::Assistant(
            "**bold** and [docs](https://example.com)".to_owned(),
        ));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let needle = "bold and docs";
        let buffer = terminal.backend().buffer();
        let (start_x, row) = (0..buffer.area.height)
            .find_map(|row| {
                (0..buffer.area.width).find_map(|column| {
                    let end = column.saturating_add(u16::try_from(needle.len()).ok()?);
                    (end <= buffer.area.width
                        && (column..end)
                            .map(|x| buffer[(x, row)].symbol())
                            .collect::<String>()
                            == needle)
                        .then_some((column, row))
                })
            })
            .expect("rendered Markdown should be visible");
        let end_x = start_x.saturating_add(u16::try_from(needle.len() - 1).unwrap_or(u16::MAX));

        assert!(app.begin_mouse_selection((start_x, row).into()));
        assert!(app.finish_mouse_selection((end_x, row).into()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(
            app.take_pending_copy().as_deref(),
            Some("bold and [docs](https://example.com)")
        );
        let selected = terminal.backend().buffer().cell((start_x, row)).unwrap();
        assert_eq!(selected.bg, Color::Indexed(8));
        assert!(selected.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn clicking_then_selecting_a_rendered_formula_copies_its_latex() {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let cache = tempfile::tempdir().unwrap();
        let renderer = Ratatex::builder(TerminalProfile::kitty(PixelSize::new(10, 20), false))
            .cache_dir(cache.path())
            .on_update(move || {
                let _ = wake_tx.try_send(());
            })
            .build()
            .unwrap();
        let source = "$$\n\\frac{a}{b}=c\n$$";
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut app = App::new("/workspace".into());
        app.set_math_renderer(renderer.clone());
        app.main
            .transcript
            .push(TranscriptItem::Assistant(source.to_owned()));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        app.invalidate_math_layouts();
        let _ = renderer.drain_terminal_commands();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let width = usize::from(terminal.backend().buffer().area.width);
        let formula_cells = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| {
                ratatex::is_formula_placeholder(cell.symbol()).then_some(Position::new(
                    u16::try_from(index % width).unwrap(),
                    u16::try_from(index / width).unwrap(),
                ))
            })
            .collect::<Vec<_>>();
        assert!(!formula_cells.is_empty());
        let start = Position::new(
            formula_cells.iter().map(|cell| cell.x).min().unwrap(),
            formula_cells.iter().map(|cell| cell.y).min().unwrap(),
        );
        let end = Position::new(
            formula_cells.iter().map(|cell| cell.x).max().unwrap(),
            formula_cells.iter().map(|cell| cell.y).max().unwrap(),
        );

        assert!(app.begin_mouse_selection(start));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.finish_mouse_selection(start));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert!(app.take_pending_copy().is_none());
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| !ratatex::is_formula_placeholder(cell.symbol()))
        );
        let fallback_text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(fallback_text.matches("$$").count(), 2);

        assert!(app.begin_mouse_selection(start));
        assert!(app.finish_mouse_selection(end));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(app.take_pending_copy().as_deref(), Some(source));
        assert_eq!(
            terminal.backend().buffer().cell(start).unwrap().bg,
            Color::Indexed(8)
        );
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| !ratatex::is_formula_placeholder(cell.symbol()))
        );
        renderer.shutdown();
    }

    #[test]
    fn source_mode_copies_multiple_formulas_with_surrounding_text() {
        let (wake_tx, wake_rx) = mpsc::channel();
        let cache = tempfile::tempdir().unwrap();
        let renderer = Ratatex::builder(TerminalProfile::kitty(PixelSize::new(10, 20), false))
            .cache_dir(cache.path())
            .on_update(move || {
                let _ = wake_tx.send(());
            })
            .build()
            .unwrap();
        let source = "Before\n\n$$\na=b\n$$\n\nBetween\n\n$$\nc=d\n$$\n\nAfter";
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let mut app = App::new("/workspace".into());
        app.set_math_renderer(renderer.clone());
        app.main
            .transcript
            .push(TranscriptItem::Assistant(source.to_owned()));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        for _ in 0..2 {
            wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        app.invalidate_math_layouts();
        let _ = renderer.drain_terminal_commands();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let width = usize::from(terminal.backend().buffer().area.width);
        let first_formula = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .enumerate()
            .find_map(|(index, cell)| {
                ratatex::is_formula_placeholder(cell.symbol()).then_some(Position::new(
                    u16::try_from(index % width).unwrap(),
                    u16::try_from(index / width).unwrap(),
                ))
            })
            .unwrap();
        assert!(app.begin_mouse_selection(first_formula));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.finish_mouse_selection(first_formula));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| !ratatex::is_formula_placeholder(cell.symbol()))
        );

        let find_text = |needle: &str| {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height).find_map(|row| {
                (0..buffer.area.width).find_map(|column| {
                    let end = column.saturating_add(u16::try_from(needle.len()).ok()?);
                    (end <= buffer.area.width
                        && (column..end)
                            .map(|x| buffer[(x, row)].symbol())
                            .collect::<String>()
                            == needle)
                        .then_some((
                            Position::new(column, row),
                            Position::new(end.saturating_sub(1), row),
                        ))
                })
            })
        };
        let (start, _) = find_text("Before").unwrap();
        let (_, end) = find_text("After").unwrap();

        assert!(app.begin_mouse_selection(start));
        assert!(app.finish_mouse_selection(end));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(
            app.take_pending_copy().as_deref(),
            Some("Before\n$$\na=b\n$$\nBetween\n$$\nc=d\n$$\nAfter")
        );
        renderer.shutdown();
    }

    #[test]
    fn rendered_javascript_fence_copies_only_its_source() {
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut app = App::new("/workspace".into());
        let source = "const result = await tools.exec_command({ cmd: \"cargo test --workspace\" });\ntext(result.output);";
        app.main.transcript.push(TranscriptItem::Assistant(format!(
            "Run this:\n\n```javascript\n{source}\n```"
        )));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let start_y = rows
            .iter()
            .position(|row| row.contains("const result"))
            .expect("first code line should be visible");
        let start_row = u16::try_from(start_y).unwrap();
        let start_x = (0..buffer.area.width)
            .find(|column| {
                (*column..buffer.area.width)
                    .take("const result".len())
                    .map(|x| buffer[(x, start_row)].symbol())
                    .collect::<String>()
                    == "const result"
            })
            .unwrap();
        let end_y = rows
            .iter()
            .enumerate()
            .skip(start_y + 1)
            .find_map(|(index, row)| row.contains("text(result.output);").then_some(index))
            .expect("last code line should be visible");
        let end_y = u16::try_from(end_y).unwrap();
        let end_start_x = (0..buffer.area.width)
            .find(|column| {
                (*column..buffer.area.width)
                    .take("text(result.output);".len())
                    .map(|x| buffer[(x, end_y)].symbol())
                    .collect::<String>()
                    == "text(result.output);"
            })
            .unwrap();
        let end_x = end_start_x
            .saturating_add(u16::try_from("text(result.output);".len() - 1).unwrap_or(u16::MAX));
        let start = (start_x, start_row);
        let end = (end_x, end_y);

        assert!(app.begin_mouse_selection(start.into()));
        assert!(app.finish_mouse_selection(end.into()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(app.take_pending_copy().as_deref(), Some(source));
    }

    #[test]
    fn selecting_history_during_a_running_response_keeps_transcript_context_visible() {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main
            .transcript
            .push_editable_user("active prompt".to_owned(), 1);
        app.main.push_assistant_delta(
            "streaming answer\nline two\nline three\nline four\nline five\nline six",
        );
        app.main.running = true;
        app.move_up();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("active prompt"));
        assert!(rendered.contains("streaming answer"));
        assert!(rendered.contains("line six"));
    }

    #[test]
    fn running_historical_edit_explains_that_submit_stops_and_forks() {
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main
            .transcript
            .push_editable_user("active prompt".to_owned(), 1);
        app.main.running = true;
        app.move_up();
        assert!(app.start_historical_edit());

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Draft parked · branch 0"));
        assert!(rendered.contains("Editing branch 0 — Enter stops live turn + forks"));
    }

    #[test]
    fn branch_navigator_renders_prompt_previews_beside_the_transcript() {
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main
            .transcript
            .push_editable_user("root branch prompt".to_owned(), 1);
        app.move_up();
        assert!(app.start_historical_edit());
        app.replace_input("revised branch prompt".to_owned());
        let request = app.commit_historical_edit().unwrap();
        let _ = app.main_branch_opened(
            request.new_branch,
            request.source_branch,
            request.prompt,
            std::sync::Arc::from("branch-session"),
        );
        app.main
            .transcript
            .push_editable_user("revised branch prompt".to_owned(), 2);
        assert!(app.toggle_branch_navigator());
        app.move_branch_navigator(-1);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Branch tree · moving switches"));
        assert!(rendered.contains("Branch 0 preview"));
        assert!(rendered.contains("root branch prompt"));
        assert!(rendered.contains("revised branch prompt"));
        assert!(rendered.contains("› ○ branch 0"));
        assert!(rendered.contains("└─● branch 1 current"));
    }

    #[test]
    fn btw_focus_keeps_main_steers_visible_in_their_own_pending_pane() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new("/workspace".into());
        app.main.running = true;
        let btw_id = app.begin_btw();
        let steer_id = app
            .queue_steer(
                crate::tui::app::PaneId::Main,
                "main correction remains visible".to_owned(),
            )
            .unwrap();
        app.steer_admitted(crate::tui::app::PaneId::Main, steer_id);
        assert!(
            app.queue_prompt(
                crate::tui::app::PaneId::Btw(btw_id),
                "queued BTW follow-up".to_owned(),
            )
            .is_some()
        );

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Main pending input"));
        assert!(rendered.contains("BTW pending input"));
        assert!(rendered.contains("↳ steer"));
        assert!(rendered.contains("main correction remains visible"));
        assert!(rendered.contains("⏳ queued"));
        assert!(rendered.contains("queued BTW follow-up"));
    }

    #[test]
    fn unseen_output_is_indicated_only_on_its_conversation() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new("/workspace".into());
        let btw_id = app.begin_btw();
        app.main.unseen_updates = 1;

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("↓ 1 update · Ctrl+End to follow"));
        assert_eq!(rendered.matches("Ctrl+End to follow").count(), 1);
        assert_eq!(app.focus, crate::tui::app::PaneId::Btw(btw_id));
    }

    #[test]
    fn empty_main_layout_snapshot() {
        let mut terminal = Terminal::new(TestBackend::new(48, 12)).unwrap();
        let mut app = App::new("/workspace".into());

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(
            terminal.backend().to_string(),
            concat!(
                "\" nanocodex   /workspace                         \"\n",
                "\"┌ Main ────────────────────────────────────────┐\"\n",
                "\"│                                              │\"\n",
                "\"│  Ask Nanocodex to inspect, edit, run, or     │\"\n",
                "\"│explain this workspace.                       │\"\n",
                "\"│                                              │\"\n",
                "\"│                                              │\"\n",
                "\"└──────────────────────────────────────────────┘\"\n",
                "\"╭ Ready → Main ───────────── gpt-5.6-sol  high ╮\"\n",
                "\"│                                              │\"\n",
                "\"╰ / actions · Enter send ────────── /workspace ╯\"\n",
                "\" / actions · Alt+O tools · Esc Esc stop         \"\n",
            )
        );
    }

    #[test]
    fn cursor_tracks_multiline_unicode_input_exactly() {
        let mut terminal = Terminal::new(TestBackend::new(48, 12)).unwrap();
        let mut app = App::new("/workspace".into());
        app.input = "ab\n界c".to_owned();
        app.cursor = app.input.len();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(terminal.get_cursor_position().unwrap(), Position::new(4, 9));
    }

    #[test]
    fn cursor_at_an_exact_wrap_boundary_uses_the_next_visual_row() {
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        let mut app = App::new("/workspace".into());
        app.input = "123456789012345678".to_owned();
        app.cursor = app.input.len();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(terminal.get_cursor_position().unwrap(), Position::new(1, 7));
    }

    #[test]
    fn multiline_cursor_moves_before_the_viewport_scrolls() {
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let mut app = App::new("/workspace".into());
        app.input = (0..10)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        app.cursor = app.input.len();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bottom = terminal.get_cursor_position().unwrap();
        app.move_up();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(terminal.get_cursor_position().unwrap().y, bottom.y - 1);
        assert_eq!(app.composer_scroll(), 3);

        for _ in 0..5 {
            app.move_up();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
        let top = terminal.get_cursor_position().unwrap().y;
        assert_eq!(app.composer_scroll(), 3);

        app.move_up();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap().y, top);
        assert_eq!(app.composer_scroll(), 2);
    }

    #[test]
    fn resize_reflows_layout_and_repositions_cursor() {
        let mut terminal = Terminal::new(TestBackend::new(48, 12)).unwrap();
        let mut app = App::new("/workspace".into());
        app.input = "abc".to_owned();
        app.cursor = app.input.len();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        terminal.backend_mut().resize(32, 10);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, 32, 10));
        assert_eq!(terminal.get_cursor_position().unwrap(), Position::new(4, 7));
    }

    #[test]
    fn ratatui_draws_only_changed_cells_after_the_first_frame() {
        let backend = CountingBackend::new(48, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("/workspace".into());

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(terminal.backend().draw_counts[0] > 0);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(terminal.backend().draw_counts[1], 0);

        app.input.push('x');
        app.cursor = app.input.len();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(terminal.backend().draw_counts[2], 25);
    }

    struct CountingBackend {
        inner: TestBackend,
        draw_counts: Vec<usize>,
    }

    impl CountingBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: TestBackend::new(width, height),
                draw_counts: Vec::new(),
            }
        }
    }

    impl Backend for CountingBackend {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            let content = content.collect::<Vec<_>>();
            self.draw_counts.push(content.len());
            self.inner.draw(content.into_iter())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> io::Result<()> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> io::Result<Size> {
            self.inner.size()
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }
}
