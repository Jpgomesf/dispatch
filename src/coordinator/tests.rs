use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::config::Effort;
use crate::results::CardOutcome;
use crate::session::Shutdown;
use crate::state::CardState;
use crate::testing::*;

const MINUTE: Duration = Duration::from_secs(60);
const TEN_MINUTES: Duration = Duration::from_secs(600);

fn lines_of(lines: &Lines) -> Vec<String> {
    lines.lock().unwrap().clone()
}

fn status_of(runner: &Runner<FakeSession>, card_ref: &str) -> Option<CardStatus> {
    runner
        .store()
        .load_state(runner.name())
        .unwrap()
        .status(card_ref)
}

fn seed_card(runner: &Runner<FakeSession>, card_ref: &str, status: CardStatus) {
    seed(runner, card_ref, &CardState::new(status, now()));
}

fn seed(runner: &Runner<FakeSession>, card_ref: &str, card: &CardState) {
    runner
        .store()
        .set_card(runner.name(), card_ref, card)
        .unwrap();
}

fn card_of(runner: &Runner<FakeSession>, card_ref: &str) -> CardState {
    runner
        .store()
        .card(runner.name(), card_ref)
        .unwrap()
        .unwrap()
}

fn state_with(cards: &[(&str, CardStatus)]) -> State {
    let mut state = State::default();
    for (card_ref, status) in cards {
        state
            .cards
            .insert(card_ref.to_string(), CardState::new(*status, now()));
    }
    state
}

#[tokio::test(start_paused = true)]
async fn unreadable_store_skips_scheduling_instead_of_assuming_empty() {
    let mut env = test_env();
    env.config.triage.max_cards_per_tick = 5;
    let db = env.paths.db.clone();
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &["EX-1"])]);
    // EX-1 corrupts its card rows while it runs; with an empty-state fallback EX-2's blocker
    // would look external and EX-2 would start.
    let session = FakeSession::routed(vec![ok(triage)], move |r| {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO cards (runner, ref, status, updated_at) VALUES ('example-app', 'EX-0', 'bogus', 'x')",
            [],
        )
        .unwrap();
        ok(card_output(r, "done"))
    });
    let (runner, lines) = make_runner(&env, session);
    runner.schedule(TEN_MINUTES, true).await;
    assert!(runner.session().card_call("EX-1").is_some());
    assert!(runner.session().card_call("EX-2").is_none());
    let lines = lines_of(&lines);
    let skipped = lines
        .iter()
        .filter(|l| l.contains("scheduling skipped"))
        .count();
    assert_eq!(skipped, 1, "one line per distinct error: {lines:?}");
}

#[test]
fn backoff_doubles_and_caps_at_interval() {
    assert_eq!(backoff_delay(0, TEN_MINUTES), TEN_MINUTES);
    assert_eq!(backoff_delay(1, TEN_MINUTES), BACKOFF_BASE);
    assert_eq!(backoff_delay(2, TEN_MINUTES), BACKOFF_BASE * 2);
    assert_eq!(backoff_delay(3, TEN_MINUTES), BACKOFF_BASE * 4);
    assert_eq!(backoff_delay(10, TEN_MINUTES), TEN_MINUTES);
    assert_eq!(backoff_delay(10_000, TEN_MINUTES), TEN_MINUTES);
}

#[test]
fn readiness_rules() {
    let state = state_with(&[("EX-1", CardStatus::Done), ("EX-2", CardStatus::Failed)]);
    let batch: HashSet<String> = ["EX-5".to_string()].into();
    let card = |blocked_by: &[&str]| CardToWork {
        card_ref: "EX-9".into(),
        blocked_by: blocked_by.iter().map(|s| s.to_string()).collect(),
    };
    assert!(is_ready(&card(&[]), &state, &batch));
    assert!(is_ready(&card(&["EX-1"]), &state, &batch), "done blocker");
    assert!(
        !is_ready(&card(&["EX-2"]), &state, &batch),
        "failed blocker"
    );
    assert!(
        !is_ready(&card(&["EX-5"]), &state, &batch),
        "blocker in batch"
    );
    assert!(
        is_ready(&card(&["EXT-7"]), &state, &batch),
        "external blocker"
    );
    assert!(!is_ready(&card(&["EX-1", "EX-5"]), &state, &batch));
}

