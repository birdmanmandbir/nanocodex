//! Responsive keyboard reference inspired by clabby/tact at
//! 4df68c820427643216d6f2d61c58af89acc27a30.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use super::{
    floating::{Floating, FooterShortcut},
    theme::Theme,
};

const FOOTER: [FooterShortcut<'static>; 1] = [FooterShortcut::new("esc", "close")];
const PRIMARY: [Binding; 10] = [
    Binding::new("/", "open actions · empty prompt"),
    Binding::new("enter", "submit prompt"),
    Binding::new("shift/alt+enter · ctrl+j", "insert newline"),
    Binding::new("tab", "queue prompt · switch pane when empty"),
    Binding::new("ctrl+g", "edit prompt in $EDITOR"),
    Binding::new("ctrl/alt+v", "attach clipboard image"),
    Binding::new("esc esc", "cancel the active turn"),
    Binding::new("ctrl+c", "exit Nanocodex"),
    Binding::new("↑/↓", "move cursor · prompt history at edge"),
    Binding::new("alt+backspace", "delete previous word"),
];
const NAVIGATION: [Binding; 10] = [
    Binding::new("pgup/pgdn · wheel", "scroll transcript"),
    Binding::new("ctrl+end", "follow latest output"),
    Binding::new("ctrl+o", "expand or collapse tool details"),
    Binding::new("alt+o", "focus latest tool"),
    Binding::new("↑/↓ · j/k", "navigate focused panels"),
    Binding::new("enter", "toggle focused tool"),
    Binding::new("ctrl+alt+b", "open branch navigator"),
    Binding::new("ctrl+alt+↑/↓", "cycle main branches"),
    Binding::new("mouse drag", "select transcript text"),
    Binding::new("e", "edit selected historical prompt"),
];

#[derive(Clone, Copy)]
struct Binding {
    key: &'static str,
    description: &'static str,
}

impl Binding {
    const fn new(key: &'static str, description: &'static str) -> Self {
        Self { key, description }
    }
}

pub(super) struct KeybindingsHelp;

impl KeybindingsHelp {
    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let layout =
            Floating::new("Keyboard shortcuts", 92, 24, &FOOTER).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }

        let lines = if layout.body.width >= 72 {
            wide_lines(layout.body.width, theme)
        } else {
            narrow_lines(theme)
        };
        frame.render_widget(Paragraph::new(lines), layout.body);
    }
}

fn wide_lines(width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let column_width = usize::from(width.saturating_sub(3) / 2);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            " Composer & session",
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(column_width.saturating_sub(19).saturating_add(3))),
        Span::styled(
            "Transcript & branches",
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    lines.push(Line::raw(""));
    for index in 0..PRIMARY.len() {
        let mut spans = binding_spans(PRIMARY[index], column_width, theme);
        spans.push(Span::raw("   "));
        spans.extend(binding_spans(NAVIGATION[index], column_width, theme));
        lines.push(Line::from(spans));
    }
    lines
}

fn narrow_lines(theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(PRIMARY.len() + NAVIGATION.len() + 3);
    lines.push(heading(" Composer & session", theme));
    lines.extend(
        PRIMARY
            .into_iter()
            .map(|binding| narrow_line(binding, theme)),
    );
    lines.push(Line::raw(""));
    lines.push(heading(" Transcript & branches", theme));
    lines.extend(
        NAVIGATION
            .into_iter()
            .map(|binding| narrow_line(binding, theme)),
    );
    lines
}

fn heading(title: &'static str, theme: &Theme) -> Line<'static> {
    Line::styled(
        title,
        Style::default()
            .fg(theme.accent())
            .add_modifier(Modifier::BOLD),
    )
}

fn narrow_line(binding: Binding, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {:<24}", binding.key),
            Style::default()
                .fg(theme.text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(binding.description, Style::default().fg(theme.muted())),
    ])
}

fn binding_spans(binding: Binding, width: usize, theme: &Theme) -> Vec<Span<'static>> {
    let key_width = PRIMARY
        .iter()
        .chain(NAVIGATION.iter())
        .map(|binding| binding.key.width())
        .max()
        .unwrap_or_default()
        .saturating_add(2)
        .min(width);
    let gap = key_width.saturating_sub(binding.key.width()).max(1);
    vec![
        Span::styled(
            format!(" {}{}", binding.key, " ".repeat(gap.saturating_sub(1))),
            Style::default()
                .fg(theme.text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(binding.description, Style::default().fg(theme.muted())),
    ]
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    use super::KeybindingsHelp;
    use crate::tui::theme::{Theme, ThemeMode};

    fn dark_theme() -> Theme {
        Theme::from_colorfgbg(ThemeMode::Auto, Some("15;0"))
    }

    fn rendered(terminal: &Terminal<TestBackend>) -> String {
        let width = usize::from(terminal.backend().buffer().area.width);
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_reference_uses_two_aligned_columns_with_real_shortcuts() {
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();

        terminal
            .draw(|frame| KeybindingsHelp.render(frame, frame.area(), &dark_theme()))
            .unwrap();

        let output = rendered(&terminal);
        for expected in [
            "Keyboard shortcuts",
            "Composer & session",
            "Transcript & branches",
            "shift/alt+enter · ctrl+j",
            "ctrl/alt+v",
            "ctrl+alt+b",
            "ctrl+alt+↑/↓",
            "esc close",
        ] {
            assert!(output.contains(expected), "missing {expected:?}\n{output}");
        }
        let buffer = terminal.backend().buffer();
        let key = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "/")
            .unwrap();
        assert_eq!(key.fg, Color::White);
    }

    #[test]
    fn narrow_reference_stays_inside_the_terminal() {
        let mut terminal = Terminal::new(TestBackend::new(34, 14)).unwrap();

        terminal
            .draw(|frame| KeybindingsHelp.render(frame, frame.area(), &dark_theme()))
            .unwrap();

        let output = rendered(&terminal);
        assert!(output.contains("Keyboard shortcuts"));
        assert!(output.contains("Composer & session"));
        assert_eq!(terminal.backend().buffer().area.width, 34);
        assert_eq!(terminal.backend().buffer().area.height, 14);
    }
}
