//! The only module that starts Claude Code: one `claude -p` process per session.

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::config::Effort;
use crate::prompts::RUNNER_RULES;

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
}

/// What one session produced.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionReport {
    /// The structured output, or why there is none.
    pub output: Result<Value, String>,
    /// Claude Code's session id: `claude --resume <id>` from the session's cwd.
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
}

impl SessionReport {
    #[must_use]
    pub fn failed(reason: impl Into<String>) -> SessionReport {
        SessionReport {
            output: Err(reason.into()),
            session_id: None,
            cost_usd: None,
        }
    }
}

/// Runner-wide shutdown level, broadcast to every running session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Shutdown {
    #[default]
    Run,
    /// First signal or kill switch: start nothing new, ask running sessions to end (SIGTERM).
    Graceful,
    /// Second signal: kill running sessions (SIGKILL).
    Force,
}

pub trait Session: Send + Sync + 'static {
    fn run(
        &self,
        request: SessionRequest,
        shutdown: watch::Receiver<Shutdown>,
    ) -> impl Future<Output = SessionReport> + Send;
}

/// `claude -p` in print mode with JSON structured output, auto permission mode, no permission
/// prompts (nobody is there to answer) and the runner rules appended to the system prompt;
/// user/project settings, skills and MCP servers load as in any Claude Code run.
#[derive(Debug, Clone)]
pub struct ClaudeCli {
    pub program: PathBuf,
    /// How long to wait for stdout/stderr to close after `claude` exits before killing
    /// whatever is left in its process group.
    pub pipe_grace: Duration,
}

impl Default for ClaudeCli {
    fn default() -> Self {
        ClaudeCli {
            program: PathBuf::from("claude"),
            pipe_grace: Duration::from_secs(5),
        }
    }
}

pub fn build_args(request: &SessionRequest) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        request.prompt.clone(),
        "--output-format".into(),
        "json".into(),
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

/// Parse the final `{"type": "result", ...}` object printed by `--output-format json`.
pub fn report_from_stdout(stdout: &str) -> SessionReport {
    let parsed = serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .or_else(|| {
            stdout
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        });
    let Some(result) = parsed.filter(|v| v["type"] == "result") else {
        return SessionReport::failed("session ended without a result");
    };
    let structured = &result["structured_output"];
    let output = if result["is_error"] == Value::Bool(true) || !structured.is_object() {
        let errors: Vec<&str> = result["errors"]
            .as_array()
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let detail = if errors.is_empty() {
            result["subtype"]
                .as_str()
                .unwrap_or("no structured output")
                .to_string()
        } else {
            errors.join("; ")
        };
        Err(format!("session failed: {detail}"))
    } else {
        Ok(structured.clone())
    };
    SessionReport {
        output,
        session_id: result["session_id"].as_str().map(str::to_string),
        cost_usd: result["total_cost_usd"].as_f64(),
    }
}

/// Signal every process in the session's group (the child leads a group of its own), so
/// subprocesses `claude` started are stopped with it.
fn signal_group(group: Option<u32>, signal: libc::c_int) {
    if let Some(group) = group.and_then(|p| libc::pid_t::try_from(p).ok()) {
        // SAFETY: plain kill(2) on the process group we created; no memory is shared.
        unsafe {
            libc::kill(-group, signal);
        }
    }
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
                .expect("pipe buffer")
                .extend_from_slice(&chunk[..read]);
        }
    });
    (buffer, reader)
}

