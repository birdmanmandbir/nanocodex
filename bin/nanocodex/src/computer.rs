use clap::{ArgAction, Args};
use eyre::{Result, WrapErr};
use nanocodex_computer::{
    Computer, ComputerControl, ComputerEvent, ComputerFrames, ComputerPreview, ComputerTool,
};

/// Opt-in native computer-use configuration for normal agent sessions.
#[derive(Args, Default)]
pub(crate) struct ComputerArgs {
    /// Expose the local macOS desktop to Code Mode as `tools.computer`.
    #[arg(
        long,
        env = "NANOCODEX_COMPUTER",
        default_value_t = false,
        action = ArgAction::SetTrue
    )]
    computer: bool,

    /// Open a loopback live preview with pause/resume/takeover controls.
    #[arg(
        long,
        env = "NANOCODEX_COMPUTER_PREVIEW",
        default_value_t = true,
        action = ArgAction::Set,
        requires = "computer"
    )]
    computer_preview: bool,
}

impl ComputerArgs {
    #[cfg(test)]
    pub(crate) const fn is_enabled(&self) -> bool {
        self.computer
    }

    pub(crate) async fn configure(self) -> Result<Option<ConfiguredComputer>> {
        if !self.computer {
            return Ok(None);
        }
        let (computer, events) = Computer::new().wrap_err("failed to configure computer use")?;
        let preview = if self.computer_preview {
            Some(
                ComputerPreview::spawn_and_open(&computer)
                    .await
                    .wrap_err("failed to start computer preview")?,
            )
        } else {
            None
        };
        let event_task = tokio::spawn(trace_events(events));
        Ok(Some(ConfiguredComputer {
            computer,
            preview,
            event_task,
        }))
    }
}

pub(crate) struct ConfiguredComputer {
    computer: Computer,
    preview: Option<ComputerPreview>,
    event_task: tokio::task::JoinHandle<()>,
}

impl ConfiguredComputer {
    pub(crate) fn tool(&self) -> ComputerTool {
        ComputerTool::from_computer(self.computer.clone())
    }

    pub(crate) fn frames(&self) -> ComputerFrames {
        self.computer.frames()
    }

    pub(crate) fn control(&self) -> ComputerControl {
        self.computer.control()
    }

    pub(crate) fn preview_url(&self) -> Option<&str> {
        self.preview.as_ref().map(ComputerPreview::url)
    }

    pub(crate) async fn shutdown(self) -> Result<()> {
        self.computer.stop();
        drop(self.preview);
        let mut event_task = self.event_task;
        if tokio::time::timeout(std::time::Duration::from_secs(1), &mut event_task)
            .await
            .is_err()
        {
            event_task.abort();
            let _ = event_task.await;
        }
        Ok(())
    }
}

async fn trace_events(mut events: nanocodex_computer::ComputerEvents) {
    while let Some(event) = events.recv().await {
        trace_event(&event);
        if matches!(event, ComputerEvent::Stopped) {
            return;
        }
    }
}

fn trace_event(event: &ComputerEvent) {
    match serde_json::to_string(event) {
        Ok(content) => tracing::info!(
            target: "nanocodex_computer",
            computer_event = content,
            "computer observed ordered lifecycle content"
        ),
        Err(error) => tracing::warn!(
            target: "nanocodex_computer",
            %error,
            "failed to serialize computer lifecycle event"
        ),
    }
}