#[tokio::test(start_paused = true)]
async fn tick_persists_cursors_and_runs_cards() {
    let env = test_env();
    let mut triage = triage_output(&[("EX-1", &[]), ("EX-1", &[]), ("OTHER-2", &[])]);
    triage["cursors"] = json!({"slack:C0000000001": "c9"});
    let session = FakeSession::routed(vec![ok(triage)], |card_ref| match card_ref {
        "EX-1" => ok(card_output("EX-1", "done")),
        _ => ok(card_output(card_ref, "blocked")),
    });
    let (runner, lines) = make_runner(&env, session);
    let old = std::collections::BTreeMap::from([("tracker:linear".to_string(), "old".to_string())]);
    runner.store().set_cursors(runner.name(), &old).unwrap();

    runner.heartbeat(TEN_MINUTES, true).await;

    let state = runner.store().load_state(runner.name()).unwrap();
    assert_eq!(state.cursors.len(), 2);
    assert_eq!(state.cursors["slack:C0000000001"], "c9");
    assert_eq!(state.cursors["tracker:linear"], "old");
    assert_eq!(state.status("EX-1"), Some(CardStatus::Done));
    assert_eq!(
        state.cards["EX-1"].pr_url.as_deref(),
        Some("https://example.com/pr/7")
    );
    assert_eq!(state.status("OTHER-2"), Some(CardStatus::Blocked));

    let session = runner.session();
    let triage = &session.calls()[0].request;
    assert_eq!(triage.mode, crate::session::Mode::Triage);
    assert!(triage.prompt.starts_with(crate::config::TRIAGE_OBJECTIVE));
    assert_eq!(triage.model, "sonnet");
    assert_eq!(triage.cwd, env.paths.state_dir);
    assert_eq!(
        crate::prompts::context_of(&triage.prompt)["cursors"],
        json!({"tracker:linear": "old"})
    );
    let card_ex = session.card_call("EX-1").unwrap().request;
    assert_eq!(card_ex.model, "claude-opus-5-5");
    assert_eq!(card_ex.effort, Effort::High);
    assert_eq!(
        card_ex.cwd, env.config.workspaces[0].path,
        "not a git checkout: used as is"
    );
    assert_eq!(
        session.card_call("OTHER-2").unwrap().request.cwd,
        env.paths.state_dir
    );
    assert_eq!(session.calls().len(), 3);

    let lines = lines_of(&lines);
    assert_eq!(lines.len(), 3);
    assert!(
        lines[0].contains("triage ok events=0 handled=1 cards=3 discussions=0 cost=$0.05"),
        "{}",
        lines[0]
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("card done EX-1 cost=$0.05 — implemented"))
    );
    assert!(lines.iter().any(|l| l.contains("card blocked OTHER-2")));
    assert!(lines[0].starts_with("2026-01-15T09:30:00Z "));
}

#[tokio::test]
async fn card_context_includes_all_workspaces() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(card_output("EX-3", "done"))]);
    let (runner, _) = make_runner(&env, session);
    runner.run_card("EX-3", None).await.unwrap();
    let context = crate::prompts::context_of(&runner.session().calls()[0].request.prompt);
    assert_eq!(context["ref"], "EX-3");
    assert_eq!(context["workspace"]["name"], "example-app");
    assert_eq!(context["workspaces"][0]["name"], "example-app");
    assert_eq!(
        context["outreach_file"],
        env.paths.outreach_file.display().to_string()
    );
}

#[tokio::test(start_paused = true)]
async fn respects_max_cards_and_skips_in_progress() {
    let env = test_env();
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &[]), ("EX-3", &[]), ("EX-4", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| ok(card_output(r, "done")));
    let (runner, _) = make_runner(&env, session);
    seed_card(&runner, "EX-1", CardStatus::InProgress);
    runner.schedule(TEN_MINUTES, true).await;
    let mut cards = runner.session().labels()[1..].to_vec();
    cards.sort();
    assert_eq!(cards, ["card EX-2", "card EX-3"]);
}

#[tokio::test]
async fn failed_triage_reports_failure() {
    let env = test_env();
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![fail("budget exceeded")]));
    assert!(runner.triage(vec![]).await.is_none());
    assert!(lines_of(&lines)[0].contains("triage api_error budget exceeded"));
}

#[tokio::test]
async fn invalid_structured_output_is_a_failure() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(json!({"unexpected": true}))]);
    let (runner, lines) = make_runner(&env, session);
    assert!(runner.triage(vec![]).await.is_none());
    assert!(lines_of(&lines)[0].contains("triage invalid_output invalid triage result"));
}

#[tokio::test(start_paused = true)]
async fn a_failed_session_schedules_a_retry() {
    let env = test_env();
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![fail("boom")]));
    assert!(runner.run_card("EX-1", None).await.unwrap().is_none());
    let card = card_of(&runner, "EX-1");
    assert_eq!(card.status, CardStatus::Failed);
    assert_eq!(card.attempts, 1);
    assert_eq!(card.retry_at, Some(now() + chrono::TimeDelta::minutes(1)));
    assert!(
        lines_of(&lines)[0]
            .contains("card api_error EX-1 cost=$0.05 — boom; retry at 2026-01-15T09:31:00Z"),
        "{:?}",
        lines_of(&lines)
    );
}

#[tokio::test]
async fn card_result_statuses_are_recorded() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(card_output("EX-1", "failed"))]);
    let (runner, lines) = make_runner(&env, session);
    let result = runner.run_card("EX-1", None).await.unwrap().unwrap();
    assert_eq!(result.status, CardOutcome::Failed);
    assert_eq!(status_of(&runner, "EX-1"), Some(CardStatus::Failed));
    assert!(lines_of(&lines)[0].contains("card failed EX-1 cost=$0.05"));
}

