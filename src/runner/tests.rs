use std::time::Duration;

use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::intake::secrets::tests::write_secrets;
use crate::results::{CardOutcome, DiscussionToRun};
use crate::testing::*;

const MINUTE: Duration = Duration::from_secs(60);

fn lines_of(lines: &Lines) -> Vec<String> {
    lines.lock().unwrap().clone()
}

fn discussion(thread: &str) -> DiscussionToRun {
    DiscussionToRun {
        discussion_ref: "EX-9".into(),
        thread: thread.into(),
        question: "Why does the example export fail?".into(),
    }
}

#[tokio::test]
async fn card_held_by_another_runner_is_skipped() {
    let env = test_env();
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![]));
    env.store()
        .claim("card:EX-1", "other-app", now(), CLAIM_LEASE)
        .unwrap();
    assert!(runner.run_card("EX-1", None).await.unwrap().is_none());
    assert!(runner.session().calls().is_empty());
    assert!(lines_of(&lines)[0].contains("card skipped EX-1 — held by other-app"));
}

#[tokio::test(start_paused = true)]
async fn two_runners_contending_for_one_card() {
    let env = test_env();
    let slow = FakeSession::sequence(vec![ok(card_output("EX-1", "done")).after(25 * MINUTE)]);
    let (alpha, _) = make_runner(&env, slow);
    let (beta, beta_lines) = make_named_runner(
        &env,
        "beta-app",
        FakeSession::sequence(vec![ok(card_output("EX-1", "done"))]),
    );
    let running = tokio::spawn(async move { alpha.run_card("EX-1", None).await });
    // Past the 10 minute lease: alpha renews every minute while its session runs.
    tokio::time::sleep(15 * MINUTE).await;
    assert!(beta.run_card("EX-1", None).await.unwrap().is_none());
    assert!(beta.session().calls().is_empty());
    assert!(lines_of(&beta_lines)[0].contains("card skipped EX-1 — held by example-app"));

    let finished = running.await.unwrap().unwrap().unwrap();
    assert_eq!(finished.status, CardOutcome::Done);
    // Released on finish: beta may take it now.
    assert!(beta.run_card("EX-1", None).await.unwrap().is_some());
    assert_eq!(beta.session().calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_crashed_runners_lease_expires() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(card_output("EX-1", "done"))]);
    let (runner, lines) = make_runner(&env, session);
    // A runner that crashed at `now()` never renews or releases.
    env.store()
        .claim("card:EX-1", "crashed-app", now(), CLAIM_LEASE)
        .unwrap();
    assert!(runner.run_card("EX-1", None).await.unwrap().is_none());
    tokio::time::sleep(CLAIM_LEASE).await;
    assert!(runner.run_card("EX-1", None).await.unwrap().is_some());
    let lines = lines_of(&lines);
    assert!(lines[0].contains("held by crashed-app"), "{lines:?}");
    assert!(lines[1].contains("card done EX-1"), "{lines:?}");
}

#[tokio::test(start_paused = true)]
async fn recover_resets_what_a_crash_left_behind() {
    let env = test_env();
    let (runner, _) = make_runner(&env, FakeSession::sequence(vec![]));
    let store = env.store();
    store.start_card("example-app", "EX-1", &[], now()).unwrap();
    store
        .claim("card:EX-1", "example-app", now(), CLAIM_LEASE)
        .unwrap();
    store
        .claim("card:EX-2", "other-app", now(), CLAIM_LEASE)
        .unwrap();
    let event = crate::intake::IncomingEvent {
        source: "manual".into(),
        external_id: "1".into(),
        kind: crate::intake::EventKind::Message,
        mentions_me: false,
        sender: None,
        occurred_at: now(),
        payload: json!({"body": "b"}),
    };
    store.enqueue("example-app", &event, now()).unwrap();
    store.take_batch("example-app").unwrap();

    runner.recover().await;

    let state = store.load_state("example-app").unwrap();
    let card = &state.cards["EX-1"];
    assert_eq!(card.status, CardStatus::Failed);
    assert_eq!(card.attempts, 1, "a crash counts");
    assert_eq!(
        card.retry_at,
        Some(now() + chrono::TimeDelta::minutes(1)),
        "retried after the first backoff"
    );
    assert_eq!(store.holder("card:EX-1", now()).unwrap(), None);
    assert_eq!(
        store.holder("card:EX-2", now()).unwrap().as_deref(),
        Some("other-app"),
        "other runners' claims are untouched"
    );
    assert_eq!(store.new_event_count("example-app").unwrap(), 1);
}

