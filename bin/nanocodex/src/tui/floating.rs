//! Shared modal chrome, adapted from clabby/tact at
//! 4df68c820427643216d6f2d61c58af89acc27a30.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};

use super::theme::Theme;

const FOOTER_SEPARATOR: &str = "  ·  ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FooterShortcut<'a> {
    key: &'a str,
    label: &'a str,
}

impl<'a> FooterShortcut<'a> {
    pub(super) const fn new(key: &'a str, label: &'a str) -> Self {
        Self { key, label }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Placement {
    #[default]
    Center,
    Top,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FloatingLayout {
    pub(super) body: Rect,
}

pub(super) struct Floating<'a> {
    title: &'a str,
    preferred_width: u16,
    preferred_height: u16,
    footer: &'a [FooterShortcut<'a>],
    placement: Placement,
}

impl<'a> Floating<'a> {
    pub(super) const fn new(
        title: &'a str,
        preferred_width: u16,
        preferred_height: u16,
        footer: &'a [FooterShortcut<'a>],
    ) -> Self {
        Self {
            title,
            preferred_width,
            preferred_height,
            footer,
            placement: Placement::Center,
        }
    }

    #[allow(dead_code)]
    pub(super) const fn at_top(mut self) -> Self {
        self.placement = Placement::Top;
        self
    }

    pub(super) fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
    ) -> FloatingLayout {
        let popup = modal_area(
            area,
            self.preferred_width,
            self.preferred_height,
            self.placement,
        );
        if popup.is_empty() {
            return FloatingLayout { body: popup };
        }

        let mut block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border()));
        if !self.title.is_empty() {
            block = block
                .title(format!(" {} ", self.title))
                .title_alignment(Alignment::Center)
                .title_style(
                    Style::default()
                        .fg(theme.accent())
                        .add_modifier(Modifier::BOLD),
                );
        }

        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);

        let (body, footer) = split_footer(inner, !self.footer.is_empty());
        self.render_footer(frame, footer, theme);
        FloatingLayout { body }
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() || self.footer.is_empty() {
            return;
        }

        let mut spans = Vec::with_capacity(self.footer.len().saturating_mul(4));
        for (index, shortcut) in self.footer.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(
                    FOOTER_SEPARATOR,
                    Style::default().fg(theme.border()),
                ));
            }
            spans.push(Span::styled(
                shortcut.key,
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                shortcut.label,
                Style::default().fg(theme.muted()),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            area,
        );
    }
}

fn modal_area(area: Rect, width: u16, height: u16, placement: Placement) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = match placement {
        Placement::Center => area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2),
        Placement::Top => area.y,
    };
    Rect::new(x, y, width, height)
}

fn split_footer(inner: Rect, has_footer: bool) -> (Rect, Rect) {
    if !has_footer || inner.height < 2 {
        return (inner, Rect::default());
    }
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    (body, footer)
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    use super::{Floating, FooterShortcut};
    use crate::tui::theme::{Theme, ThemeMode};

    fn dark_theme() -> Theme {
        Theme::from_colorfgbg(ThemeMode::Auto, Some("15;0"))
    }

    const FOOTER: [FooterShortcut<'static>; 2] = [
        FooterShortcut::new("enter", "select"),
        FooterShortcut::new("esc", "close"),
    ];

    #[test]
    fn centers_rounded_chrome_title_and_aligned_footer() {
        let mut terminal = Terminal::new(TestBackend::new(24, 10)).unwrap();
        let mut layout = None;

        terminal
            .draw(|frame| {
                layout = Some(Floating::new("Reference", 20, 8, &FOOTER).render(
                    frame,
                    frame.area(),
                    &dark_theme(),
                ));
            })
            .unwrap();

        let layout = layout.unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(layout.body, ratatui::layout::Rect::new(3, 2, 18, 5));
        assert_eq!(buffer[(2, 1)].symbol(), "╭");
        assert_eq!(buffer[(21, 8)].symbol(), "╯");
        assert_eq!(buffer[(7, 1)].symbol(), "R");
        assert_eq!(buffer[(3, 7)].symbol(), "e");
        assert_eq!(buffer[(3, 7)].fg, Color::Cyan);
        assert_eq!(buffer[(9, 7)].fg, Color::Gray);
    }

    #[test]
    fn top_placement_and_tiny_areas_are_bounded() {
        let mut terminal = Terminal::new(TestBackend::new(8, 3)).unwrap();
        let mut layout = None;

        terminal
            .draw(|frame| {
                layout = Some(
                    Floating::new("Long title", 40, 20, &FOOTER)
                        .at_top()
                        .render(frame, frame.area(), &dark_theme()),
                );
            })
            .unwrap();

        let layout = layout.unwrap();
        assert_eq!(layout.body.height, 1);
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "╭");
        assert_eq!(terminal.backend().buffer().area.width, 8);
    }
}