#[tokio::test(start_paused = true)]
async fn heartbeat_once_runs_a_single_triage() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(triage_output(&[]))]);
    let (runner, _) = make_runner(&env, session);
    runner.heartbeat(TEN_MINUTES, true).await;
    assert_eq!(runner.session().calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn heartbeat_loop_backs_off_then_stops() {
    let env = test_env();
    let kill_switch = env.paths.kill_switch();
    let count = AtomicUsize::new(0);
    let session = FakeSession::new(move |_| match count.fetch_add(1, Ordering::SeqCst) {
        0 | 1 => fail("boom"),
        2 => ok(triage_output(&[])),
        _ => {
            std::fs::write(&kill_switch, "").unwrap();
            ok(triage_output(&[]))
        }
    });
    let (runner, lines) = make_runner(&env, session);
    runner.heartbeat(TEN_MINUTES, false).await;
    let starts: Vec<Instant> = runner.session().calls().iter().map(|c| c.started).collect();
    let gaps: Vec<Duration> = starts.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(gaps, [BACKOFF_BASE, BACKOFF_BASE * 2, TEN_MINUTES]);
    assert!(
        lines_of(&lines)
            .last()
            .unwrap()
            .contains("triage stopped kill switch present")
    );
}

#[tokio::test(start_paused = true)]
async fn kill_switch_prevents_ticks() {
    let env = test_env();
    std::fs::create_dir_all(&env.paths.state_dir).unwrap();
    std::fs::write(env.paths.kill_switch(), "").unwrap();
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![]));
    runner.heartbeat(TEN_MINUTES, false).await;
    assert!(runner.session().calls().is_empty());
    assert!(lines_of(&lines)[0].contains("stopped kill switch present"));
}

#[tokio::test(start_paused = true)]
async fn kill_switch_between_cards() {
    let mut env = test_env();
    env.config.card.max_parallel = 1;
    let kill_switch = env.paths.kill_switch();
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], move |r| {
        std::fs::write(&kill_switch, "").unwrap();
        ok(card_output(r, "done"))
    });
    let (runner, _) = make_runner(&env, session);
    runner.heartbeat(TEN_MINUTES, false).await;
    assert_eq!(runner.session().calls().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn sleep_wakes_on_kill_switch() {
    let env = test_env();
    let (runner, _) = make_runner(&env, FakeSession::sequence(vec![ok(triage_output(&[]))]));
    let kill_switch = env.paths.kill_switch();
    tokio::spawn(async move {
        sleep(Duration::from_secs(7)).await;
        std::fs::write(kill_switch, "").unwrap();
    });
    let start = Instant::now();
    runner.heartbeat(TEN_MINUTES, false).await;
    assert!(start.elapsed() <= Duration::from_secs(7) + KILL_SWITCH_POLL);
}

#[tokio::test]
async fn run_card_with_named_workspace() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(card_output("ZZ-1", "done"))]);
    let (runner, _) = make_runner(&env, session);
    runner.run_card("ZZ-1", Some("example-app")).await.unwrap();
    assert_eq!(
        runner.session().calls()[0].request.cwd,
        env.config.workspaces[0].path
    );
}

#[tokio::test]
async fn run_card_unknown_workspace_is_an_error() {
    let env = test_env();
    let (runner, _) = make_runner(&env, FakeSession::sequence(vec![]));
    let error = runner.run_card("EX-1", Some("missing")).await.unwrap_err();
    assert!(error.to_string().contains("no workspace"));
    assert!(runner.session().calls().is_empty());
}

#[tokio::test]
async fn runner_accepts_default_config() {
    let env = test_env();
    let runner = Runner::new(
        crate::config::Config::default(),
        env.paths.clone(),
        env.store(),
        FakeSession::sequence(vec![]),
    );
    assert!(!runner.killed());
}

#[tokio::test(start_paused = true)]
async fn a_card_left_in_progress_is_retried_by_the_runner_not_by_triage() {
    let env = test_env();
    let kill_switch = env.paths.kill_switch();
    let triage = triage_output(&[("EX-1", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], move |r| {
        std::fs::write(&kill_switch, "").unwrap();
        ok(card_output(r, "done"))
    });
    let (runner, lines) = make_runner(&env, session);
    seed_card(&runner, "EX-1", CardStatus::InProgress);
    let start = Instant::now();
    runner.heartbeat(TEN_MINUTES, false).await;
    let call = runner.session().card_call("EX-1").unwrap();
    assert!(
        call.started - start >= MINUTE,
        "triage listed it at once, but the crash's retry waits a minute"
    );
    assert_eq!(runner.session().labels().len(), 2, "one triage, one card");
    assert_eq!(status_of(&runner, "EX-1"), Some(CardStatus::Done));
    assert_eq!(
        card_of(&runner, "EX-1").attempts,
        0,
        "done: the run is over"
    );
    assert!(
        lines_of(&lines)[0]
            .contains("card crash EX-1 — the runner stopped before the session ended; retry at")
    );
}

#[tokio::test(start_paused = true)]
async fn transient_failures_retry_until_max_attempts_then_escalate() {
    let mut env = test_env();
    env.config.card.max_attempts = 3;
    let kill_switch = env.paths.kill_switch();
    let triage = triage_output(&[("EX-1", &[])]);
    let count = AtomicUsize::new(0);
    // Triage lists the card once; every attempt times out; the third triage stops the loop.
    let session = FakeSession::new(move |request| match request.mode {
        crate::session::Mode::Card => {
            ended(crate::session::Ended::Timeout("no result within 3h".into()))
        }
        _ => {
            if count.fetch_add(1, Ordering::SeqCst) == 2 {
                std::fs::write(&kill_switch, "").unwrap();
            }
            ok(triage.clone())
        }
    });
    let (runner, lines) = make_runner(&env, session);
    runner.heartbeat(TEN_MINUTES, false).await;

    let session = runner.session();
    let cards: Vec<Instant> = session
        .calls()
        .iter()
        .filter(|c| c.request.mode == crate::session::Mode::Card)
        .map(|c| c.started)
        .collect();
    assert_eq!(cards.len(), 3, "{:?}", session.labels());
    let gaps: Vec<Duration> = cards.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps[0] >= MINUTE && gaps[0] < MINUTE + KILL_SWITCH_POLL * 2,
        "{gaps:?}"
    );
    assert!(
        gaps[1] >= MINUTE * 2 && gaps[1] < MINUTE * 2 + KILL_SWITCH_POLL * 2,
        "{gaps:?}"
    );
    let card = card_of(&runner, "EX-1");
    assert_eq!(card.status, CardStatus::NeedsHuman);
    assert_eq!(card.reason.as_deref(), Some("max_attempts"));
    assert_eq!(card.attempts, 3);
    assert!(
        lines_of(&lines).iter().any(|l| l.contains(
            "card timeout EX-1 cost=$0.05 — no result within 3h; needs_human (max_attempts)"
        )),
        "{:?}",
        lines_of(&lines)
    );
    // Later triages list it again, but a card that needs a person is not restarted; triage
    // sees it among its escalations instead.
    let last_triage = session
        .calls()
        .into_iter()
        .rfind(|c| c.request.mode == crate::session::Mode::Triage)
        .unwrap();
    assert_eq!(
        last_triage.context()["escalations"],
        json!([{"ref": "EX-1", "reason": "max_attempts", "attempts": 3,
                "last_summary": "no result within 3h"}])
    );
}

