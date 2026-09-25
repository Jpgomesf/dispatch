//! The only module that starts Claude Code: one `claude -p` process per session, read as a
//! stream of JSON events (`--output-format stream-json --verbose`).

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};

use crate::config::Effort;
use crate::prompts::RUNNER_RULES;

mod stream;

use stream::{Stop, Stream};

/// Why a session ended without a result when the runner stopped it.
pub const INTERRUPTED: &str = "interrupted: dispatch is stopping";

/// Which kind of session: decides the objective, the schema and the limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Triage,
    Card,
    Discussion,
}

impl Mode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Triage => "triage",
            Mode::Card => "card",
            Mode::Discussion => "discussion",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRequest {
    pub mode: Mode,
    pub prompt: String,
    pub model: String,
    pub effort: Effort,
    pub max_budget_usd: f64,
    pub cwd: PathBuf,
    /// Passed as `--plugin-dir` when set.
    pub plugin_dir: Option<PathBuf>,
    pub output_schema: Value,
    pub limits: Limits,
}

/// What keeps a session bounded; the runner ends it gracefully past either limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Wall-clock limit (start-to-close).
    pub timeout: Duration,
    /// No stream event for this long means the session is stuck.
    pub idle_timeout: Duration,
    /// MCP servers that must be connected when the session starts.
    pub required_mcp: Vec<String>,
}

/// How a session ended, judged from the stream and the process, never from what the agent
/// says about its own work.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// A successful `result` with an object `structured_output` (the caller parses it).
    Output(Value),
    /// A `result` reporting an error: API error, budget or turn limit, and the like.
    ApiError(String),
    /// A successful `result` without an object `structured_output`.
    InvalidOutput(String),
    /// The process ended without a `result`.
    Crash(String),
    /// Stopped at the wall-clock timeout.
    Timeout(String),
    /// Stopped by the inactivity watchdog.
    Stuck(String),
    /// A usage or rate limit ended it (not a failure of the work).
    RateLimited { resets_at: Option<DateTime<Utc>> },
    /// Stopped at start: plugin errors, or a required MCP server not connected.
    Environment(String),
    /// Stopped by the kill switch or a signal.
    Interrupted,
}

impl Ended {
    /// The structured output, or why there is none.
    pub fn output(&self) -> Result<Value, String> {
        match self {
            Ended::Output(output) => Ok(output.clone()),
            Ended::ApiError(detail)
            | Ended::InvalidOutput(detail)
            | Ended::Crash(detail)
            | Ended::Timeout(detail)
            | Ended::Stuck(detail)
            | Ended::Environment(detail) => Err(detail.clone()),
            Ended::RateLimited { resets_at: None } => Err("usage or rate limit".into()),
            Ended::RateLimited {
                resets_at: Some(at),
            } => Err(format!(
                "usage or rate limit until {}",
                at.format("%Y-%m-%dT%H:%M:%SZ")
            )),
            Ended::Interrupted => Err(INTERRUPTED.into()),
        }
    }
}

/// What one session produced.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionReport {
    pub ended: Ended,
    /// Claude Code's session id: `claude --resume <id>` from the session's cwd.
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
}

impl SessionReport {
    #[must_use]
    pub fn ended(ended: Ended) -> SessionReport {
        SessionReport {
            ended,
            session_id: None,
            cost_usd: None,
        }
    }
}

/// What a running session tells the runner before it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The stream named the session.
    Started { session_id: String },
    /// A usage or rate limit was hit (the session keeps retrying on its own); the reset
    /// time when an event gave one.
    RateLimited { resets_at: Option<DateTime<Utc>> },
}

/// Runner-wide shutdown level, broadcast to every running session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Shutdown {
    #[default]
    Run,
    /// First signal or kill switch: start nothing new, end running sessions with the stop
    /// sequence (SIGINT, then SIGTERM, then SIGKILL).
    Graceful,
    /// Second signal: kill running sessions (SIGKILL).
    Force,
}

/// What the runner hands a session: the shutdown level to obey, and where to send notices.
#[derive(Debug)]
pub struct Control {
    pub shutdown: watch::Receiver<Shutdown>,
    pub notices: mpsc::UnboundedSender<Notice>,
}

pub trait Session: Send + Sync + 'static {
    fn run(
        &self,
        request: SessionRequest,
        control: Control,
    ) -> impl Future<Output = SessionReport> + Send;
}