fn text(buffer: &Buffer) -> String {
    String::from_utf8_lossy(&buffer.lock().expect("pipe buffer")).into_owned()
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

impl Session for ClaudeCli {
    async fn run(
        &self,
        request: SessionRequest,
        mut shutdown: watch::Receiver<Shutdown>,
    ) -> SessionReport {
        if *shutdown.borrow_and_update() != Shutdown::Run {
            return SessionReport::failed("not started: dispatch is stopping");
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
                return SessionReport::failed(format!(
                    "cannot start {}: {e}",
                    self.program.display()
                ));
            }
        };
        let group = child.id();
        let (stdout, mut read_out) = drain(child.stdout.take().expect("piped stdout"));
        let (stderr, mut read_err) = drain(child.stderr.take().expect("piped stderr"));

        let mut terminated = false;
        let waited = loop {
            tokio::select! {
                status = child.wait() => break status,
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        break child.wait().await;
                    }
                    match *shutdown.borrow_and_update() {
                        Shutdown::Run => {}
                        Shutdown::Graceful => {
                            terminated = true;
                            signal_group(group, libc::SIGTERM);
                        }
                        Shutdown::Force => {
                            terminated = true;
                            signal_group(group, libc::SIGKILL);
                        }
                    }
                }
            }
        };
        let status = match waited {
            Ok(status) => status,
            Err(e) => return SessionReport::failed(format!("waiting for claude: {e}")),
        };

        // A leftover subprocess holding a pipe open must not pin this session forever.
        let pipes_closed = timeout(self.pipe_grace, async {
            let _ = tokio::join!(&mut read_out, &mut read_err);
        })
        .await;
        if pipes_closed.is_err() {
            signal_group(group, libc::SIGKILL);
            let after_kill = timeout(Duration::from_secs(1), async {
                let _ = tokio::join!(&mut read_out, &mut read_err);
            })
            .await;
            if after_kill.is_err() {
                read_out.abort();
                read_err.abort();
            }
        }
        let (stdout, stderr) = (text(&stdout), text(&stderr));
        let mut report = report_from_stdout(&stdout);
        if terminated {
            report.output = Err("terminated: dispatch is stopping".into());
        } else if stdout.trim().is_empty() {
            report.output = Err(format!(
                "claude exited with {status}: {}",
                last_line(&stderr)
            ));
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> SessionRequest {
        SessionRequest {
            mode: Mode::Triage,
            prompt: "Check the new activity.".into(),
            model: "sonnet".into(),
            effort: Effort::Medium,
            max_budget_usd: 1.5,
            cwd: PathBuf::from("/tmp/example/state"),
            plugin_dir: Some(PathBuf::from("/tmp/example/plugin")),
            output_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn builds_documented_print_mode_args() {
        let args = build_args(&request());
        assert_eq!(
            args,
            [
                "-p",
                "Check the new activity.",
                "--output-format",
                "json",
                "--json-schema",
                r#"{"type":"object"}"#,
                "--permission-mode",
                "auto",
                "--permission-prompts",
                "none",
                "--append-system-prompt",
                RUNNER_RULES,
                "--model",
                "sonnet",
                "--effort",
                "medium",
                "--max-budget-usd",
                "1.5",
                "--plugin-dir",
                "/tmp/example/plugin",
            ]
        );
        let whole = SessionRequest {
            max_budget_usd: 20.0,
            ..request()
        };
        assert!(build_args(&whole).contains(&"20".to_string()));
        let no_plugin = SessionRequest {
            plugin_dir: None,
            ..request()
        };
        assert!(!build_args(&no_plugin).contains(&"--plugin-dir".to_string()));
    }

    fn result(overrides: Value) -> String {
        let mut base = json!({
            "type": "result", "subtype": "success", "is_error": false,
            "total_cost_usd": 0.42, "structured_output": {"summary": "ok"},
            "result": "{\"summary\":\"ok\"}", "session_id": "session-example"
        });
        for (key, value) in overrides.as_object().unwrap() {
            base[key] = value.clone();
        }
        base.to_string()
    }

    #[test]
    fn report_from_success() {
        let report = report_from_stdout(&result(json!({})));
        assert_eq!(report.output, Ok(json!({"summary": "ok"})));
        assert_eq!(report.cost_usd, Some(0.42));
        assert_eq!(report.session_id.as_deref(), Some("session-example"));
        let with_noise = format!("warning: something\n{}\n", result(json!({})));
        assert!(report_from_stdout(&with_noise).output.is_ok());
    }

    #[test]
    fn report_from_failures() {
        let budget = result(json!({
            "is_error": true, "subtype": "error_max_budget_usd",
            "errors": ["Reached maximum budget ($0.05)"], "structured_output": null
        }));
        let report = report_from_stdout(&budget);
        assert_eq!(
            report.output,
            Err("session failed: Reached maximum budget ($0.05)".into())
        );
        assert_eq!(report.session_id.as_deref(), Some("session-example"));
        let subtype_only = result(json!({"is_error": true, "subtype": "error_during_execution"}));
        assert!(
            report_from_stdout(&subtype_only)
                .output
                .unwrap_err()
                .contains("error_during_execution")
        );
        for bad in [
            String::new(),
            "not json".to_string(),
            json!({"type": "assistant"}).to_string(),
            result(json!({"structured_output": null})),
            result(json!({"structured_output": "plain text"})),
        ] {
            assert!(report_from_stdout(&bad).output.is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn missing_program_is_a_session_error() {
        let cli = ClaudeCli {
            program: PathBuf::from("/nonexistent/claude-example"),
            ..ClaudeCli::default()
        };
        let (_tx, rx) = watch::channel(Shutdown::Run);
        let request = SessionRequest {
            cwd: std::env::temp_dir(),
            ..request()
        };
        let error = cli.run(request, rx).await.output.unwrap_err();
        assert!(error.starts_with("cannot start"), "{error}");
    }

    /// A fake `claude`: a shell script with the given body.
    fn fake_cli(dir: &std::path::Path, body: &str) -> ClaudeCli {
        let script = dir.join("fake-claude");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        ClaudeCli {
            program: script,
            pipe_grace: Duration::from_millis(300),
        }
    }

    fn in_dir(dir: &std::path::Path) -> SessionRequest {
        SessionRequest {
            cwd: dir.to_path_buf(),
            ..request()
        }
    }

    fn is_alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks that the pid exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn graceful_shutdown_terminates_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let body = format!("sleep 30 &\necho $! > {}\nsleep 30", pid_file.display());
        let cli = fake_cli(dir.path(), &body);
        let (tx, rx) = watch::channel(Shutdown::Run);
        let request = in_dir(dir.path());
        let run = tokio::spawn(async move { cli.run(request, rx).await });
        let written =
            |p: &std::path::Path| std::fs::read_to_string(p).is_ok_and(|t| t.ends_with('\n'));
        for _ in 0..100 {
            if written(&pid_file) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tx.send_replace(Shutdown::Graceful);
        let error = timeout(Duration::from_secs(3), run)
            .await
            .expect("session ended promptly")
            .unwrap()
            .output
            .unwrap_err();
        assert!(error.starts_with("terminated"), "{error}");
        let grandchild: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!is_alive(grandchild), "background sleep was left running");
    }

    #[tokio::test]
    async fn grandchild_holding_stdout_does_not_pin_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        // The backgrounded sleep inherits stdout and would keep the pipe open for 30s.
        let body = format!(
            "sleep 30 &\necho $! > {}\ncat <<'EOF'\n{}\nEOF",
            pid_file.display(),
            result(json!({}))
        );
        let cli = fake_cli(dir.path(), &body);
        let (_tx, rx) = watch::channel(Shutdown::Run);
        let outcome = timeout(Duration::from_secs(5), cli.run(in_dir(dir.path()), rx))
            .await
            .expect("pipe reads are bounded");
        assert_eq!(outcome.output.unwrap()["summary"], "ok");
        let grandchild: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!is_alive(grandchild), "stray grandchild was not killed");
    }

    #[tokio::test]
    async fn fake_cli_output_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("cat <<'EOF'\n{}\nEOF", result(json!({})));
        let cli = fake_cli(dir.path(), &body);
        let (_tx, rx) = watch::channel(Shutdown::Run);
        let report = cli.run(in_dir(dir.path()), rx).await;
        assert_eq!(report.output.unwrap()["summary"], "ok");
        assert_eq!(report.session_id.as_deref(), Some("session-example"));
    }
}