#[tokio::test(start_paused = true)]
async fn blocked_cards_wait_for_triage_and_retries_skip_the_triage_limit() {
    let mut env = test_env();
    env.config.triage.max_cards_per_tick = 1;
    let mut waiting = CardState::new(CardStatus::Failed, now());
    waiting.attempts = 1;
    waiting.retry_at = Some(now());
    waiting.blocked_by = vec!["EXT-1".into()];
    let triage = triage_output(&[("EX-2", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| match r {
        "EX-2" => ok(card_output(r, "blocked")),
        _ => ok(card_output(r, "done")),
    });
    let (runner, _) = make_runner(&env, session);
    seed(&runner, "EX-1", &waiting);
    runner.schedule(TEN_MINUTES, true).await;
    let retried = runner.session().card_call("EX-1").unwrap();
    assert_eq!(retried.request.mode, crate::session::Mode::Card);
    assert!(
        runner.session().card_call("EX-2").is_some(),
        "not held by the retry"
    );
    assert_eq!(status_of(&runner, "EX-1"), Some(CardStatus::Done));
    let blocked = card_of(&runner, "EX-2");
    assert_eq!(blocked.status, CardStatus::Blocked);
    assert_eq!(
        blocked.retry_at, None,
        "blocked is never retried by the runner"
    );
    assert_eq!(blocked.attempts, 1);
}

#[tokio::test(start_paused = true)]
async fn a_new_event_mentioning_a_card_that_needs_a_person_reopens_it() {
    let env = test_env();
    let mut escalated = CardState::new(CardStatus::NeedsHuman, now());
    escalated.attempts = 3;
    escalated.reason = Some("max_attempts".into());
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![ok(triage_output(&[]))]));
    seed(&runner, "EX-1", &escalated);
    seed(&runner, "EX-12", &escalated);
    let mut event = manual_event("1");
    event.payload = json!({"body": "EX-1 is unblocked, the decision is made"});
    env.store().enqueue("example-app", &event, now()).unwrap();
    runner.heartbeat(TEN_MINUTES, true).await;
    let reopened = card_of(&runner, "EX-1");
    assert_eq!(
        (reopened.status, reopened.attempts, reopened.reason),
        (CardStatus::Failed, 0, None)
    );
    assert_eq!(status_of(&runner, "EX-12"), Some(CardStatus::NeedsHuman));
    assert!(
        lines_of(&lines)
            .iter()
            .any(|l| l.contains("card reopened EX-1 — a new event mentions it"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_discussion_that_crashes_is_retried_in_a_fresh_session() {
    let env = test_env();
    let kill_switch = env.paths.kill_switch();
    let mut triage = triage_output(&[]);
    triage["discussions_to_run"] = json!([
        {"ref": "EX-9", "thread": "https://tracker.example.com/EX-9/c1", "question": "q1"}
    ]);
    let attempts = AtomicUsize::new(0);
    let session = FakeSession::new(move |request| match request.mode {
        crate::session::Mode::Discussion => {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                ended(crate::session::Ended::Crash(
                    "no result: exit status: 1".into(),
                ))
            } else {
                std::fs::write(&kill_switch, "").unwrap();
                ok(discussion_output("EX-9", "replied"))
            }
        }
        _ => ok(triage.clone()),
    });
    let (runner, lines) = make_runner(&env, session);
    runner.heartbeat(Duration::from_secs(3600), false).await;
    let starts: Vec<Instant> = runner
        .session()
        .calls()
        .iter()
        .filter(|c| c.request.mode == crate::session::Mode::Discussion)
        .map(|c| c.started)
        .collect();
    assert_eq!(starts.len(), 2);
    assert!(starts[1] - starts[0] >= MINUTE, "after the first backoff");
    let lines = lines_of(&lines);
    assert!(
        lines.iter().any(|l| l
            .contains("discussion crash EX-9 cost=$0.05 — no result: exit status: 1; retry at")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("discussion replied EX-9")));
}

#[tokio::test(start_paused = true)]
async fn a_retry_that_cannot_start_is_left_to_triage() {
    let env = test_env();
    let mut waiting = CardState::new(CardStatus::Failed, now());
    waiting.attempts = 1;
    waiting.retry_at = Some(now());
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![ok(triage_output(&[]))]));
    seed(&runner, "EX-1", &waiting);
    env.store()
        .claim("card:EX-1", "other-app", now(), crate::store::CLAIM_LEASE)
        .unwrap();
    runner.schedule(TEN_MINUTES, true).await;
    let skipped = lines_of(&lines)
        .iter()
        .filter(|l| l.contains("card skipped EX-1 — held by other-app"))
        .count();
    assert_eq!(skipped, 1);
    let card = card_of(&runner, "EX-1");
    assert_eq!((card.status, card.retry_at), (CardStatus::Failed, None));
}

#[tokio::test(start_paused = true)]
async fn a_usage_limit_pauses_every_session_start_and_is_not_a_failure() {
    let mut env = test_env();
    env.config.card.max_parallel = 1;
    env.config.triage.max_cards_per_tick = 5;
    let kill_switch = env.paths.kill_switch();
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &[])]);
    let tries = AtomicUsize::new(0);
    // EX-1 hits the limit once; after the pause EX-2 and EX-1's retry finish, then the loop
    // stops.
    let session = FakeSession::routed(vec![ok(triage)], move |r| match r {
        "EX-1" if tries.fetch_add(1, Ordering::SeqCst) == 0 => rate_limited(None),
        "EX-1" => {
            std::fs::write(&kill_switch, "").unwrap();
            ok(card_output(r, "done"))
        }
        _ => ok(card_output(r, "done")),
    });
    let (runner, lines) = make_runner(&env, session);
    let start = Instant::now();
    runner.heartbeat(Duration::from_secs(3600), false).await;

    let session = runner.session();
    let calls = session.calls();
    let cards: Vec<(String, Duration)> = calls
        .iter()
        .filter(|c| c.request.mode == crate::session::Mode::Card)
        .map(|c| (c.reference(), c.started - start))
        .collect();
    assert_eq!(cards.len(), 3, "{cards:?}");
    assert_eq!(cards[0], ("EX-1".to_string(), Duration::ZERO));
    let pause = Duration::from_secs(15 * 60);
    for (card_ref, at) in &cards[1..] {
        assert!(
            *at >= pause && *at < pause + KILL_SWITCH_POLL * 2,
            "{card_ref} waited out the pause: {cards:?}"
        );
    }
    let lines = lines_of(&lines);
    assert!(
        lines.iter().any(|l| l
            .contains("pause set until 2026-01-15T09:45:00Z — usage or rate limit in card EX-1")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("card rate_limited EX-1")
            && l.ends_with("retry at 2026-01-15T09:45:00Z")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("pause lifted")),
        "{lines:?}"
    );
    let ex1 = card_of(&runner, "EX-1");
    assert_eq!(ex1.status, CardStatus::Done);
    let attempts = runner
        .store()
        .attempts(runner.name(), Some("EX-1"), -1)
        .unwrap();
    assert_eq!(
        attempts.last().unwrap().outcome.as_deref(),
        Some("rate_limited"),
        "recorded, but not counted as a failure"
    );
}

