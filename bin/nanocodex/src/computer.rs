use clap::{ArgAction, Args};
use eyre::{Result, WrapErr};
use nanocodex_computer::{
    Computer, ComputerControl, ComputerEvent, ComputerPip, ComputerPreview, ComputerTool,
};

/// Native computer-use configuration for normal agent sessions.
#[derive(Args)]
pub(crate) struct ComputerArgs {
    /// Expose the local macOS desktop to Code Mode as `tools.computer`.
    ///
    /// Enabled by default on macOS. Pass `--computer=false` to disable it.
    #[arg(
        long,
        env = "NANOCODEX_COMPUTER",
        default_value_t = default_computer_enabled(),
        action = ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    computer: bool,

    /// Open a non-activating native PIP and loopback takeover controls.
    #[arg(
        long,
        env = "NANOCODEX_COMPUTER_PREVIEW",
        default_value_t = true,
        action = ArgAction::Set
    )]
    computer_preview: bool,

    /// Restrict native computer use to these exact application bundle IDs.
    #[arg(
        long = "computer-allow-app",
        env = "NANOCODEX_COMPUTER_ALLOW_APP",
        value_delimiter = ','
    )]
    allowed_apps: Vec<String>,

    /// Restrict browser documents and links to these exact HTTP(S) origins.
    #[arg(
        long = "computer-allow-url",
        env = "NANOCODEX_COMPUTER_ALLOW_URL",
        value_delimiter = ','
    )]
    allowed_urls: Vec<String>,
}

impl Default for ComputerArgs {
    fn default() -> Self {
        Self {
            computer: default_computer_enabled(),
            computer_preview: true,
            allowed_apps: Vec::new(),
            allowed_urls: Vec::new(),
        }
    }
}

const fn default_computer_enabled() -> bool {
    cfg!(target_os = "macos")
}

impl ComputerArgs {
    #[cfg(test)]
    pub(crate) const fn is_enabled(&self) -> bool {
        self.computer
    }

    pub(crate) async fn configure(self) -> Result<Option<ConfiguredComputer>> {
        if !self.computer {
            if !self.allowed_apps.is_empty() || !self.allowed_urls.is_empty() {
                return Err(eyre::eyre!(
                    "computer allowlists require computer use to be enabled"
                ));
            }
            return Ok(None);
        }
        let mut builder = Computer::builder();
        for bundle_id in self.allowed_apps {
            builder = builder.allow_bundle_id(bundle_id);
        }
        for origin in self.allowed_urls {
            builder = builder.allow_url_origin(origin);
        }
        let (computer, events) = builder
            .build()
            .wrap_err("failed to configure computer use")?;
        let (preview, pip) = if self.computer_preview {
            let preview = ComputerPreview::spawn(&computer)
                .await
                .wrap_err("failed to start computer preview controls")?;
            let pip = ComputerPip::spawn(&computer)
                .await
                .wrap_err("failed to start native computer PIP")?;
            (Some(preview), Some(pip))
        } else {
            (None, None)
        };
        let event_task = tokio::spawn(trace_events(events));
        Ok(Some(ConfiguredComputer {
            computer,
            preview,
            pip,
            event_task,
        }))
    }
}

pub(crate) struct ConfiguredComputer {
    computer: Computer,
    preview: Option<ComputerPreview>,
    pip: Option<ComputerPip>,
    event_task: tokio::task::JoinHandle<()>,
}

impl ConfiguredComputer {
    pub(crate) fn tool(&self) -> ComputerTool {
        ComputerTool::from_computer(self.computer.clone())
    }

    pub(crate) fn control(&self) -> ComputerControl {
        self.computer.control()
    }

    pub(crate) fn preview_url(&self) -> Option<&str> {
        self.preview.as_ref().map(ComputerPreview::url)
    }

    pub(crate) async fn shutdown(self) -> Result<()> {
        self.computer.stop();
        drop(self.pip);
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
