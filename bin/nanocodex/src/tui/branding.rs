// Responsive animated Nanocodex branding for an empty transcript.

use std::f64::consts::TAU;

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Paragraph, Widget},
};

const FRAME_COUNT: usize = 48;
const MAX_WIDTH: u16 = 64;
const MAX_HEIGHT: u16 = 11;
const RAMP: [&str; 9] = ["·", ":", "-", "=", "+", "*", "#", "%", "@"];
const LEVEL_THRESHOLDS: [f64; 8] = [0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5];
const ASCII_WORDMARK: [&str; 4] = [
    " _ __   __ _ _ __   ___   ___ ___   __| | _____  __",
    "| '_ \\ / _` | '_ \\ / _ \\ / __/ _ \\ / _` |/ _ \\ \\/ /",
    "| | | | (_| | | | | (_) | (_| (_) | (_| |  __/>  <",
    "|_| |_|\\__,_|_| |_|\\___/ \\___\\___/ \\__,_|\\___/_/\\_\\",
];
const ASCII_WIDTH: u16 = 51;
const COMPACT_WORDMARK: &str = "nanocodex";
const TAGLINE: &str = "small agent · sharp tools";

pub(super) fn render_empty(
    area: Rect,
    buffer: &mut Buffer,
    animation_frame: usize,
    accent: Color,
    fallback: &str,
) {
    if area.width < 18 || area.height < 4 {
        render_fallback(area, buffer, fallback);
        return;
    }

    let mask = centered_mask(area);
    render_signal(mask, buffer, animation_frame, accent);
    if mask.width >= ASCII_WIDTH.saturating_add(4) && mask.height >= 8 {
        render_ascii_wordmark(mask, buffer);
    } else {
        render_compact_wordmark(mask, buffer);
    }
}

fn render_signal(area: Rect, buffer: &mut Buffer, animation_frame: usize, accent: Color) {
    let Ok(phase_index) = u16::try_from(animation_frame % FRAME_COUNT) else {
        unreachable!("branding frame index is bounded by FRAME_COUNT");
    };
    let phase = TAU * f64::from(phase_index) / 48.0;
    let center_x = f64::from(area.width.saturating_sub(1)) / 2.0;
    let center_y = f64::from(area.height.saturating_sub(1)) / 2.0;
    for row in 0..area.height {
        let inset = corner_inset(row, area.height).min(area.width.saturating_sub(1) / 2);
        for column in inset..area.width.saturating_sub(inset) {
            let x = f64::from(column) - center_x;
            let y = (f64::from(row) - center_y) * 2.2;
            let distance =
                ((x - phase.cos() * 8.0).powi(2) + (y - phase.sin() * 3.0).powi(2)).sqrt();
            let value = (x * 0.23 + phase).sin()
                + (x * 0.11 + y * 0.41 - phase * 2.0).sin()
                + (distance * 0.31 - phase).sin();
            let scaled = ((value + 3.0) / 6.0 * 8.0).clamp(0.0, 8.0);
            let level = LEVEL_THRESHOLDS.partition_point(|threshold| scaled >= *threshold);
            buffer[(area.x + column, area.y + row)]
                .set_symbol(RAMP[level])
                .set_style(signal_style(level, accent));
        }
    }
}

fn render_ascii_wordmark(mask: Rect, buffer: &mut Buffer) {
    let panel = Rect::new(
        mask.x + mask.width.saturating_sub(ASCII_WIDTH.saturating_add(2)) / 2,
        mask.y + mask.height.saturating_sub(6) / 2,
        ASCII_WIDTH.saturating_add(2),
        6,
    );
    clear(panel, buffer);
    let logo_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    for (row, line) in ASCII_WORDMARK.iter().enumerate() {
        buffer.set_string(
            panel.x.saturating_add(1),
            panel
                .y
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
            *line,
            logo_style,
        );
    }
    Paragraph::new(Line::styled(TAGLINE, Style::default().fg(Color::DarkGray)))
        .alignment(Alignment::Center)
        .render(
            Rect::new(panel.x, panel.bottom().saturating_sub(1), panel.width, 1),
            buffer,
        );
}

fn render_compact_wordmark(mask: Rect, buffer: &mut Buffer) {
    let panel_width = mask.width.min(32);
    let panel_height = mask.height.min(3);
    let panel = Rect::new(
        mask.x + mask.width.saturating_sub(panel_width) / 2,
        mask.y + mask.height.saturating_sub(panel_height) / 2,
        panel_width,
        panel_height,
    );
    clear(panel, buffer);
    let wordmark_y = panel.y + panel.height.saturating_sub(2) / 2;
    Paragraph::new(Line::styled(
        COMPACT_WORDMARK,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Center)
    .render(Rect::new(panel.x, wordmark_y, panel.width, 1), buffer);
    if panel.height >= 2 {
        Paragraph::new(Line::styled(TAGLINE, Style::default().fg(Color::DarkGray)))
            .alignment(Alignment::Center)
            .render(
                Rect::new(panel.x, wordmark_y.saturating_add(1), panel.width, 1),
                buffer,
            );
    }
}

fn render_fallback(area: Rect, buffer: &mut Buffer, message: &str) {
    Paragraph::new(Line::styled(
        format!("  {message}"),
        Style::default().fg(Color::DarkGray),
    ))
    .render(area, buffer);
}

fn centered_mask(area: Rect) -> Rect {
    let width = MAX_WIDTH.min(area.width.saturating_sub(4)).max(1);
    let height = MAX_HEIGHT.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn clear(area: Rect, buffer: &mut Buffer) {
    for row in area.y..area.bottom() {
        for column in area.x..area.right() {
            buffer[(column, row)].reset();
        }
    }
}

fn corner_inset(row: u16, height: u16) -> u16 {
    let edge_distance = row.min(height.saturating_sub(1).saturating_sub(row));
    match edge_distance {
        0 => 3,
        1 => 1,
        _ => 0,
    }
}

fn signal_style(level: usize, accent: Color) -> Style {
    let style = Style::default().fg(accent);
    if level < 3 {
        style.add_modifier(Modifier::DIM)
    } else if level >= 6 {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};

    use super::{ASCII_WORDMARK, render_empty};

    fn rendered(width: u16, height: u16, frame: usize) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        render_empty(area, &mut buffer, frame, Color::Yellow, "fallback");
        buffer
    }

    fn text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_empty_state_centers_the_full_ascii_wordmark() {
        let buffer = rendered(80, 20, 0);
        let rendered = text(&buffer);
        for line in ASCII_WORDMARK {
            assert!(rendered.contains(line));
        }
        assert!(rendered.contains("small agent · sharp tools"));
    }

    #[test]
    fn compact_empty_state_keeps_branding_legible() {
        let rendered = text(&rendered(40, 6, 0));
        assert!(rendered.contains("nanocodex"));
        assert!(rendered.contains("small agent · sharp tools"));
        assert!(!rendered.contains(ASCII_WORDMARK[0]));
    }

    #[test]
    fn signal_field_changes_across_animation_frames() {
        assert_ne!(rendered(80, 20, 0), rendered(80, 20, 4));
    }

    #[test]
    fn tiny_empty_state_uses_the_plain_fallback() {
        assert!(text(&rendered(16, 2, 0)).contains("fallback"));
    }
}
