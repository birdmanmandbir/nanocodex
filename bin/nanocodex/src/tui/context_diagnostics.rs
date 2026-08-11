//! Read-only context diagnostics inspired by clabby/tact at
//! 4df68c820427643216d6f2d61c58af89acc27a30.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::{
    floating::{Floating, FooterShortcut},
    theme::Theme,
};

const FOOTER: [FooterShortcut<'static>; 1] = [FooterShortcut::new("esc", "close")];

#[derive(Clone, Copy, Debug)]
pub(super) struct ContextDiagnostics<'a> {
    pub(super) model: &'a str,
    pub(super) reasoning: &'a str,
    pub(super) fast_mode: bool,
    pub(super) context_used_tokens: Option<u64>,
    pub(super) context_limit_tokens: Option<u64>,
    pub(super) input_tokens: Option<u64>,
    pub(super) cached_input_tokens: Option<u64>,
    pub(super) model_calls: Option<u32>,
    pub(super) compactions: Option<u32>,
    pub(super) response_retries: Option<u32>,
    pub(super) retry: Option<&'a str>,
    pub(super) turn_status: &'a str,
    pub(super) queued_turns: usize,
    pub(super) turn_elapsed: Option<Duration>,
    pub(super) last_cost: Option<&'a str>,
}

impl Default for ContextDiagnostics<'_> {
    fn default() -> Self {
        Self {
            model: "unavailable",
            reasoning: "unavailable",
            fast_mode: false,
            context_used_tokens: None,
            context_limit_tokens: None,
            input_tokens: None,
            cached_input_tokens: None,
            model_calls: None,
            compactions: None,
            response_retries: None,
            retry: None,
            turn_status: "idle",
            queued_turns: 0,
            turn_elapsed: None,
            last_cost: None,
        }
    }
}

pub(super) struct ContextDiagnosticsPanel<'a> {
    diagnostics: ContextDiagnostics<'a>,
}

impl<'a> ContextDiagnosticsPanel<'a> {
    pub(super) const fn new(diagnostics: ContextDiagnostics<'a>) -> Self {
        Self { diagnostics }
    }

    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let layout =
            Floating::new("Context diagnostics", 72, 22, &FOOTER).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        frame.render_widget(
            Paragraph::new(self.lines(theme)).wrap(Wrap { trim: false }),
            layout.body,
        );
    }

    fn lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let diagnostics = self.diagnostics;
        let mut lines = Vec::with_capacity(18);
        lines.push(heading(" Model", theme));
        lines.push(fact(" Model", diagnostics.model.to_owned(), theme));
        lines.push(fact(
            " Reasoning / fast",
            format!(
                "{} / {}",
                diagnostics.reasoning,
                if diagnostics.fast_mode { "on" } else { "off" }
            ),
            theme,
        ));
        lines.push(heading(" Context window", theme));
        lines.push(fact(
            " Used / limit",
            format_window(
                diagnostics.context_used_tokens,
                diagnostics.context_limit_tokens,
            ),
            theme,
        ));
        lines.push(fact(
            " Headroom",
            optional_count(
                diagnostics
                    .context_used_tokens
                    .zip(diagnostics.context_limit_tokens)
                    .map(|(used, limit)| limit.saturating_sub(used)),
            ),
            theme,
        ));
        lines.push(heading(" Prompt cache", theme));
        lines.push(fact(
            " Input / cached",
            format!(
                "{} / {}",
                optional_count(diagnostics.input_tokens),
                optional_count(diagnostics.cached_input_tokens)
            ),
            theme,
        ));
        lines.push(fact(
            " Cache share",
            percentage(diagnostics.cached_input_tokens, diagnostics.input_tokens),
            theme,
        ));
        lines.push(heading(" Retry", theme));
        lines.push(fact(
            " Status",
            diagnostics.retry.unwrap_or("idle").to_owned(),
            theme,
        ));
        lines.push(heading(" Turn", theme));
        lines.push(fact(
            " State / queued",
            format!("{} / {}", diagnostics.turn_status, diagnostics.queued_turns),
            theme,
        ));
        lines.push(fact(
            " Calls / compactions",
            format!(
                "{} / {}",
                optional_u32(diagnostics.model_calls),
                optional_u32(diagnostics.compactions)
            ),
            theme,
        ));
        lines.push(fact(
            " Response retries",
            optional_u32(diagnostics.response_retries),
            theme,
        ));
        lines.push(fact(
            " Elapsed",
            diagnostics
                .turn_elapsed
                .map_or_else(|| "unavailable".to_owned(), format_duration),
            theme,
        ));
        lines.push(fact(
            " Last cost",
            diagnostics.last_cost.unwrap_or("unavailable").to_owned(),
            theme,
        ));
        lines
    }
}

