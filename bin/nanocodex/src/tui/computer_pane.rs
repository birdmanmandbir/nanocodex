use nanocodex_computer::{ComputerFrame, ComputerFramePhase};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};

/// Prepared state for the application-owned live computer pane.
///
/// PNG decoding happens before this reaches the render loop. Ratatui only
/// performs protocol-specific resizing when the pane dimensions change.
pub(super) struct ComputerPane {
    sequence: u64,
    generation: u64,
    application: String,
    window: String,
    phase: ComputerFramePhase,
    dimensions: (u32, u32),
    protocol: Option<StatefulProtocol>,
}

impl ComputerPane {
    pub(super) fn prepare(frame: &ComputerFrame, picker: Option<&Picker>) -> Result<Self, String> {
        let protocol = picker
            .map(|picker| {
                image::load_from_memory(frame.image.png())
                    .map(|image| picker.new_resize_protocol(image))
                    .map_err(|error| format!("failed to decode computer frame: {error}"))
            })
            .transpose()?;
        let mut pane = Self::status_only(
            frame.sequence,
            frame.generation,
            frame.application.name.clone(),
            frame
                .window
                .title
                .clone()
                .unwrap_or_else(|| "Untitled window".to_owned()),
            frame.phase,
            (frame.image.width(), frame.image.height()),
        );
        pane.protocol = protocol;
        Ok(pane)
    }

    pub(super) fn status_only(
        sequence: u64,
        generation: u64,
        application: impl Into<String>,
        window: impl Into<String>,
        phase: ComputerFramePhase,
        dimensions: (u32, u32),
    ) -> Self {
        Self {
            sequence,
            generation,
            application: application.into(),
            window: window.into(),
            phase,
            dimensions,
            protocol: None,
        }
    }

    pub(super) fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let title = format!(
            " Computer · {} — {} · #{} {} ",
            self.application,
            self.window,
            self.sequence,
            phase_label(self.phase),
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(phase_color(self.phase)))
            .title(Line::from(title));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        if let Some(protocol) = &mut self.protocol {
            frame.render_stateful_widget(StatefulImage::new(), inner, protocol);
        } else {
            frame.render_widget(
                Paragraph::new(format!(
                    "Live frame {}×{}\ngeneration {}\n\nTerminal graphics unavailable. Use the external computer preview for pixels.",
                    self.dimensions.0, self.dimensions.1, self.generation,
                ))
                .style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
    }
}

const fn phase_label(phase: ComputerFramePhase) -> &'static str {
    match phase {
        ComputerFramePhase::Observed => "observed",
        ComputerFramePhase::Settling => "settling",
        ComputerFramePhase::Settled => "settled",
        ComputerFramePhase::TimedOut => "timed out",
    }
}

const fn phase_color(phase: ComputerFramePhase) -> Color {
    match phase {
        ComputerFramePhase::Observed | ComputerFramePhase::Settled => Color::Green,
        ComputerFramePhase::Settling => Color::Yellow,
        ComputerFramePhase::TimedOut => Color::Red,
    }
}
