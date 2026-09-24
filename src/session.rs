//! The only module that starts Claude Code: one `claude -p` process per session.

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;

use crate::config::Effort;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRequest {
    pub prompt: String,
    pub model: String,
    pub effort: Effort,
    pub max_budget_usd: f64,
    pub cwd: PathBuf,
    pub plugin_dir: PathBuf,
    pub output_schema: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionOutcome {
    pub output: Value,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct SessionError(pub String);

/// Runner-wide shutdown level, broadcast to every running session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
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
    ) -> impl Future<Output = Result<SessionOutcome, SessionError>> + Send;
}

/// `claude -p` in print mode with JSON structured output, auto permission mode and the
/// harness plugin; user/project settings and MCP servers load as in any Claude Code run.
#[derive(Debug, Clone)]
pub struct ClaudeCli {
    pub program: PathBuf,
}

impl Default for ClaudeCli {
    fn default() -> Self {
        ClaudeCli {
            program: PathBuf::from("claude"),
        }
    }
}

pub fn build_args(request: &SessionRequest) -> Vec<String> {
    vec![
        "-p".into(),
        request.prompt.clone(),
        "--output-format".into(),
        "json".into(),
        "--json-schema".into(),
        request.output_schema.to_string(),
        "--permission-mode".into(),
        "auto".into(),
        "--model".into(),
        request.model.clone(),
        "--effort".into(),
        request.effort.as_str().into(),
        "--max-budget-usd".into(),
        request.max_budget_usd.to_string(),
        "--plugin-dir".into(),
        request.plugin_dir.display().to_string(),
    ]
}

/// Parse the final `{"type": "result", ...}` object printed by `--output-format json`.
pub fn outcome_from_stdout(stdout: &str) -> Result<SessionOutcome, SessionError> {
    let parsed = serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .or_else(|| {
            stdout
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        });
    let Some(result) = parsed.filter(|v| v["type"] == "result") else {
        return Err(SessionError("session ended without a result".into()));
    };
    let structured = &result["structured_output"];
    if result["is_error"] == Value::Bool(true) || !structured.is_object() {
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
        return Err(SessionError(format!("session failed: {detail}")));
    }
    Ok(SessionOutcome {
        output: structured.clone(),
        cost_usd: result["total_cost_usd"].as_f64(),
    })
}