/// `claude -p` in print mode streaming JSON events, with structured output, auto permission
/// mode, no permission prompts (nobody is there to answer) and the runner rules appended to
/// the system prompt; user/project settings, skills and MCP servers load as in any Claude Code
/// run.
#[derive(Debug, Clone)]
pub struct ClaudeCli {
    pub program: PathBuf,
    /// How long to wait for stdout/stderr to close after `claude` exits before killing
    /// whatever is left in its process group.
    pub pipe_grace: Duration,
    /// Between the steps of the stop sequence (SIGINT, SIGTERM, SIGKILL).
    pub stop_grace: Duration,
}

impl Default for ClaudeCli {
    fn default() -> Self {
        ClaudeCli {
            program: PathBuf::from("claude"),
            pipe_grace: Duration::from_secs(5),
            stop_grace: Duration::from_secs(20),
        }
    }
}

pub fn build_args(request: &SessionRequest) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        request.prompt.clone(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--json-schema".into(),
        request.output_schema.to_string(),
        "--permission-mode".into(),
        "auto".into(),
        "--permission-prompts".into(),
        "none".into(),
        "--append-system-prompt".into(),
        RUNNER_RULES.into(),
        "--model".into(),
        request.model.clone(),
        "--effort".into(),
        request.effort.as_str().into(),
        "--max-budget-usd".into(),
        request.max_budget_usd.to_string(),
    ];
    if let Some(plugin_dir) = &request.plugin_dir {
        args.extend(["--plugin-dir".into(), plugin_dir.display().to_string()]);
    }
    args
}

/// Signal `claude` alone, or its whole process group (it leads a group of its own), so
/// subprocesses it started are stopped with it.
fn send_signal(pid: Option<u32>, signal: libc::c_int, whole_group: bool) {
    if let Some(pid) = pid.and_then(|p| libc::pid_t::try_from(p).ok()) {
        let target = if whole_group { -pid } else { pid };
        // SAFETY: plain kill(2) on the process (group) we started; no memory is shared.
        unsafe {
            libc::kill(target, signal);
        }
    }
}

fn signal_group(group: Option<u32>, signal: libc::c_int) {
    send_signal(group, signal, true);
}

/// How a session is ended: SIGINT to `claude` first (it ends the turn cleanly), then SIGTERM
/// to its process group, then SIGKILL, `grace` apart. SIGTERM alone would leave the turn
/// unfinished.
#[derive(Debug)]
struct StopSequence {
    pid: Option<u32>,
    grace: Duration,
    started: bool,
    /// The next signal to the group, and when.
    next: Option<(Instant, libc::c_int)>,
}

impl StopSequence {
    fn new(pid: Option<u32>, grace: Duration) -> Self {
        StopSequence {
            pid,
            grace,
            started: false,
            next: None,
        }
    }

    fn begin(&mut self) {
        if !self.started {
            self.started = true;
            send_signal(self.pid, libc::SIGINT, false);
            self.next = Some((Instant::now() + self.grace, libc::SIGTERM));
        }
    }

    fn escalate(&mut self) {
        if let Some((_, signal)) = self.next {
            signal_group(self.pid, signal);
            self.next =
                (signal == libc::SIGTERM).then(|| (Instant::now() + self.grace, libc::SIGKILL));
        }
    }

    fn kill(&mut self) {
        self.started = true;
        signal_group(self.pid, libc::SIGKILL);
        self.next = None;
    }

    fn due(&self) -> Option<Instant> {
        self.next.map(|(at, _)| at)
    }
}

/// `claude`'s stdout, one line at a time, as it arrives.
fn read_lines(
    pipe: impl AsyncRead + Unpin + Send + 'static,
) -> (mpsc::UnboundedReceiver<String>, JoinHandle<()>) {
    let (lines, received) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        let mut pipe = BufReader::new(pipe);
        let mut line = Vec::new();
        loop {
            line.clear();
            match pipe.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if lines
                        .send(String::from_utf8_lossy(&line).into_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    (received, reader)
}

type Buffer = Arc<Mutex<Vec<u8>>>;

/// Read a pipe into a shared buffer, so what was read survives a timeout.
fn drain(mut pipe: impl AsyncRead + Unpin + Send + 'static) -> (Buffer, JoinHandle<()>) {
    let buffer: Buffer = Arc::default();
    let sink = buffer.clone();
    let reader = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        while let Ok(read) = pipe.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            sink.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(&chunk[..read]);
        }
    });
    (buffer, reader)
}

