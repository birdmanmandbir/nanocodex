use std::{
    collections::HashMap,
    io,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use clap::{ArgAction, Args, builder::NonEmptyStringValueParser};
use eyre::{Result, WrapErr};
use nanocodex::TurnUsage;
use nanocodex_rlm::RlmUsage;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const DEFAULT_MAX_CONTINUATIONS: u16 = 3;
const DEFAULT_MAX_TURNS: u16 = 12;
const DEFAULT_MAX_TOKENS: u64 = 80_000;
const DEFAULT_TIMEOUT_SECONDS: u64 = 30 * 60;
const DEFAULT_GATE_RETRIES: u16 = 3;
const DEFAULT_GATE_TIMEOUT_SECONDS: u64 = 5 * 60;
const DEFAULT_REFINE_EVERY: u16 = 25;
const MAX_GATE_OUTPUT_BYTES: usize = 6 * 1024;

const CONTINUATION_PROMPT: &str = "No human input is available in autonomous mode. Continue working until the configured quality gates pass or the host limits stop the run. Make reasonable assumptions and verify them. If blocked, preserve host-observable evidence and keep looking for safe progress while budget remains. Do not declare success without terminal evidence.";

#[derive(Args)]
pub(crate) struct AutonomousArgs {
    /// Continue this retained session until quality gates pass or a limit is reached.
    #[arg(long, action = ArgAction::SetTrue)]
    autonomous: bool,

    /// Shell command that must pass before autonomous completion; repeatable.
    #[arg(long = "autonomous-gate", value_parser = NonEmptyStringValueParser::new())]
    gates: Vec<String>,

    /// Maximum host-injected autonomous follow-on prompts.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    autonomous_max_continuations: Option<u16>,

    /// Maximum completed root turns in this autonomous run.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    autonomous_max_turns: Option<u16>,

    /// Maximum non-cached root and recursive tokens in this autonomous run.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    autonomous_max_tokens: Option<u64>,

    /// Maximum autonomous wall-clock duration in seconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    autonomous_timeout_seconds: Option<u64>,

    /// Maximum failed attempts for each autonomous gate.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    autonomous_gate_retries: Option<u16>,

    /// Maximum wall-clock duration for one gate command in seconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    autonomous_gate_timeout_seconds: Option<u64>,

    /// Ask an RLM session to run one evidence review every N completed turns.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    autonomous_refine_every: Option<u16>,
}

impl AutonomousArgs {
    pub(crate) const fn enabled(&self) -> bool {
        self.autonomous
            || !self.gates.is_empty()
            || self.autonomous_max_continuations.is_some()
            || self.autonomous_max_turns.is_some()
            || self.autonomous_max_tokens.is_some()
            || self.autonomous_timeout_seconds.is_some()
            || self.autonomous_gate_retries.is_some()
            || self.autonomous_gate_timeout_seconds.is_some()
            || self.autonomous_refine_every.is_some()
    }

    pub(crate) fn start(&self) -> Option<AutonomousRun> {
        self.enabled().then(|| AutonomousRun {
            started_at: Instant::now(),
            continuations: 0,
            turns: 0,
            non_cached_tokens: 0,
            recursive_non_cached_tokens_seen: 0,
            max_continuations: self
                .autonomous_max_continuations
                .unwrap_or(DEFAULT_MAX_CONTINUATIONS),
            max_turns: self.autonomous_max_turns.unwrap_or(DEFAULT_MAX_TURNS),
            max_tokens: self.autonomous_max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            timeout: Duration::from_secs(
                self.autonomous_timeout_seconds
                    .unwrap_or(DEFAULT_TIMEOUT_SECONDS),
            ),
            gates: self.gates.clone(),
            gate_retries: self.autonomous_gate_retries.unwrap_or(DEFAULT_GATE_RETRIES),
            gate_timeout: Duration::from_secs(
                self.autonomous_gate_timeout_seconds
                    .unwrap_or(DEFAULT_GATE_TIMEOUT_SECONDS),
            ),
            gate_attempts: HashMap::new(),
            refine_every: self.autonomous_refine_every.unwrap_or(DEFAULT_REFINE_EVERY),
        })
    }
}

pub(crate) struct AutonomousRun {
    started_at: Instant,
    continuations: u16,
    turns: u16,
    non_cached_tokens: u64,
    recursive_non_cached_tokens_seen: u64,
    max_continuations: u16,
    max_turns: u16,
    max_tokens: u64,
    timeout: Duration,
    gates: Vec<String>,
    gate_retries: u16,
    gate_timeout: Duration,
    gate_attempts: HashMap<String, u16>,
    refine_every: u16,
}

pub(crate) enum AutonomousDecision {
    Continue(String),
    Stop(AutonomousStop),
}

pub(crate) enum AutonomousStop {
    GatesPassed,
    LimitReached {
        limit: &'static str,
        gates_pending: bool,
    },
    GateRetriesExhausted {
        command: String,
        attempts: u16,
    },
}

impl AutonomousRun {
    pub(crate) const fn record_turn(&mut self, usage: &TurnUsage) {
        self.turns = self.turns.saturating_add(1);
        self.non_cached_tokens = self.non_cached_tokens.saturating_add(non_cached_tokens(
            usage.input_tokens(),
            usage.cached_input_tokens(),
            usage.output_tokens(),
        ));
    }

    pub(crate) fn record_recursive_usage(&mut self, usage: &RlmUsage) {
        let observed = non_cached_tokens(
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
        );
        let delta = observed.saturating_sub(self.recursive_non_cached_tokens_seen);
        self.recursive_non_cached_tokens_seen = self.recursive_non_cached_tokens_seen.max(observed);
        self.non_cached_tokens = self.non_cached_tokens.saturating_add(delta);
    }

    pub(crate) async fn decide(
        &mut self,
        cwd: &Path,
        rlm_enabled: bool,
    ) -> Result<AutonomousDecision> {
        if !self.gates.is_empty() {
            for command in self.gates.clone() {
                let result = run_gate(&command, cwd, self.gate_timeout)
                    .await
                    .wrap_err_with(|| format!("failed to execute autonomous gate `{command}`"))?;
                if result.success {
                    self.gate_attempts.insert(command, 0);
                    continue;
                }
                let attempts = {
                    let attempts = self.gate_attempts.entry(command.clone()).or_default();
                    *attempts = attempts.saturating_add(1);
                    *attempts
                };
                if attempts > self.gate_retries {
                    return Ok(AutonomousDecision::Stop(
                        AutonomousStop::GateRetriesExhausted { command, attempts },
                    ));
                }
                if let Some(limit) = self.limit() {
                    return Ok(AutonomousDecision::Stop(AutonomousStop::LimitReached {
                        limit,
                        gates_pending: true,
                    }));
                }
                self.continuations = self.continuations.saturating_add(1);
                return Ok(AutonomousDecision::Continue(self.failure_prompt(
                    &command,
                    attempts,
                    &result,
                    rlm_enabled,
                )));
            }
            return Ok(AutonomousDecision::Stop(AutonomousStop::GatesPassed));
        }

        if let Some(limit) = self.limit() {
            return Ok(AutonomousDecision::Stop(AutonomousStop::LimitReached {
                limit,
                gates_pending: false,
            }));
        }
        self.continuations = self.continuations.saturating_add(1);
        Ok(AutonomousDecision::Continue(
            self.continuation_prompt(rlm_enabled),
        ))
    }

    fn limit(&self) -> Option<&'static str> {
        if self.continuations >= self.max_continuations {
            return Some("maximum continuations");
        }
        if self.turns >= self.max_turns {
            return Some("maximum turns");
        }
        if self.non_cached_tokens >= self.max_tokens {
            return Some("maximum non-cached tokens");
        }
        (self.started_at.elapsed() >= self.timeout).then_some("wall-clock timeout")
    }

    fn refinement_checkpoint(&self, rlm_enabled: bool) -> Option<&'static str> {
        (rlm_enabled && self.turns.is_multiple_of(self.refine_every)).then_some(
            "Before continuing the task, review the concrete trajectory and gate evidence. If it reveals a repeated failure or reusable tactic, call refine_harness with one concise evidence-backed observation, wait for that refiner to finish, and verify the resulting harness revision. Keep the immutable base prompt unchanged.",
        )
    }

    fn continuation_prompt(&self, rlm_enabled: bool) -> String {
        match self.refinement_checkpoint(rlm_enabled) {
            Some(checkpoint) => format!("{CONTINUATION_PROMPT}\n\n{checkpoint}"),
            None => CONTINUATION_PROMPT.to_owned(),
        }
    }

    fn failure_prompt(
        &self,
        command: &str,
        attempt: u16,
        result: &GateResult,
        rlm_enabled: bool,
    ) -> String {
        let mut prompt = format!(
            "Autonomous quality gate failed (attempt {attempt}/{}): `{command}` {}.\n\nOutput:\n{}\n\nContinue working, fix the failure, and produce host-observable terminal evidence.",
            self.gate_retries, result.exit, result.output
        );
        if let Some(checkpoint) = self.refinement_checkpoint(rlm_enabled) {
            prompt.push_str("\n\n");
            prompt.push_str(checkpoint);
        }
        prompt
    }
}

