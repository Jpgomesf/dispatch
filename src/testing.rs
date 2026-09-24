//! Test fixtures: a scripted fake session, a temp-dir config and store, and a clock that
//! follows tokio's (pausable) time. No model calls and no network in tests.

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
use crate::paths::{EnvPaths, Paths};
use crate::runner::{Clock, Runner};
use crate::session::{Session, SessionError, SessionOutcome, SessionRequest, Shutdown};
use crate::store::Store;

pub fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 15, 9, 30, 0).unwrap()
}

/// `now()` plus however much tokio time has passed since the clock was made, so leases and
/// timestamps move with `start_paused` tests.
pub fn tokio_clock() -> Clock {
    let base = Instant::now();
    Arc::new(move || now() + (Instant::now() - base))
}

pub struct TestEnv {
    pub dir: TempDir,
    pub config: Config,
    pub paths: Paths,
}

impl TestEnv {
    pub fn env_paths(&self) -> EnvPaths {
        EnvPaths {
            config: None,
            db: Some(self.dir.path().join("harness.db").display().to_string()),
            secrets: Some(self.dir.path().join("secrets.env").display().to_string()),
        }
    }

    pub fn store(&self) -> Store {
        Store::open(&self.paths.db).unwrap()
    }
}

pub fn test_env() -> TestEnv {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_toml(dir.path())).unwrap();
    let config = crate::config::load_config(&config_path).unwrap();
    let mut env = TestEnv {
        paths: Paths::resolve(&config_path, &config, &EnvPaths::default()),
        dir,
        config,
    };
    env.paths = Paths::resolve(&config_path, &env.config, &env.env_paths());
    env.paths.notifications_db = env.dir.path().join("notifications.db");
    env
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

    /// Triage steps in order (then quiet ticks); card steps by ref; discussions are drafted.
    pub fn routed(triages: Vec<Step>, card: impl Fn(&str) -> Step + Send + Sync + 'static) -> Self {
        let triages = Mutex::new(VecDeque::from(triages));
        FakeSession::new(move |request| {
            let first = request.prompt.lines().next().unwrap_or("");
            let command = first
                .strip_prefix("/claude-harness:workflow ")
                .unwrap_or("");
            if let Some(card_ref) = command.strip_prefix("card ") {
                card(card_ref)
            } else if let Some(discussion_ref) = command.strip_prefix("discussion ") {
                ok(discussion_output(discussion_ref, "drafted"))
            } else {
                triages
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| ok(triage_output(&[])))
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

pub fn triage_output(cards: &[(&str, &[&str])]) -> Value {
    let cards: Vec<Value> = cards
        .iter()
        .map(|(card_ref, blocked_by)| json!({"ref": card_ref, "blocked_by": blocked_by}))
        .collect();
    json!({
        "cursors": {},
        "handled": [{"source": "slack:C0000000001", "item": "m1", "action": "drafted"}],
        "cards_to_work": cards,
        "discussions_to_run": [],
        "summary": "one quick reply drafted"
    })
}

pub fn card_output(card_ref: &str, status: &str) -> Value {
    let pr_url = (status == "done").then_some("https://example.com/pr/7");
    json!({"ref": card_ref, "status": status, "pr_url": pr_url, "blocked_on": null, "summary": "implemented"})
}

pub fn discussion_output(discussion_ref: &str, status: &str) -> Value {
    json!({"ref": discussion_ref, "status": status, "summary": "answered in thread"})
}

pub type Lines = Arc<Mutex<Vec<String>>>;

pub fn make_runner(env: &TestEnv, session: FakeSession) -> (Runner<FakeSession>, Lines) {
    make_named_runner(env, &env.config.name, session)
}

/// Another runner on the same machine: same store, its own name.
pub fn make_named_runner(
    env: &TestEnv,
    name: &str,
    session: FakeSession,
) -> (Runner<FakeSession>, Lines) {
    let lines: Lines = Arc::default();
    let sink = lines.clone();
    let mut config = env.config.clone();
    config.name = name.to_string();
    let mut paths = env.paths.clone();
    paths.runner = name.to_string();
    let runner = Runner::new(config, paths, env.store(), session)
        .with_clock(tokio_clock())
        .with_env(Arc::new(|_| None))
        .with_output(Arc::new(move |line| sink.lock().unwrap().push(line)));
    (runner, lines)
}
