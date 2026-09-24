//! Test fixtures: a scripted fake session and a temp-dir config. No model calls in tests.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::config::{Config, tests::config_toml};
use crate::paths::Paths;
use crate::runner::Runner;
use crate::session::{Session, SessionError, SessionOutcome, SessionRequest, Shutdown};

pub fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 15, 9, 30, 0).unwrap()
}

pub struct TestEnv {
    pub dir: TempDir,
    pub config: Config,
    pub paths: Paths,
}

pub fn test_env() -> TestEnv {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_toml(dir.path())).unwrap();
    let config = crate::config::load_config(&config_path).unwrap();
    let paths = Paths::resolve(&config_path, &config);
    TestEnv { dir, config, paths }
}

pub fn write_plugin(root: &Path) {
    let manifest = root.join("plugin/.claude-plugin/plugin.json");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(manifest, r#"{"name": "claude-harness"}"#).unwrap();
}

pub struct Step {
    pub delay: Duration,
    pub output: Result<Value, String>,
}

pub fn ok(output: Value) -> Step {
    Step {
        delay: Duration::ZERO,
        output: Ok(output),
    }
}

pub fn fail(message: &str) -> Step {
    Step {
        delay: Duration::ZERO,
        output: Err(message.to_string()),
    }
}

impl Step {
    pub fn after(mut self, delay: Duration) -> Step {
        self.delay = delay;
        self
    }
}

#[derive(Clone)]
pub struct Call {
    pub request: SessionRequest,
    pub started: Instant,
}

impl Call {
    pub fn first_line(&self) -> &str {
        self.request.prompt.lines().next().unwrap_or("")
    }
}

type Script = Box<dyn Fn(&SessionRequest) -> Step + Send + Sync>;

/// Scripted session. A delayed step ends early with "terminated" when shutdown is raised,
/// like a real child process receiving SIGTERM.
pub struct FakeSession {
    script: Script,
    pub calls: Mutex<Vec<Call>>,
    active: AtomicUsize,
    pub max_active: AtomicUsize,
}

impl FakeSession {
    pub fn new(script: impl Fn(&SessionRequest) -> Step + Send + Sync + 'static) -> Self {
        FakeSession {
            script: Box::new(script),
            calls: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        }
    }

    /// Steps in call order.
    pub fn sequence(steps: Vec<Step>) -> Self {
        let steps = Mutex::new(VecDeque::from(steps));
        FakeSession::new(move |_| {
            steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| fail("no step"))
        })
    }

    /// Heartbeat steps in order (then quiet ticks); card steps by ref.
    pub fn routed(
        heartbeats: Vec<Step>,
        card: impl Fn(&str) -> Step + Send + Sync + 'static,
    ) -> Self {
        let heartbeats = Mutex::new(VecDeque::from(heartbeats));
        FakeSession::new(move |request| {
            let first = request.prompt.lines().next().unwrap_or("");
            match first.strip_prefix("/claude-harness:workflow card ") {
                Some(card_ref) => card(card_ref),
                None => heartbeats
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| ok(heartbeat_output(&[]))),
            }
        })
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn first_lines(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|c| c.first_line().to_string())
            .collect()
    }

    pub fn card_call(&self, card_ref: &str) -> Option<Call> {
        let line = format!("/claude-harness:workflow card {card_ref}");
        self.calls().into_iter().find(|c| c.first_line() == line)
    }
}

impl Session for FakeSession {
    async fn run(
        &self,
        request: SessionRequest,
        mut shutdown: watch::Receiver<Shutdown>,
    ) -> Result<SessionOutcome, SessionError> {
        let step = (self.script)(&request);
        self.calls.lock().unwrap().push(Call {
            request,
            started: Instant::now(),
        });
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let terminated = if step.delay.is_zero() {
            false
        } else {
            tokio::select! {
                () = tokio::time::sleep(step.delay) => false,
                _ = shutdown.wait_for(|level| *level != Shutdown::Run) => true,
            }
        };
        self.active.fetch_sub(1, Ordering::SeqCst);
        if terminated {
            return Err(SessionError("terminated: harness is stopping".into()));
        }
        step.output
            .map(|output| SessionOutcome {
                output,
                cost_usd: Some(0.05),
            })
            .map_err(SessionError)
    }
}

pub fn heartbeat_output(cards: &[(&str, &[&str])]) -> Value {
    let cards: Vec<Value> = cards
        .iter()
        .map(|(card_ref, blocked_by)| json!({"ref": card_ref, "blocked_by": blocked_by}))
        .collect();
    json!({
        "cursors": {},
        "handled": [{"source": "slack:C0000000001", "item": "m1", "action": "drafted"}],
        "cards_to_work": cards,
        "summary": "one quick reply drafted"
    })
}

pub fn card_output(card_ref: &str, status: &str) -> Value {
    let pr_url = (status == "done").then_some("https://example.com/pr/7");
    json!({"ref": card_ref, "status": status, "pr_url": pr_url, "blocked_on": null, "summary": "implemented"})
}

pub type Lines = Arc<Mutex<Vec<String>>>;

pub fn make_runner(env: &TestEnv, session: FakeSession) -> (Runner<FakeSession>, Lines) {
    let lines: Lines = Arc::default();
    let sink = lines.clone();
    let runner = Runner::new(env.config.clone(), env.paths.clone(), session)
        .with_clock(Arc::new(now))
        .with_output(Arc::new(move |line| sink.lock().unwrap().push(line)));
    (runner, lines)
}