fn heading(title: &'static str, theme: &Theme) -> Line<'static> {
    Line::styled(
        title,
        Style::default()
            .fg(theme.accent())
            .add_modifier(Modifier::BOLD),
    )
}

fn fact(label: &'static str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<20}"), Style::default().fg(theme.muted())),
        Span::styled(value, Style::default().fg(theme.text())),
    ])
}

fn format_window(used: Option<u64>, limit: Option<u64>) -> String {
    match (used, limit) {
        (Some(used), Some(limit)) => format!(
            "{} / {} ({})",
            format_count(used),
            format_count(limit),
            percentage(Some(used), Some(limit))
        ),
        (used, limit) => format!("{} / {}", optional_count(used), optional_count(limit)),
    }
}

fn percentage(part: Option<u64>, whole: Option<u64>) -> String {
    match (part, whole) {
        (Some(part), Some(whole)) if whole > 0 => {
            format!("{}%", part.saturating_mul(100) / whole)
        }
        _ => "unavailable".to_owned(),
    }
}

fn optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), format_count)
}

fn optional_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        return format!("{}.{:01}s", seconds, duration.subsec_millis() / 100);
    }
    format!("{}m {:02}s", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::{Terminal, backend::TestBackend};

    use super::{ContextDiagnostics, ContextDiagnosticsPanel};
    use crate::tui::theme::Theme;

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
    fn renders_model_context_cache_retry_and_turn_snapshot() {
        let diagnostics = ContextDiagnostics {
            model: "gpt-5-codex",
            reasoning: "high",
            fast_mode: true,
            context_used_tokens: Some(42_000),
            context_limit_tokens: Some(128_000),
            input_tokens: Some(40_000),
            cached_input_tokens: Some(30_000),
            model_calls: Some(3),
            compactions: Some(1),
            response_retries: Some(2),
            retry: Some("attempt 2/4 in 1.2s · overloaded"),
            turn_status: "thinking",
            queued_turns: 2,
            turn_elapsed: Some(Duration::from_millis(65_900)),
            last_cost: Some("$0.0142"),
        };
        let panel = ContextDiagnosticsPanel::new(diagnostics);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| panel.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let output = rendered(&terminal);
        for expected in [
            "Context diagnostics",
            "gpt-5-codex",
            "42,000 / 128,000 (32%)",
            "40,000 / 30,000",
            "75%",
            "attempt 2/4 in 1.2s · overloaded",
            "thinking / 2",
            "3 / 1",
            "Response retries   2",
            "1m 05s",
            "$0.0142",
            "esc close",
        ] {
            assert!(output.contains(expected), "missing {expected:?}\n{output}");
        }
    }

    #[test]
    fn unavailable_data_and_narrow_terminals_render_without_overflow() {
        let panel = ContextDiagnosticsPanel::new(ContextDiagnostics {
            model: "gpt-5",
            ..ContextDiagnostics::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(28, 9)).unwrap();

        terminal
            .draw(|frame| panel.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let output = rendered(&terminal);
        assert!(output.contains("Context diagnostics"));
        assert!(output.contains("gpt-5"));
        assert_eq!(terminal.backend().buffer().area.width, 28);
        assert_eq!(terminal.backend().buffer().area.height, 9);
    }
}