impl std::fmt::Display for AutonomousStop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GatesPassed => formatter.write_str("all quality gates passed"),
            Self::LimitReached {
                limit,
                gates_pending: true,
            } => write!(
                formatter,
                "stopped at {limit} with quality gates still failing"
            ),
            Self::LimitReached {
                limit,
                gates_pending: false,
            } => write!(formatter, "stopped at {limit}"),
            Self::GateRetriesExhausted { command, attempts } => write!(
                formatter,
                "gate `{command}` remained failing after {attempts} attempts"
            ),
        }
    }
}

impl AutonomousStop {
    pub(crate) const fn is_failure(&self) -> bool {
        matches!(
            self,
            Self::LimitReached {
                gates_pending: true,
                ..
            } | Self::GateRetriesExhausted { .. }
        )
    }
}

const fn non_cached_tokens(input: u64, cached_input: u64, output: u64) -> u64 {
    input.saturating_sub(cached_input).saturating_add(output)
}

struct GateResult {
    success: bool,
    exit: String,
    output: String,
}

async fn run_gate(command: &str, cwd: &Path, timeout: Duration) -> io::Result<GateResult> {
    let mut process = shell_command(command);
    process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);

    let mut child = process.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("autonomous gate started without a process identifier"))?;
    let mut process_group = ProcessGroupGuard::new(pid)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("autonomous gate stdout was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("autonomous gate stderr was unavailable"))?;
    let stdout = tokio::spawn(read_bounded(stdout, MAX_GATE_OUTPUT_BYTES));
    let stderr = tokio::spawn(read_bounded(stderr, MAX_GATE_OUTPUT_BYTES));

    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (status?, false),
        Err(_) => {
            process_group.terminate_and_disarm()?;
            (child.wait().await?, true)
        }
    };
    process_group.terminate_and_disarm()?;
    let stdout = stdout
        .await
        .map_err(|error| io::Error::other(error.to_string()))??;
    let stderr = stderr
        .await
        .map_err(|error| io::Error::other(error.to_string()))??;
    let output = bounded_output(stdout, stderr);
    let exit = if timed_out {
        format!("timed out after {} seconds", timeout.as_secs())
    } else {
        status.code().map_or_else(
            || "was terminated by a signal".to_owned(),
            |code| format!("exited {code}"),
        )
    };
    Ok(GateResult {
        success: !timed_out && status.success(),
        exit,
        output,
    })
}