#[tokio::test(start_paused = true)]
async fn once_starts_nothing_while_another_runner_has_paused_the_machine() {
    let env = test_env();
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![]));
    env.store()
        .set_pause(
            now() + chrono::TimeDelta::minutes(15),
            "usage or rate limit in card EX-9",
            "other-app",
            now(),
        )
        .unwrap();
    runner.heartbeat(TEN_MINUTES, true).await;
    assert!(runner.session().calls().is_empty());
    let lines = lines_of(&lines);
    assert!(
        lines[0].contains("pause waiting until 2026-01-15T09:45:00Z — usage or rate limit in card EX-9 (set by other-app)"),
        "{lines:?}"
    );
    assert!(lines[1].contains("triage skipped paused until 2026-01-15T09:45:00Z"));
}

#[test]
fn jitter_adds_up_to_half_the_stagger() {
    let base = Duration::from_secs(30);
    assert_eq!(jittered(base, 0), base);
    assert_eq!(jittered(base, 500), Duration::from_secs(45));
    assert_eq!(jittered(base, 501), base, "wraps");
    for random in [1, 250, 499, 12_345, u64::MAX] {
        let delay = jittered(base, random);
        assert!(delay >= base && delay <= base + base / 2, "{delay:?}");
    }
    assert_eq!(jittered(Duration::ZERO, 400), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn sessions_start_a_stagger_apart_even_in_once() {
    let mut env = test_env();
    env.config.sessions.start_stagger = Duration::from_secs(30);
    env.config.triage.max_cards_per_tick = 5;
    env.config.card.max_parallel = 3;
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| {
        ok(card_output(r, "done")).after(TEN_MINUTES)
    });
    let (runner, _) = make_runner(&env, session);
    runner.heartbeat(TEN_MINUTES, true).await;
    let starts: Vec<Instant> = runner.session().calls().iter().map(|c| c.started).collect();
    assert_eq!(starts.len(), 3, "triage and both cards ran");
    for gap in starts.windows(2).map(|w| w[1] - w[0]) {
        let base = Duration::from_secs(30);
        assert!(
            gap >= base && gap <= base + base / 2 + Duration::from_millis(5),
            "{gap:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn once_does_not_wait_out_the_stagger_with_nothing_queued() {
    let mut env = test_env();
    env.config.sessions.start_stagger = Duration::from_secs(30);
    let session = FakeSession::sequence(vec![ok(triage_output(&[])).after(MINUTE / 6)]);
    let (runner, _) = make_runner(&env, session);
    let start = Instant::now();
    runner.heartbeat(TEN_MINUTES, true).await;
    assert_eq!(start.elapsed(), MINUTE / 6, "exits when triage ends");
}

#[tokio::test(start_paused = true)]
async fn an_overdue_sweep_waits_out_the_pause_without_spinning() {
    let env = test_env();
    let kill_switch = env.paths.kill_switch();
    // A sweep is due every minute, all through a 15 minute pause set by another runner.
    let session = FakeSession::new(move |_| {
        std::fs::write(&kill_switch, "").unwrap();
        ok(triage_output(&[]))
    });
    let (runner, _) = make_runner(&env, session);
    env.store()
        .set_pause(
            now() + chrono::TimeDelta::minutes(15),
            "usage or rate limit in card EX-9",
            "other-app",
            now(),
        )
        .unwrap();
    let start = Instant::now();
    runner.heartbeat(MINUTE, false).await;
    let calls = runner.session().calls();
    assert_eq!(calls.len(), 1);
    let waited = calls[0].started - start;
    assert!(
        waited >= MINUTE * 15 && waited < MINUTE * 15 + KILL_SWITCH_POLL * 2,
        "{waited:?}"
    );
}

#[test]
fn refs_are_matched_as_whole_tokens() {
    assert!(mentions_ref("see EX-1.", "EX-1"));
    assert!(mentions_ref("{\"body\":\"EX-1\"}", "EX-1"));
    assert!(!mentions_ref("see EX-12", "EX-1"));
    assert!(!mentions_ref("PREX-1 is done", "EX-1"));
    assert!(!mentions_ref("anything", ""));
}

#[tokio::test(start_paused = true)]
async fn cards_run_in_parallel_up_to_max_parallel() {
    let mut env = test_env();
    env.config.triage.max_cards_per_tick = 5;
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &[]), ("EX-3", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| {
        ok(card_output(r, "done")).after(TEN_MINUTES)
    });
    let (runner, _) = make_runner(&env, session);
    let start = Instant::now();
    runner.schedule(TEN_MINUTES, true).await;
    assert_eq!(runner.session().max_active.load(Ordering::SeqCst), 2);
    assert_eq!(start.elapsed(), TEN_MINUTES * 2);
    for r in ["EX-1", "EX-2", "EX-3"] {
        assert_eq!(status_of(&runner, r), Some(CardStatus::Done), "{r}");
    }
}

#[tokio::test(start_paused = true)]
async fn blocked_by_orders_cards() {
    let mut env = test_env();
    env.config.triage.max_cards_per_tick = 5;
    let triage = triage_output(&[("EX-2", &["EX-1"]), ("EX-1", &[]), ("EX-3", &["EXT-9"])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| {
        let delay = if r == "EX-1" { TEN_MINUTES } else { MINUTE };
        ok(card_output(r, "done")).after(delay)
    });
    let (runner, _) = make_runner(&env, session);
    runner.schedule(TEN_MINUTES, true).await;
    let session = runner.session();
    let triage_at = session.calls()[0].started;
    let started = |r: &str| session.card_call(r).unwrap().started - triage_at;
    assert_eq!(started("EX-1"), Duration::ZERO);
    assert_eq!(
        started("EX-3"),
        Duration::ZERO,
        "external blocker does not hold a card"
    );
    assert_eq!(started("EX-2"), TEN_MINUTES, "starts once EX-1 is done");
}

#[tokio::test(start_paused = true)]
async fn unfinished_blocker_holds_the_card() {
    let mut env = test_env();
    env.config.triage.max_cards_per_tick = 5;
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &["EX-1"]), ("EX-3", &["EX-7"])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| ok(card_output(r, "blocked")));
    let (runner, _) = make_runner(&env, session);
    seed_card(&runner, "EX-7", CardStatus::Failed);
    runner.schedule(TEN_MINUTES, true).await;
    assert!(runner.session().card_call("EX-1").is_some());
    assert!(runner.session().card_call("EX-2").is_none());
    assert!(runner.session().card_call("EX-3").is_none());
}

#[tokio::test(start_paused = true)]
async fn triage_keeps_ticking_while_cards_run() {
    let env = test_env();
    let kill_switch = env.paths.kill_switch();
    let triage = triage_output(&[("EX-1", &[])]);
    // Every triage lists EX-1; it must not be started again while it runs.
    let session = FakeSession::routed(
        vec![ok(triage.clone()), ok(triage.clone()), ok(triage)],
        |r| ok(card_output(r, "done")).after(Duration::from_secs(25 * 60)),
    );
    let (runner, _) = make_runner(&env, session);
    tokio::spawn(async move {
        sleep(Duration::from_secs(24 * 60)).await;
        std::fs::write(kill_switch, "").unwrap();
    });
    runner.heartbeat(TEN_MINUTES, false).await;
    let lines = runner.session().labels();
    let triages = lines.iter().filter(|l| *l == "triage").count();
    let cards = lines.iter().filter(|l| l.starts_with("card ")).count();
    assert_eq!(triages, 3, "t=0, 10m, 20m while the card runs: {lines:?}");
    assert_eq!(cards, 1);
    assert_eq!(
        status_of(&runner, "EX-1"),
        Some(CardStatus::Failed),
        "terminated by the stop"
    );
}

#[tokio::test(start_paused = true)]
async fn stop_terminates_running_cards_and_starts_no_more() {
    let mut env = test_env();
    env.config.card.max_parallel = 1;
    env.config.triage.max_cards_per_tick = 5;
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| {
        ok(card_output(r, "done")).after(TEN_MINUTES)
    });
    let (runner, lines) = make_runner(&env, session);
    let shutdown = runner.shutdown_handle();
    tokio::spawn(async move {
        sleep(MINUTE).await;
        shutdown.send_replace(Shutdown::Graceful);
    });
    let start = Instant::now();
    runner.heartbeat(TEN_MINUTES, false).await;
    assert!(start.elapsed() < TEN_MINUTES);
    assert!(runner.session().card_call("EX-2").is_none());
    assert_eq!(status_of(&runner, "EX-1"), Some(CardStatus::Failed));
    assert!(lines_of(&lines).iter().any(|l| l.contains(
        "card interrupted EX-1 cost=$0.05 — interrupted: dispatch is stopping; retry at"
    )));
}

