use super::*;
use crate::testing::*;

fn argv(env: &TestEnv, args: &[&str]) -> Vec<OsString> {
    let mut all: Vec<OsString> = vec!["dispatch".into(), "--config".into()];
    all.push(env.paths.config.clone().into());
    all.extend(args.iter().map(OsString::from));
    all
}

async fn run_with(env: &TestEnv, args: &[&str], session: FakeSession) -> (i32, Arc<FakeSession>) {
    let shared = Arc::new(session);
    let handle = shared.clone();
    let code = run(argv(env, args), env.env_paths(), move || {
        SharedSession(handle)
    })
    .await;
    (code, shared)
}

/// Lets a test keep a handle on the fake after the runner takes ownership.
struct SharedSession(Arc<FakeSession>);

impl Session for SharedSession {
    async fn run(
        &self,
        request: crate::session::SessionRequest,
        control: crate::session::Control,
    ) -> crate::session::SessionReport {
        self.0.run(request, control).await
    }
}

fn no_session() -> FakeSession {
    FakeSession::sequence(vec![])
}

#[tokio::test]
async fn check_reports_paths() {
    let env = test_env();
    write_plugin(env.dir.path());
    assert_eq!(run_with(&env, &["check"], no_session()).await.0, EXIT_OK);
}

#[tokio::test]
async fn check_fails_when_the_configured_plugin_is_missing() {
    let env = test_env();
    assert_eq!(
        run_with(&env, &["check"], no_session()).await.0,
        EXIT_FAILED
    );
}

#[test]
fn plugin_is_optional() {
    let dir = tempfile::tempdir().unwrap();
    let (none, ok) = plugin_report(None);
    assert!(ok && none.starts_with("none"), "{none}");
    let (missing, ok) = plugin_report(Some(dir.path()));
    assert!(!ok && missing.contains("MISSING"), "{missing}");
    write_plugin(dir.path());
    let (present, ok) = plugin_report(Some(&dir.path().join("plugin")));
    assert!(ok && present.ends_with("(ok)"), "{present}");
}

#[tokio::test]
async fn invalid_or_missing_config_exits_2() {
    let env = test_env();
    let invalid = "name = \"example-app\"\n[triage]\neffort = \"extreme\"\n";
    std::fs::write(&env.paths.config, invalid).unwrap();
    assert_eq!(
        run_with(&env, &["check"], no_session()).await.0,
        EXIT_BAD_CONFIG
    );
    std::fs::write(&env.paths.config, "[triage]\ninterval = \"10m\"\n").unwrap();
    assert_eq!(
        run_with(&env, &["check"], no_session()).await.0,
        EXIT_BAD_CONFIG,
        "name is required"
    );
    std::fs::remove_file(&env.paths.config).unwrap();
    assert_eq!(
        run_with(&env, &["check"], no_session()).await.0,
        EXIT_BAD_CONFIG
    );
}

#[tokio::test]
async fn stop_and_resume() {
    let env = test_env();
    let kill_switch = env.dir.path().join("state/STOP");
    assert_eq!(run_with(&env, &["stop"], no_session()).await.0, EXIT_OK);
    assert!(kill_switch.exists());
    assert_eq!(run_with(&env, &["resume"], no_session()).await.0, EXIT_OK);
    assert!(!kill_switch.exists());
    assert_eq!(run_with(&env, &["resume"], no_session()).await.0, EXIT_OK);
}

#[tokio::test]
async fn config_from_env() {
    let env = test_env();
    let args = ["dispatch", "stop"].map(OsString::from);
    let paths = EnvPaths {
        config: Some(env.paths.config.display().to_string()),
        ..env.env_paths()
    };
    assert_eq!(run(args, paths, no_session).await, EXIT_OK);
    assert!(env.dir.path().join("state/STOP").exists());
}

#[tokio::test]
async fn card_command() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(card_output("EX-1", "done"))]);
    let (code, session) = run_with(&env, &["card", "EX-1"], session).await;
    assert_eq!(code, EXIT_OK);
    assert_eq!(session.labels(), ["card EX-1"]);
}

#[tokio::test]
async fn card_command_failure_exit_code() {
    let env = test_env();
    let (code, _) = run_with(
        &env,
        &["card", "EX-1"],
        FakeSession::sequence(vec![fail("boom")]),
    )
    .await;
    assert_eq!(code, EXIT_FAILED);
    let failed = FakeSession::sequence(vec![ok(card_output("EX-1", "failed"))]);
    assert_eq!(
        run_with(&env, &["card", "EX-1"], failed).await.0,
        EXIT_FAILED
    );
}

