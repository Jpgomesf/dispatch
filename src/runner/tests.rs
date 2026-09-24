use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::testing::*;

const MINUTE: Duration = Duration::from_secs(60);
const TEN_MINUTES: Duration = Duration::from_secs(600);

fn lines_of(lines: &Lines) -> Vec<String> {
    lines.lock().unwrap().clone()
}

fn status_of(runner: &Runner<FakeSession>, card_ref: &str) -> Option<CardStatus> {
    runner.store().load().unwrap().status(card_ref)
}

fn seed(runner: &Runner<FakeSession>, change: impl FnOnce(&mut State)) {
    runner.store().update(change).unwrap();
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
    let mut state = State::default();
    state.set_card("EX-1", CardStatus::Done, now(), None);
    state.set_card("EX-2", CardStatus::Failed, now(), None);
    let batch: HashSet<&str> = ["EX-5"].into();
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
    let mut triage = heartbeat_output(&[("EX-1", &[]), ("EX-1", &[]), ("OTHER-2", &[])]);
    triage["cursors"] = json!({"slack:C0000000001": "c9"});
    let session = FakeSession::routed(vec![ok(triage)], |card_ref| match card_ref {
        "EX-1" => ok(card_output("EX-1", "done")),
        _ => ok(card_output(card_ref, "blocked")),
    });
    let (runner, lines) = make_runner(&env, session);
    seed(&runner, |s| {
        s.cursors.insert("tracker:linear".into(), "old".into());
    });

    runner.heartbeat(TEN_MINUTES, true).await;

    let state = runner.store().load().unwrap();
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
    assert!(
        triage
            .prompt
            .starts_with("/claude-harness:workflow heartbeat\n")
    );
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
        lines[0].contains("heartbeat ok handled=1 cards=3 cost=$0.05"),
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
    let triage = heartbeat_output(&[("EX-1", &[]), ("EX-2", &[]), ("EX-3", &[]), ("EX-4", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| ok(card_output(r, "done")));
    let (runner, _) = make_runner(&env, session);
    seed(&runner, |s| {
        s.set_card("EX-1", CardStatus::InProgress, now(), None)
    });
    runner.schedule(TEN_MINUTES, true).await;
    let mut cards = runner.session().first_lines()[1..].to_vec();
    cards.sort();
    assert_eq!(
        cards,
        [
            "/claude-harness:workflow card EX-2",
            "/claude-harness:workflow card EX-3"
        ]
    );
}

#[tokio::test]
async fn failed_triage_reports_failure() {
    let env = test_env();
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![fail("budget exceeded")]));
    assert!(runner.triage().await.is_none());
    assert!(lines_of(&lines)[0].contains("heartbeat failed budget exceeded"));
}

#[tokio::test]
async fn invalid_structured_output_is_a_failure() {
    let env = test_env();
    let session = FakeSession::sequence(vec![ok(json!({"unexpected": true}))]);
    let (runner, lines) = make_runner(&env, session);
    assert!(runner.triage().await.is_none());
    assert!(lines_of(&lines)[0].contains("heartbeat failed invalid heartbeat result"));
}

#[tokio::test]
async fn failed_card_is_marked_failed() {
    let env = test_env();
    let (runner, lines) = make_runner(&env, FakeSession::sequence(vec![fail("boom")]));
    assert!(runner.run_card("EX-1", None).await.unwrap().is_none());
    assert_eq!(status_of(&runner, "EX-1"), Some(CardStatus::Failed));
    assert!(lines_of(&lines)[0].contains("card failed EX-1 — boom"));
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
    let session = FakeSession::sequence(vec![ok(heartbeat_output(&[]))]);
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
        2 => ok(heartbeat_output(&[])),
        _ => {
            std::fs::write(&kill_switch, "").unwrap();
            ok(heartbeat_output(&[]))
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
            .contains("heartbeat stopped kill switch present")
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
    let triage = heartbeat_output(&[("EX-1", &[]), ("EX-2", &[])]);
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
    let (runner, _) = make_runner(&env, FakeSession::sequence(vec![ok(heartbeat_output(&[]))]));
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
        Config::default(),
        env.paths.clone(),
        FakeSession::sequence(vec![]),
    );
    assert!(!runner.killed());
}

#[tokio::test(start_paused = true)]
async fn heartbeat_releases_cards_left_in_progress() {
    let env = test_env();
    let triage = heartbeat_output(&[("EX-1", &[])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| ok(card_output(r, "done")));
    let (runner, _) = make_runner(&env, session);
    seed(&runner, |s| {
        s.set_card("EX-1", CardStatus::InProgress, now(), None)
    });
    runner.heartbeat(TEN_MINUTES, true).await;
    assert!(runner.session().card_call("EX-1").is_some());
    assert_eq!(status_of(&runner, "EX-1"), Some(CardStatus::Done));
}

#[tokio::test(start_paused = true)]
async fn cards_run_in_parallel_up_to_max_parallel() {
    let mut env = test_env();
    env.config.heartbeat.max_cards_per_tick = 5;
    let triage = heartbeat_output(&[("EX-1", &[]), ("EX-2", &[]), ("EX-3", &[])]);
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
    env.config.heartbeat.max_cards_per_tick = 5;
    let triage = heartbeat_output(&[("EX-2", &["EX-1"]), ("EX-1", &[]), ("EX-3", &["EXT-9"])]);
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
    env.config.heartbeat.max_cards_per_tick = 5;
    let triage = heartbeat_output(&[("EX-1", &[]), ("EX-2", &["EX-1"]), ("EX-3", &["EX-7"])]);
    let session = FakeSession::routed(vec![ok(triage)], |r| ok(card_output(r, "blocked")));
    let (runner, _) = make_runner(&env, session);
    seed(&runner, |s| {
        s.set_card("EX-7", CardStatus::Failed, now(), None)
    });
    runner.schedule(TEN_MINUTES, true).await;
    assert!(runner.session().card_call("EX-1").is_some());
    assert!(runner.session().card_call("EX-2").is_none());
    assert!(runner.session().card_call("EX-3").is_none());
}

#[tokio::test(start_paused = true)]
async fn triage_keeps_ticking_while_cards_run() {
    let env = test_env();
    let kill_switch = env.paths.kill_switch();
    let triage = heartbeat_output(&[("EX-1", &[])]);
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
    let lines = runner.session().first_lines();
    let triages = lines.iter().filter(|l| l.ends_with("heartbeat")).count();
    let cards = lines.iter().filter(|l| l.contains(" card ")).count();
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
    env.config.heartbeat.max_cards_per_tick = 5;
    let triage = heartbeat_output(&[("EX-1", &[]), ("EX-2", &[])]);
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
    assert!(
        lines_of(&lines)
            .iter()
            .any(|l| l.contains("card failed EX-1 — terminated"))
    );
}

#[tokio::test(start_paused = true)]
async fn requeued_triage_drops_cards_no_longer_listed() {
    let mut env = test_env();
    env.config.card.max_parallel = 1;
    env.config.heartbeat.max_cards_per_tick = 5;
    let first = heartbeat_output(&[("EX-1", &[]), ("EX-2", &[])]);
    let second = heartbeat_output(&[("EX-3", &[])]);
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