fn shell_command(command: &str) -> Command {
    #[cfg(unix)]
    {
        let mut process = Command::new("/bin/sh");
        process.args(["-lc", command]);
        process
    }
    #[cfg(windows)]
    {
        let mut process = Command::new("cmd.exe");
        process.args(["/D", "/S", "/C", command]);
        process
    }
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin, limit: usize) -> io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(retained)
}

fn bounded_output(stdout: Vec<u8>, stderr: Vec<u8>) -> String {
    let mut output = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr);
    if !output.is_empty() && !stderr.is_empty() {
        output.push('\n');
    }
    output.push_str(&stderr);
    if output.trim().is_empty() {
        "(no output)".to_owned()
    } else {
        output
    }
}

struct ProcessGroupGuard {
    #[cfg(unix)]
    process_group: Option<nix::unistd::Pid>,
    #[cfg(not(unix))]
    process_group: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(pid: u32) -> io::Result<Self> {
        #[cfg(unix)]
        let process_group = Some(nix::unistd::Pid::from_raw(
            i32::try_from(pid)
                .map_err(|_| io::Error::other("process identifier exceeds i32::MAX"))?,
        ));
        #[cfg(not(unix))]
        let process_group = Some(pid);
        Ok(Self { process_group })
    }

    #[cfg(unix)]
    fn terminate(&self) -> io::Result<()> {
        use nix::{
            errno::Errno,
            sys::signal::{Signal, killpg},
        };

        let Some(process_group) = self.process_group else {
            return Ok(());
        };
        match killpg(process_group, Signal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }

    #[cfg(windows)]
    fn terminate(&self) -> io::Result<()> {
        let Some(process_group) = self.process_group else {
            return Ok(());
        };
        std::process::Command::new("taskkill.exe")
            .args(["/PID", &process_group.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|_| ())
    }

    #[cfg(not(any(unix, windows)))]
    fn terminate(&self) -> io::Result<()> {
        Ok(())
    }

    fn terminate_and_disarm(&mut self) -> io::Result<()> {
        self.terminate()?;
        self.process_group = None;
        Ok(())
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomous_suboptions_enable_the_host_policy() {
        let args = AutonomousArgs {
            autonomous: false,
            gates: Vec::new(),
            autonomous_max_continuations: None,
            autonomous_max_turns: Some(2),
            autonomous_max_tokens: None,
            autonomous_timeout_seconds: None,
            autonomous_gate_retries: None,
            autonomous_gate_timeout_seconds: None,
            autonomous_refine_every: None,
        };
        assert!(args.enabled());
    }

    #[test]
    fn refinement_checkpoint_is_added_without_replacing_the_continuation() {
        let mut run = AutonomousArgs {
            autonomous: true,
            gates: Vec::new(),
            autonomous_max_continuations: None,
            autonomous_max_turns: None,
            autonomous_max_tokens: None,
            autonomous_timeout_seconds: None,
            autonomous_gate_retries: None,
            autonomous_gate_timeout_seconds: None,
            autonomous_refine_every: Some(2),
        }
        .start()
        .unwrap();
        run.turns = 2;
        let prompt = run.continuation_prompt(true);
        assert!(prompt.contains(CONTINUATION_PROMPT));
        assert!(prompt.contains("call refine_harness"));
        assert!(
            !run.continuation_prompt(false)
                .contains("call refine_harness")
        );
    }

    #[test]
    fn cached_input_does_not_consume_the_autonomous_token_budget() {
        assert_eq!(non_cached_tokens(100, 90, 10), 20);
        assert_eq!(non_cached_tokens(5, 10, 2), 2);
    }

    #[test]
    fn recursive_usage_only_adds_each_observed_child_token_once() {
        let mut run = AutonomousArgs {
            autonomous: true,
            gates: Vec::new(),
            autonomous_max_continuations: None,
            autonomous_max_turns: None,
            autonomous_max_tokens: None,
            autonomous_timeout_seconds: None,
            autonomous_gate_retries: None,
            autonomous_gate_timeout_seconds: None,
            autonomous_refine_every: None,
        }
        .start()
        .unwrap();
        let first = RlmUsage {
            input_tokens: 100,
            cached_input_tokens: 80,
            output_tokens: 10,
            ..RlmUsage::default()
        };
        run.record_recursive_usage(&first);
        run.record_recursive_usage(&first);
        assert_eq!(run.non_cached_tokens, 30);

        let second = RlmUsage {
            input_tokens: 150,
            cached_input_tokens: 110,
            output_tokens: 20,
            ..RlmUsage::default()
        };
        run.record_recursive_usage(&second);
        assert_eq!(run.non_cached_tokens, 60);
    }

    #[test]
    fn empty_gate_output_is_explicit() {
        assert_eq!(bounded_output(Vec::new(), Vec::new()), "(no output)");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gate_captures_bounded_failure_evidence() {
        let temporary = tempfile::tempdir().unwrap();
        let result = run_gate(
            "printf 'diagnostic' >&2; exit 7",
            temporary.path(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!result.success);
        assert_eq!(result.exit, "exited 7");
        assert_eq!(result.output, "diagnostic");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gate_timeout_is_a_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let result = run_gate("sleep 30", temporary.path(), Duration::from_millis(10))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.exit.starts_with("timed out after"));
    }
}