fn text(buffer: &Buffer) -> String {
    let bytes = buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    String::from_utf8_lossy(&bytes).into_owned()
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

impl Session for ClaudeCli {
    async fn run(&self, request: SessionRequest, control: Control) -> SessionReport {
        let Control {
            mut shutdown,
            notices,
        } = control;
        if *shutdown.borrow_and_update() != Shutdown::Run {
            return SessionReport::ended(Ended::Interrupted);
        }
        let spawned = Command::new(&self.program)
            .args(build_args(&request))
            .current_dir(&request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                let detail = format!("cannot start {}: {e}", self.program.display());
                return SessionReport::ended(Ended::Crash(detail));
            }
        };
        let group = child.id();
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            signal_group(group, libc::SIGKILL);
            return SessionReport::ended(Ended::Crash("claude's output is not piped".into()));
        };
        let (mut lines, read_out) = read_lines(stdout);
        let (stderr, mut read_err) = drain(stderr);

        let limits = &request.limits;
        let deadline = Instant::now() + limits.timeout;
        let mut last_event = Instant::now();
        let mut stream = Stream::new(limits.required_mcp.clone());
        let mut stop = None;
        let mut stopping = StopSequence::new(group, self.stop_grace);
        let mut exited: Option<String> = None;
        let mut stdout_open = true;
        let mut pipe_deadline: Option<Instant> = None;
        let mut watching_shutdown = true;
        while exited.is_none() || stdout_open {
            let running = exited.is_none() && stop.is_none();
            let escalation = stopping.due().filter(|_| exited.is_none());
            tokio::select! {
                line = lines.recv(), if stdout_open => match line {
                    Some(line) => {
                        last_event = Instant::now();
                        let observed = stream.observe(&line);
                        for notice in observed.notices {
                            let _ = notices.send(notice);
                        }
                        if let Some(reason) = observed.stop
                            && stop.is_none()
                        {
                            stop = Some(reason);
                            stopping.begin();
                        }
                    }
                    None => stdout_open = false,
                },
                () = sleep_until(deadline), if running => {
                    stop = Some(Stop::Timeout(limits.timeout));
                    stopping.begin();
                }
                () = sleep_until(last_event + limits.idle_timeout), if running => {
                    stop = Some(Stop::Idle(limits.idle_timeout));
                    stopping.begin();
                }
                () = sleep_until(escalation.unwrap_or_else(Instant::now)), if escalation.is_some() => {
                    stopping.escalate();
                }
                status = child.wait(), if exited.is_none() => {
                    exited = Some(match status {
                        Ok(status) => status.to_string(),
                        Err(e) => format!("waiting for claude: {e}"),
                    });
                    pipe_deadline = Some(Instant::now() + self.pipe_grace);
                }
                () = sleep_until(pipe_deadline.unwrap_or_else(Instant::now)), if pipe_deadline.is_some() => {
                    // A leftover subprocess holding stdout open must not pin this session.
                    signal_group(group, libc::SIGKILL);
                    read_out.abort();
                    pipe_deadline = None;
                }
                changed = shutdown.changed(), if watching_shutdown => {
                    if changed.is_err() {
                        watching_shutdown = false;
                        continue;
                    }
                    let level = *shutdown.borrow_and_update();
                    match level {
                        Shutdown::Run => {}
                        Shutdown::Graceful => {
                            stop.get_or_insert(Stop::Interrupted);
                            stopping.begin();
                        }
                        Shutdown::Force => {
                            stop.get_or_insert(Stop::Interrupted);
                            stopping.kill();
                        }
                    }
                }
            }
        }
        if timeout(self.pipe_grace, &mut read_err).await.is_err() {
            signal_group(group, libc::SIGKILL);
            read_err.abort();
        }
        let exited = exited.unwrap_or_default();
        let stderr = text(&stderr);
        let detail = match last_line(&stderr) {
            "" => format!("claude exited with {exited}"),
            tail => format!("claude exited with {exited}: {tail}"),
        };
        stream.finish(stop, &detail)
    }
}

#[cfg(test)]
mod tests;
