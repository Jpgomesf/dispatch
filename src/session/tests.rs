//! `ClaudeCli` against fake `claude` shell scripts that print scripted stream-json lines. No
//! model calls.

use std::path::Path;

use serde_json::json;

use super::*;

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
        limits: Limits {
            timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(10),
        },
    }
}

fn limited(dir: &Path, timeout: u64, idle_timeout: u64) -> SessionRequest {
    SessionRequest {
        limits: Limits {
            timeout: Duration::from_millis(timeout),
            idle_timeout: Duration::from_millis(idle_timeout),
        },
        ..in_dir(dir)
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
            "stream-json",
            "--verbose",
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

fn init_line() -> String {
    json!({"type": "system", "subtype": "init", "session_id": "session-example",
           "mcp_servers": [], "plugins": []})
    .to_string()
}

fn result_line(overrides: Value) -> String {
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

/// Shell lines printing `lines` verbatim.
fn print(lines: &[String]) -> String {
    format!("cat <<'EOF'\n{}\nEOF", lines.join("\n"))
}

/// A fake `claude`: a shell script with the given body.
fn fake_cli(dir: &Path, body: &str) -> ClaudeCli {
    let script = dir.join("fake-claude");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    ClaudeCli {
        program: script,
        pipe_grace: Duration::from_millis(300),
    }
}

fn in_dir(dir: &Path) -> SessionRequest {
    SessionRequest {
        cwd: dir.to_path_buf(),
        ..request()
    }
}

/// A control with a shutdown sender and a notice receiver for the test to hold.
fn new_control() -> (
    Control,
    watch::Sender<Shutdown>,
    mpsc::UnboundedReceiver<Notice>,
) {
    let (shutdown_tx, shutdown) = watch::channel(Shutdown::Run);
    let (notices, received) = mpsc::unbounded_channel();
    (Control { shutdown, notices }, shutdown_tx, received)
}

fn is_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn read_pid(file: &Path) -> libc::pid_t {
    std::fs::read_to_string(file)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

async fn wait_for_line(file: &Path) {
    for _ in 0..100 {
        if std::fs::read_to_string(file).is_ok_and(|t| t.ends_with('\n')) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} never written", file.display());
}

#[tokio::test]
async fn missing_program_is_a_crash() {
    let cli = ClaudeCli {
        program: PathBuf::from("/nonexistent/claude-example"),
        ..ClaudeCli::default()
    };
    let request = SessionRequest {
        cwd: std::env::temp_dir(),
        ..request()
    };
    let (control, _shutdown, _notices) = new_control();
    let report = cli.run(request, control).await;
    assert!(
        matches!(&report.ended, Ended::Crash(detail) if detail.starts_with("cannot start")),
        "{report:?}"
    );
}

#[tokio::test]
async fn the_stream_names_the_session_and_ends_with_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let body = print(&[
        init_line(),
        "not json: ignored".into(),
        result_line(json!({})),
    ]);
    let cli = fake_cli(dir.path(), &body);
    let (control, _shutdown, mut notices) = new_control();
    let report = cli.run(in_dir(dir.path()), control).await;
    assert_eq!(report.ended, Ended::Output(json!({"summary": "ok"})));
    assert_eq!(report.session_id.as_deref(), Some("session-example"));
    assert_eq!(report.cost_usd, Some(0.42));
    assert_eq!(
        notices.try_recv().unwrap(),
        Notice::Started {
            session_id: "session-example".into()
        }
    );
}

#[tokio::test]
async fn an_error_result_and_a_missing_result_are_told_apart() {
    let dir = tempfile::tempdir().unwrap();
    let error = result_line(json!({
        "is_error": true, "subtype": "error_max_turns",
        "errors": ["Reached maximum turns"], "structured_output": null
    }));
    let cli = fake_cli(dir.path(), &print(&[init_line(), error]));
    let (control, _shutdown, _notices) = new_control();
    let report = cli.run(in_dir(dir.path()), control).await;
    assert_eq!(
        report.ended,
        Ended::ApiError("Reached maximum turns".into())
    );

    let body = format!("{}\necho 'Error: boom' >&2\nexit 3", print(&[init_line()]));
    let cli = fake_cli(dir.path(), &body);
    let (control, _shutdown, _notices) = new_control();
    let report = cli.run(in_dir(dir.path()), control).await;
    let Ended::Crash(detail) = &report.ended else {
        panic!("{report:?}");
    };
    assert!(
        detail.contains("exit status: 3") && detail.ends_with("Error: boom"),
        "{detail}"
    );
    assert_eq!(report.session_id.as_deref(), Some("session-example"));
}

#[tokio::test]
async fn graceful_shutdown_terminates_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    let body = format!("sleep 30 &\necho $! > {}\nsleep 30", pid_file.display());
    let cli = fake_cli(dir.path(), &body);
    let (control, shutdown, _notices) = new_control();
    let request = in_dir(dir.path());
    let run = tokio::spawn(async move { cli.run(request, control).await });
    wait_for_line(&pid_file).await;
    shutdown.send_replace(Shutdown::Graceful);
    let report = timeout(Duration::from_secs(3), run)
        .await
        .expect("session ended promptly")
        .unwrap();
    assert_eq!(report.ended, Ended::Interrupted);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !is_alive(read_pid(&pid_file)),
        "background sleep was left running"
    );
}

#[tokio::test]
async fn the_wall_clock_timeout_ends_a_session() {
    let dir = tempfile::tempdir().unwrap();
    // Busy with events all along: only the wall clock can end it.
    let body = format!(
        "{}\nwhile true; do echo '{{\"type\":\"system\"}}'; sleep 0.05; done",
        print(&[init_line()])
    );
    let cli = fake_cli(dir.path(), &body);
    let (control, _shutdown, _notices) = new_control();
    let report = timeout(
        Duration::from_secs(5),
        cli.run(limited(dir.path(), 1_500, 10_000), control),
    )
    .await
    .expect("ended by the timeout");
    assert!(matches!(report.ended, Ended::Timeout(_)), "{report:?}");
    assert_eq!(report.session_id.as_deref(), Some("session-example"));
}

#[tokio::test]
async fn the_watchdog_ends_a_silent_session_and_events_keep_one_alive() {
    let dir = tempfile::tempdir().unwrap();
    let silent = format!("{}\nsleep 30", print(&[init_line()]));
    let cli = fake_cli(dir.path(), &silent);
    let (control, _shutdown, _notices) = new_control();
    let report = timeout(
        Duration::from_secs(5),
        cli.run(limited(dir.path(), 10_000, 300), control),
    )
    .await
    .expect("ended by the watchdog");
    assert!(matches!(report.ended, Ended::Stuck(_)), "{report:?}");

    // Six events 500ms apart outlast a 2s idle limit only because each resets it.
    let chatty = format!(
        "for i in 1 2 3 4 5 6; do echo '{{\"type\":\"system\"}}'; sleep 0.5; done\n{}",
        print(&[result_line(json!({}))])
    );
    let cli = fake_cli(dir.path(), &chatty);
    let (control, _shutdown, _notices) = new_control();
    let report = cli.run(limited(dir.path(), 10_000, 2_000), control).await;
    assert_eq!(report.ended, Ended::Output(json!({"summary": "ok"})));
}

#[tokio::test]
async fn grandchild_holding_stdout_does_not_pin_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    // The backgrounded sleep inherits stdout and would keep the pipe open for 30s.
    let body = format!(
        "sleep 30 &\necho $! > {}\n{}",
        pid_file.display(),
        print(&[result_line(json!({}))])
    );
    let cli = fake_cli(dir.path(), &body);
    let (control, _shutdown, _notices) = new_control();
    let report = timeout(Duration::from_secs(5), cli.run(in_dir(dir.path()), control))
        .await
        .expect("pipe reads are bounded");
    assert_eq!(report.ended, Ended::Output(json!({"summary": "ok"})));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !is_alive(read_pid(&pid_file)),
        "stray grandchild was not killed"
    );
}