async fn linear_answering(is_me: Option<bool>) -> MockServer {
    let server = MockServer::start().await;
    let issue = match is_me {
        Some(is_me) => json!({"identifier": "EX-1", "assignee": {"isMe": is_me}}),
        None => json!(null),
    };
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"issue": issue}})))
        .mount(&server)
        .await;
    server
}

fn with_linear(env: &mut TestEnv, url: &str) {
    env.config.intake.linear.enabled = true;
    env.config.intake.linear.api_url = url.to_string();
    write_secrets(
        &env.paths.secrets,
        "LINEAR_API_KEY=lin_api_example\n",
        0o600,
    );
}

#[tokio::test]
async fn card_not_assigned_to_me_is_refused() {
    let server = linear_answering(Some(false)).await;
    let mut env = test_env();
    with_linear(&mut env, &server.uri());
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![]));
    assert!(runner.run_card("EX-1", None).await.unwrap().is_none());
    assert!(runner.session().calls().is_empty());
    assert!(lines_of(&lines)[0].contains("card refused EX-1 — not assigned to me in linear"));
    assert_eq!(
        env.store().holder("card:EX-1", now()).unwrap(),
        None,
        "never claimed"
    );
}

#[tokio::test]
async fn card_assigned_to_me_runs() {
    let server = linear_answering(Some(true)).await;
    let mut env = test_env();
    with_linear(&mut env, &server.uri());
    let session = FakeSession::sequence(vec![ok(card_output("EX-1", "done"))]);
    let (runner, _) = make_runner(&env, session);
    assert!(runner.run_card("EX-1", None).await.unwrap().is_some());
}

#[tokio::test]
async fn unknown_ref_or_missing_key_is_refused() {
    let server = linear_answering(None).await;
    let mut env = test_env();
    with_linear(&mut env, &server.uri());
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![]));
    assert!(runner.run_card("ZZ-1", None).await.unwrap().is_none());
    assert!(lines_of(&lines)[0].contains("card refused ZZ-1 — not found in linear"));

    std::fs::remove_file(&env.paths.secrets).unwrap();
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![]));
    assert!(runner.run_card("EX-1", None).await.unwrap().is_none());
    assert!(lines_of(&lines)[0].contains("card refused EX-1 — linear: LINEAR_API_KEY missing"));
    assert!(runner.session().calls().is_empty());
}

#[tokio::test]
async fn discussion_runs_without_an_assignee_check_or_a_workspace() {
    let mut env = test_env();
    // A tracker whose key is missing would refuse any card; discussions are never gated.
    env.config.intake.linear.enabled = true;
    env.config.intake.linear.api_url = "http://127.0.0.1:9".into();
    env.config.workspaces.clear();
    let session = FakeSession::sequence(vec![ok(discussion_output("EX-9", "replied"))]);
    let (runner, lines) = make_runner(&env, session);
    let thread = "https://tracker.example.com/EX-9/c1";
    let result = runner.run_discussion(discussion(thread)).await.unwrap();
    assert_eq!(result.status, crate::results::DiscussionOutcome::Replied);

    let call = &runner.session().calls()[0];
    assert_eq!(call.label(), "discussion EX-9");
    assert_eq!(call.request.mode, crate::session::Mode::Discussion);
    assert_eq!(
        call.request.cwd, env.paths.state_dir,
        "no workspace: the state dir"
    );
    assert_eq!(call.request.model, env.config.card.model);
    let context = crate::prompts::context_of(&call.request.prompt);
    assert_eq!(context["thread"], thread);
    assert_eq!(context["runner"], "example-app");
    assert!(context["workspace"].is_null());
    assert!(
        lines_of(&lines)[0].contains("discussion replied EX-9 cost=$0.05 — answered in thread")
    );
    let key = format!("discussion:{thread}");
    assert_eq!(env.store().holder(&key, now()).unwrap(), None, "released");
}