#[tokio::test(start_paused = true)]
async fn requeued_triage_drops_cards_no_longer_listed() {
    let mut env = test_env();
    env.config.card.max_parallel = 1;
    env.config.triage.max_cards_per_tick = 5;
    let first = triage_output(&[("EX-1", &[]), ("EX-2", &[])]);
    let second = triage_output(&[("EX-3", &[])]);
    let kill_switch = env.paths.kill_switch();
    let session = FakeSession::routed(vec![ok(first), ok(second)], move |r| {
        if r == "EX-3" {
            std::fs::write(&kill_switch, "").unwrap();
        }
        ok(card_output(r, "done")).after(Duration::from_secs(15 * 60))
    });
    let (runner, _) = make_runner(&env, session);
    runner.heartbeat(TEN_MINUTES, false).await;
    assert!(runner.session().card_call("EX-1").is_some());
    assert!(
        runner.session().card_call("EX-2").is_none(),
        "dropped by the second triage"
    );
    assert!(runner.session().card_call("EX-3").is_some());
}

fn manual_event(external_id: &str) -> crate::intake::IncomingEvent {
    crate::intake::IncomingEvent {
        source: "manual".into(),
        external_id: external_id.into(),
        kind: crate::intake::EventKind::Message,
        mentions_me: false,
        sender: None,
        occurred_at: now(),
        payload: json!({"body": "test event"}),
    }
}