fn signal_child(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid.and_then(|p| libc::pid_t::try_from(p).ok()) {
        // SAFETY: plain kill(2) on our own child's pid; no memory is shared.
        unsafe {
            libc::kill(pid, signal);
        }
    }
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
    ) -> Result<SessionOutcome, SessionError> {
        if *shutdown.borrow_and_update() != Shutdown::Run {
            return Err(SessionError("not started: harness is stopping".into()));
        }
        let mut child = Command::new(&self.program)
            .args(build_args(&request))
            .current_dir(&request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| SessionError(format!("cannot start {}: {e}", self.program.display())))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let read_out = tokio::spawn(async move {
            let mut text = String::new();
            stdout.read_to_string(&mut text).await.map(|_| text)
        });
        let read_err = tokio::spawn(async move {
            let mut text = String::new();
            stderr.read_to_string(&mut text).await.map(|_| text)
        });

        let mut terminated = false;
        let status = loop {
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
                            signal_child(child.id(), libc::SIGTERM);
                        }
                        Shutdown::Force => {
                            terminated = true;
                            let _ = child.start_kill();
                        }
                    }
                }
            }
        }
        .map_err(|e| SessionError(format!("waiting for claude: {e}")))?;

        let stdout = read_out.await.ok().and_then(Result::ok).unwrap_or_default();
        let stderr = read_err.await.ok().and_then(Result::ok).unwrap_or_default();
        if terminated {
            return Err(SessionError("terminated: harness is stopping".into()));
        }
        outcome_from_stdout(&stdout).map_err(|error| {
            if stdout.trim().is_empty() {
                SessionError(format!(
                    "claude exited with {status}: {}",
                    last_line(&stderr)
                ))
            } else {
                error
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> SessionRequest {
        SessionRequest {
            prompt: "/claude-harness:workflow heartbeat".into(),
            model: "sonnet".into(),
            effort: Effort::Medium,
            max_budget_usd: 1.5,
            cwd: PathBuf::from("/tmp/example/state"),
            plugin_dir: PathBuf::from("/tmp/example/plugin"),
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
                "/claude-harness:workflow heartbeat",
                "--output-format",
                "json",
                "--json-schema",
                r#"{"type":"object"}"#,
                "--permission-mode",
                "auto",
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
    }

    fn result(overrides: Value) -> String {
        let mut base = json!({
            "type": "result", "subtype": "success", "is_error": false,
            "total_cost_usd": 0.42, "structured_output": {"summary": "ok"},
            "result": "{\"summary\":\"ok\"}"
        });
        for (key, value) in overrides.as_object().unwrap() {
            base[key] = value.clone();
        }
        base.to_string()
    }

    #[test]
    fn outcome_from_success() {
        let outcome = outcome_from_stdout(&result(json!({}))).unwrap();
        assert_eq!(outcome.output, json!({"summary": "ok"}));
        assert_eq!(outcome.cost_usd, Some(0.42));
        let with_noise = format!("warning: something\n{}\n", result(json!({})));
        assert!(outcome_from_stdout(&with_noise).is_ok());
    }

    #[test]
    fn outcome_from_failures() {
        let budget = result(json!({
            "is_error": true, "subtype": "error_max_budget_usd",
            "errors": ["Reached maximum budget ($0.05)"], "structured_output": null
        }));
        let error = outcome_from_stdout(&budget).unwrap_err();
        assert_eq!(error.0, "session failed: Reached maximum budget ($0.05)");
        let subtype_only = result(json!({"is_error": true, "subtype": "error_during_execution"}));
        assert!(
            outcome_from_stdout(&subtype_only)
                .unwrap_err()
                .0
                .contains("error_during_execution")
        );
        for bad in [
            String::new(),
            "not json".to_string(),
            json!({"type": "assistant"}).to_string(),
            result(json!({"structured_output": null})),
            result(json!({"structured_output": "plain text"})),
        ] {
            assert!(outcome_from_stdout(&bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn missing_program_is_a_session_error() {
        let cli = ClaudeCli {
            program: PathBuf::from("/nonexistent/claude-example"),
        };
        let (_tx, rx) = watch::channel(Shutdown::Run);
        let request = SessionRequest {
            cwd: std::env::temp_dir(),
            ..request()
        };
        let error = cli.run(request, rx).await.unwrap_err();
        assert!(error.0.starts_with("cannot start"), "{}", error.0);
    }

    #[tokio::test]
    async fn graceful_shutdown_terminates_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let cli = ClaudeCli { program: script };
        let (tx, rx) = watch::channel(Shutdown::Run);
        let request = SessionRequest {
            cwd: dir.path().to_path_buf(),
            ..request()
        };
        let run = tokio::spawn(async move { cli.run(request, rx).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        tx.send_replace(Shutdown::Graceful);
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("child ended")
            .unwrap()
            .unwrap_err();
        assert!(error.0.starts_with("terminated"), "{}", error.0);
    }

    #[tokio::test]
    async fn fake_cli_output_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude");
        let body = format!("#!/bin/sh\ncat <<'EOF'\n{}\nEOF\n", result(json!({})));
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let cli = ClaudeCli { program: script };
        let (_tx, rx) = watch::channel(Shutdown::Run);
        let request = SessionRequest {
            cwd: dir.path().to_path_buf(),
            ..request()
        };
        let outcome = cli.run(request, rx).await.unwrap();
        assert_eq!(outcome.output["summary"], "ok");
    }
}