#[tokio::test]
async fn card_unknown_workspace() {
    let env = test_env();
    let args = ["card", "EX-1", "--workspace", "missing"];
    assert_eq!(run_with(&env, &args, no_session()).await.0, EXIT_BAD_CONFIG);
}

#[tokio::test]
async fn card_refuses_when_stopped() {
    let env = test_env();
    run_with(&env, &["stop"], no_session()).await;
    let (code, session) = run_with(&env, &["card", "EX-1"], no_session()).await;
    assert_eq!(code, EXIT_FAILED);
    assert!(session.calls().is_empty());
}

#[tokio::test]
async fn second_instance_of_a_runner_refuses_to_run_sessions() {
    let env = test_env();
    assert!(env.paths.instance_lock_file().ends_with("example-app.lock"));
    let held = InstanceLock::try_acquire(&env.paths.instance_lock_file())
        .unwrap()
        .unwrap();
    let (code, session) = run_with(&env, &["card", "EX-1"], no_session()).await;
    assert_eq!(code, EXIT_FAILED);
    let args = ["heartbeat", "--once"];
    let (hb_code, hb_session) = run_with(&env, &args, no_session()).await;
    assert_eq!(hb_code, EXIT_FAILED);
    assert!(session.calls().is_empty() && hb_session.calls().is_empty());
    // Commands that run no sessions are not blocked.
    assert_eq!(run_with(&env, &["stop"], no_session()).await.0, EXIT_OK);
    drop(held);
    let done = FakeSession::sequence(vec![ok(card_output("EX-1", "done"))]);
    run_with(&env, &["resume"], no_session()).await;
    assert_eq!(run_with(&env, &["card", "EX-1"], done).await.0, EXIT_OK);
}

#[tokio::test]
async fn heartbeat_once() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(triage_output(&[]))]);
    let args = ["heartbeat", "--once", "--interval", "1m"];
    let (code, session) = run_with(&env, &args, session).await;
    assert_eq!(code, EXIT_OK);
    assert_eq!(session.labels(), ["triage"]);
}

#[tokio::test]
async fn enqueued_event_reaches_the_next_triage() {
    let env = test_env();
    let args = ["enqueue", "manual", "test event"];
    assert_eq!(run_with(&env, &args, no_session()).await.0, EXIT_OK);
    let session = FakeSession::sequence(vec![ok(triage_output(&[]))]);
    let (code, session) = run_with(&env, &["heartbeat", "--once"], session).await;
    assert_eq!(code, EXIT_OK);
    let context = crate::prompts::context_of(&session.calls()[0].request.prompt);
    assert_eq!(context["events"][0]["source"], "manual");
    assert_eq!(context["events"][0]["kind"], "message");
    assert_eq!(context["events"][0]["payload"]["body"], "test event");
    let store = Store::open(&env.paths.db).unwrap();
    assert_eq!(store.new_event_count("example-app").unwrap(), 0);
    let id: i64 = context["events"][0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(store.event_status(id).unwrap().as_deref(), Some("done"));
}

#[tokio::test]
async fn card_refuses_while_the_machine_is_paused() {
    let env = test_env();
    let later = chrono::Utc::now() + chrono::TimeDelta::minutes(15);
    Store::open(&env.paths.db)
        .unwrap()
        .set_pause(
            later,
            "usage or rate limit in card EX-9",
            "other-app",
            chrono::Utc::now(),
        )
        .unwrap();
    let (code, session) = run_with(&env, &["card", "EX-1"], no_session()).await;
    assert_eq!(code, EXIT_FAILED);
    assert!(session.calls().is_empty());
}

#[tokio::test]
async fn status_and_history_are_read_only() {
    let env = test_env();
    for args in [&["status"][..], &["history"], &["history", "EX-1"]] {
        assert_eq!(run_with(&env, args, no_session()).await.0, EXIT_OK);
    }
    assert!(!env.paths.db.exists(), "never created by a view");

    let done = FakeSession::sequence(vec![ok(card_output("EX-1", "done"))]);
    assert_eq!(run_with(&env, &["card", "EX-1"], done).await.0, EXIT_OK);
    let modified = || {
        std::fs::metadata(&env.paths.db)
            .unwrap()
            .modified()
            .unwrap()
    };
    let before = modified();
    for args in [&["status"][..], &["history"], &["history", "EX-1"]] {
        assert_eq!(run_with(&env, args, no_session()).await.0, EXIT_OK);
    }
    assert_eq!(modified(), before);
}

#[tokio::test]
async fn bad_interval_is_rejected() {
    let env = test_env();
    let code = run_with(&env, &["heartbeat", "--interval", "soon"], no_session())
        .await
        .0;
    assert_eq!(code, EXIT_BAD_CONFIG);
}