fn event_count(call: &crate::testing::Call) -> usize {
    crate::prompts::context_of(&call.request.prompt)["events"]
        .as_array()
        .map_or(0, Vec::len)
}

#[tokio::test(start_paused = true)]
async fn a_batch_window_starts_triage_with_the_events() {
    let env = test_env();
    let kill_switch = env.paths.kill_switch();
    let count = AtomicUsize::new(0);
    let session = FakeSession::new(move |_| {
        if count.fetch_add(1, Ordering::SeqCst) == 1 {
            std::fs::write(&kill_switch, "").unwrap();
        }
        ok(triage_output(&[]))
    });
    let (runner, lines) = make_runner(&env, session);
    let store = env.store();
    store
        .enqueue("example-app", &manual_event("1"), now())
        .unwrap();
    store
        .enqueue("example-app", &manual_event("2"), now())
        .unwrap();
    let start = Instant::now();
    runner.heartbeat(Duration::from_secs(30 * 60), false).await;

    let calls = runner.session().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(event_count(&calls[0]), 0, "t=0: the fallback sweep");
    assert_eq!(calls[1].started - start, MINUTE, "the batch window closed");
    assert_eq!(event_count(&calls[1]), 2);
    assert_eq!(store.new_event_count("example-app").unwrap(), 0);
    let context = crate::prompts::context_of(&calls[1].request.prompt);
    let id: i64 = context["events"][0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(store.event_status(id).unwrap().as_deref(), Some("done"));
    assert!(
        lines_of(&lines)
            .iter()
            .any(|l| l.contains("triage ok events=2"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_batch_returns_to_new_and_is_retried_with_backoff() {
    let env = test_env();
    let kill_switch = env.paths.kill_switch();
    let count = AtomicUsize::new(0);
    // Sweep ok, first batch fails, retried batch succeeds and stops the loop.
    let session = FakeSession::new(move |_| match count.fetch_add(1, Ordering::SeqCst) {
        0 => ok(triage_output(&[])),
        1 => fail("boom"),
        _ => {
            std::fs::write(&kill_switch, "").unwrap();
            ok(triage_output(&[]))
        }
    });
    let (runner, _) = make_runner(&env, session);
    let store = env.store();
    store
        .enqueue("example-app", &manual_event("1"), now())
        .unwrap();
    let start = Instant::now();
    runner.heartbeat(Duration::from_secs(30 * 60), false).await;

    let calls = runner.session().calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(event_count(&calls[1]), 1);
    assert_eq!(event_count(&calls[2]), 1, "the same event, back from new");
    assert_eq!(calls[1].started - start, MINUTE);
    assert_eq!(
        calls[2].started - calls[1].started,
        MINUTE,
        "the window again, which outlasts the 30s backoff"
    );
    assert_eq!(store.new_event_count("example-app").unwrap(), 0);
}

#[tokio::test(start_paused = true)]
async fn once_takes_pending_events_immediately() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(triage_output(&[]))]);
    let (runner, _) = make_runner(&env, session);
    env.store()
        .enqueue("example-app", &manual_event("1"), now())
        .unwrap();
    let start = Instant::now();
    runner.heartbeat(TEN_MINUTES, true).await;
    let calls = runner.session().calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].started, start);
    assert_eq!(event_count(&calls[0]), 1);
}

