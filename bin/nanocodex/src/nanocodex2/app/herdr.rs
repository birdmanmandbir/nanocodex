// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Best-effort lifecycle reporting for Nanocodex2 sessions hosted by Herdr.

use std::{
    env,
    ffi::{OsStr, OsString},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

const AGENT: &str = "nanocodex2";
const SOURCE: &str = "herdr:nanocodex2";

pub(crate) struct Reporter(Option<ActiveReporter>);

impl Reporter {
    pub(crate) fn from_env(session_id: &str) -> Self {
        let Some((binary, pane_id)) = environment(
            env::var("HERDR_ENV").ok(),
            env::var_os("HERDR_BIN_PATH"),
            env::var("HERDR_PANE_ID").ok(),
        ) else {
            return Self(None);
        };
        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_mul(1_000)
            .min(u128::from(u64::MAX)) as u64;
        let mut reporter = ActiveReporter {
            binary,
            pane_id,
            session_id: session_id.to_owned(),
            sequence,
        };
        reporter.report(State::Idle);
        Self(Some(reporter))
    }

    pub(crate) fn working(&mut self, session_id: Option<&str>) {
        if let Some(reporter) = &mut self.0 {
            reporter.update_session(session_id);
            reporter.report(State::Working);
        }
    }

    pub(crate) fn idle(&mut self, session_id: Option<&str>) {
        if let Some(reporter) = &mut self.0 {
            reporter.update_session(session_id);
            reporter.report(State::Idle);
        }
    }
}

struct ActiveReporter {
    binary: OsString,
    pane_id: String,
    session_id: String,
    sequence: u64,
}

impl ActiveReporter {
    fn update_session(&mut self, session_id: Option<&str>) {
        if let Some(session_id) = session_id {
            session_id.clone_into(&mut self.session_id);
        }
    }

    fn report(&mut self, state: State) {
        let sequence = self.next_sequence();
        spawn(command(
            &self.binary,
            &self.pane_id,
            Report::State {
                state,
                session_id: &self.session_id,
            },
            sequence,
        ));
    }

    fn next_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        sequence
    }
}

impl Drop for ActiveReporter {
    fn drop(&mut self) {
        let sequence = self.next_sequence();
        spawn(command(
            &self.binary,
            &self.pane_id,
            Report::Release,
            sequence,
        ));
    }
}

#[derive(Clone, Copy)]
enum State {
    Idle,
    Working,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
        }
    }
}

enum Report<'a> {
    State { state: State, session_id: &'a str },
    Release,
}

fn environment(
    herdr_env: Option<String>,
    binary: Option<OsString>,
    pane_id: Option<String>,
) -> Option<(OsString, String)> {
    if herdr_env.as_deref() != Some("1") {
        return None;
    }
    Some((
        binary
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| "herdr".into()),
        pane_id.filter(|pane_id| !pane_id.is_empty())?,
    ))
}

fn command(binary: &OsStr, pane_id: &str, report: Report<'_>, sequence: u64) -> Command {
    let mut command = Command::new(binary);
    command.arg("pane");
    match report {
        Report::State { state, session_id } => {
            command.args([
                "report-agent",
                pane_id,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--state",
                state.as_str(),
                "--agent-session-id",
                session_id,
            ]);
        }
        Report::Release => {
            command.args([
                "release-agent",
                pane_id,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
            ]);
        }
    }
    command
        .args(["--seq", &sequence.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn spawn(mut command: Command) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let Ok(mut child) = command.spawn() else {
        return;
    };
    runtime.spawn(async move {
        drop(child.wait().await);
    });
}