#[tokio::test]
async fn discussion_uses_the_matching_workspace_and_its_claim() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(discussion_output("EX-9", "drafted"))]);
    let (runner, lines) = make_runner(&env, session);
    env.store()
        .claim("discussion:EX-9", "other-app", now(), CLAIM_LEASE)
        .unwrap();
    assert!(runner.run_discussion(discussion("")).await.is_none());
    assert!(lines_of(&lines)[0].contains("discussion skipped EX-9 — held by other-app"));

    let thread = "https://tracker.example.com/EX-9/c2";
    runner.run_discussion(discussion(thread)).await.unwrap();
    let call = &runner.session().calls()[0];
    assert_eq!(
        call.request.cwd, env.config.workspaces[0].path,
        "matched by ref; not a repository, so used as is"
    );
}

#[tokio::test]
async fn invalid_discussion_result_is_a_failure() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(json!({"ref": "EX-9", "status": "done"}))]);
    let (runner, lines) = make_runner(&env, session);
    assert!(runner.run_discussion(discussion("t")).await.is_none());
    assert!(
        lines_of(&lines)[0]
            .contains("discussion invalid_output EX-9 cost=$0.05 — invalid discussion result")
    );
}

#[tokio::test]
async fn every_session_is_recorded_as_an_attempt() {
    let env = test_env();
    let session = FakeSession::sequence(vec![
        ok(triage_output(&[])),
        ok(card_output("EX-1", "blocked")),
        ok(discussion_output("EX-9", "replied")),
        fail("boom"),
    ]);
    let (runner, _) = make_runner(&env, session);
    runner.triage(vec![]).await.unwrap();
    runner.run_card("EX-1", None).await.unwrap().unwrap();
    runner.run_discussion(discussion("t1")).await.unwrap();
    assert!(runner.run_card("EX-1", None).await.unwrap().is_none());

    let store = env.store();
    let mut attempts = store.attempts("example-app", None, -1).unwrap();
    attempts.reverse();
    let recorded: Vec<String> = attempts
        .iter()
        .map(|a| {
            format!(
                "{} {} #{} {} {}",
                a.mode,
                a.reference,
                a.attempt,
                a.outcome.as_deref().unwrap_or("-"),
                a.session_id.as_deref().unwrap_or("-")
            )
        })
        .collect();
    assert_eq!(
        recorded,
        [
            "triage  #1 ok session-1",
            "card EX-1 #1 blocked session-2",
            "discussion EX-9 #1 replied session-3",
            "card EX-1 #2 api_error session-4",
        ]
    );
    assert_eq!(attempts[1].cost_usd, Some(0.05));
    assert_eq!(
        attempts[1].cwd,
        env.config.workspaces[0].path.display().to_string()
    );
    assert_eq!(attempts[3].summary.as_deref(), Some("boom"));
    assert!(attempts.iter().all(|a| a.ended_at.is_some()));
}

#[tokio::test]
async fn each_mode_gets_its_timeout_and_the_shared_idle_limit() {
    let mut env = test_env();
    env.config.triage.timeout = MINUTE * 20;
    env.config.card.timeout = MINUTE * 180;
    env.config.discussion.timeout = MINUTE * 60;
    env.config.sessions.idle_timeout = MINUTE * 15;
    let session = FakeSession::sequence(vec![
        ok(triage_output(&[])),
        ok(card_output("EX-1", "done")),
        ok(discussion_output("EX-9", "replied")),
    ]);
    let (runner, _) = make_runner(&env, session);
    runner.triage(vec![]).await.unwrap();
    runner.run_card("EX-1", None).await.unwrap().unwrap();
    runner.run_discussion(discussion("t1")).await.unwrap();
    let timeouts: Vec<(Duration, Duration)> = runner
        .session()
        .calls()
        .iter()
        .map(|c| (c.request.limits.timeout, c.request.limits.idle_timeout))
        .collect();
    assert_eq!(
        timeouts,
        [
            (MINUTE * 20, MINUTE * 15),
            (MINUTE * 180, MINUTE * 15),
            (MINUTE * 60, MINUTE * 15)
        ]
    );
}

#[tokio::test]
async fn recover_closes_attempts_a_stopped_runner_left_open() {
    let env = test_env();
    let (runner, _) = make_runner(&env, FakeSession::sequence(vec![]));
    let store = env.store();
    let cwd = std::path::Path::new("/tmp/example");
    store
        .begin_attempt("example-app", "card", "EX-1", cwd, now())
        .unwrap();
    runner.recover().await;
    assert!(store.open_attempts("example-app").unwrap().is_empty());
    let closed = &store.attempts("example-app", Some("EX-1"), -1).unwrap()[0];
    assert_eq!(closed.outcome.as_deref(), Some("crash"));
}