#[tokio::test(start_paused = true)]
async fn events_of_other_runners_are_not_taken() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(triage_output(&[]))]);
    let (runner, _) = make_runner(&env, session);
    let store = env.store();
    store
        .enqueue("other-app", &manual_event("1"), now())
        .unwrap();
    runner.heartbeat(TEN_MINUTES, true).await;
    assert_eq!(event_count(&runner.session().calls()[0]), 0);
    assert_eq!(store.new_event_count("other-app").unwrap(), 1);
}

#[tokio::test(start_paused = true)]
async fn discussions_share_max_parallel_with_cards() {
    let mut env = test_env();
    env.config.card.max_parallel = 1;
    env.config.triage.max_cards_per_tick = 5;
    let mut triage = triage_output(&[("EX-1", &[]), ("EX-2", &[])]);
    triage["discussions_to_run"] = json!([
        {"ref": "EX-9", "thread": "https://tracker.example.com/EX-9/c1", "question": "q1"},
        {"ref": "EX-9", "thread": "https://tracker.example.com/EX-9/c1", "question": "duplicate"},
    ]);
    let session = FakeSession::routed(vec![ok(triage)], |r| {
        ok(card_output(r, "done")).after(TEN_MINUTES)
    });
    let (runner, lines) = make_runner(&env, session);
    runner.schedule(TEN_MINUTES, true).await;

    let session = runner.session();
    assert_eq!(session.max_active.load(Ordering::SeqCst), 1);
    let firsts = session.labels();
    assert_eq!(
        firsts
            .iter()
            .filter(|l| l.starts_with("discussion "))
            .count(),
        1,
        "deduplicated by claim key: {firsts:?}"
    );
    assert_eq!(firsts[1], "discussion EX-9", "discussions first");
    let triage_at = session.calls()[0].started;
    assert_eq!(session.card_call("EX-1").unwrap().started, triage_at);
    assert_eq!(
        session.card_call("EX-2").unwrap().started - triage_at,
        TEN_MINUTES,
        "waits for a slot"
    );
    assert!(
        lines_of(&lines)
            .iter()
            .any(|l| l.contains("discussion drafted EX-9"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_panicked_triage_returns_its_events_to_new() {
    let env = test_env();
    let session = FakeSession::new(|_| panic!("triage task blew up"));
    let (runner, _) = make_runner(&env, session);
    let store = env.store();
    let Ok(crate::store::Enqueued::Inserted(id)) =
        store.enqueue("example-app", &manual_event("1"), now())
    else {
        panic!("not inserted");
    };
    runner.heartbeat(TEN_MINUTES, true).await;
    assert_eq!(store.event_status(id).unwrap().as_deref(), Some("new"));
}

#[tokio::test(start_paused = true)]
async fn a_card_that_ends_without_a_status_keeps_holding_its_dependents() {
    let mut env = test_env();
    env.config.triage.max_cards_per_tick = 5;
    let triage = triage_output(&[("EX-1", &[]), ("EX-2", &["EX-1"])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| ok(card_output(r, "done")));
    let (runner, lines) = make_runner(&env, session);
    // Another runner works EX-1: this runner skips it and records nothing.
    env.store()
        .claim("card:EX-1", "other-app", now(), crate::store::CLAIM_LEASE)
        .unwrap();
    runner.schedule(TEN_MINUTES, true).await;
    assert!(runner.session().card_call("EX-1").is_none());
    assert!(
        runner.session().card_call("EX-2").is_none(),
        "EX-1 is not done: {:?}",
        lines_of(&lines)
    );
    assert!(
        lines_of(&lines)
            .iter()
            .any(|l| l.contains("card skipped EX-1 — held by other-app"))
    );
}

#[tokio::test(start_paused = true)]
async fn triage_cannot_overwrite_intake_cursors() {
    let env = test_env();
    let mut triage = triage_output(&[]);
    triage["cursors"] = json!({"intake:linear:work": "rewound", "slack:C0000000001": "c2"});
    let (runner, _) = make_runner(&env, FakeSession::sequence(vec![ok(triage)]));
    let seeded = std::collections::BTreeMap::from([(
        "intake:linear:work".to_string(),
        "2026-01-15T09:30:00.000Z".to_string(),
    )]);
    runner.store().set_cursors(runner.name(), &seeded).unwrap();
    runner.heartbeat(TEN_MINUTES, true).await;
    let cursors = runner.store().load_state(runner.name()).unwrap().cursors;
    assert_eq!(cursors["intake:linear:work"], "2026-01-15T09:30:00.000Z");
    assert_eq!(cursors["slack:C0000000001"], "c2");
}
