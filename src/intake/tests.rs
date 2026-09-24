use std::sync::Mutex;

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::SLACK_APP_ID;
use crate::intake::notifications::tests::{fixture_db, fixture_plist};
use crate::intake::secrets::tests::write_secrets;
use crate::testing::{TestEnv, now, test_env, tokio_clock};

type Lines = Arc<Mutex<Vec<String>>>;

fn context(env: &TestEnv) -> (IntakeContext, Lines) {
    let lines: Lines = Arc::default();
    let sink = lines.clone();
    let ctx = IntakeContext {
        config: Arc::new(env.config.clone()),
        paths: Arc::new(env.paths.clone()),
        store: env.store(),
        env: Arc::new(|_| None),
        clock: tokio_clock(),
        out: Arc::new(move |line| sink.lock().unwrap().push(line)),
        wake: Arc::new(Notify::new()),
    };
    (ctx, lines)
}

fn event(kind: EventKind, sender: Option<&str>, mentions_me: bool) -> IncomingEvent {
    IncomingEvent {
        source: "notifications".into(),
        external_id: "1".into(),
        kind,
        mentions_me,
        sender: sender.map(str::to_string),
        occurred_at: now(),
        payload: json!({}),
    }
}

#[test]
fn allow_senders_never_blocks_mentions_or_work() {
    let open = IntakeConfig::default();
    assert!(admitted(&open, &event(EventKind::Message, None, false)));
    let strict = IntakeConfig {
        allow_senders: vec!["Example Person".into()],
        ..IntakeConfig::default()
    };
    assert!(admitted(
        &strict,
        &event(
            EventKind::Message,
            Some("example person in #example-channel"),
            false
        )
    ));
    assert!(!admitted(
        &strict,
        &event(EventKind::Message, Some("Other Person"), false)
    ));
    assert!(!admitted(
        &strict,
        &event(EventKind::Discussion, None, false)
    ));
    assert!(admitted(
        &strict,
        &event(EventKind::Message, Some("Other Person"), true)
    ));
    assert!(admitted(
        &strict,
        &event(EventKind::Discussion, Some("Other Person"), true)
    ));
    assert!(admitted(&strict, &event(EventKind::Work, None, false)));
}

#[test]
fn backoff_doubles_from_the_poll_interval_and_caps() {
    let poll = Duration::from_secs(5);
    assert_eq!(source_backoff(poll, 1), Duration::from_secs(10));
    assert_eq!(source_backoff(poll, 3), Duration::from_secs(40));
    assert_eq!(source_backoff(poll, 40), MAX_BACKOFF);
    let slow = Duration::from_secs(3600);
    assert_eq!(
        source_backoff(slow, 2),
        slow,
        "never shorter than the interval cap"
    );
}

#[tokio::test]
async fn linear_idles_until_its_key_appears() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("HarnessWork"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"issues": {
            "nodes": [{"id": "uuid-1", "identifier": "EX-1", "title": "Example task",
                       "url": "https://linear.app/example/issue/EX-1", "updatedAt": "2026-01-15T10:00:00.000Z"}],
            "pageInfo": {"hasNextPage": false, "endCursor": null}}}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("HarnessDiscussion"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"notifications": {
            "nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}}})),
        )
        .mount(&server)
        .await;
    let mut env = test_env();
    env.config.intake.linear.enabled = true;
    env.config.intake.linear.api_url = server.uri();
    let (ctx, _) = context(&env);

    assert_eq!(
        poll_once(&ctx, SourceKind::Linear).await,
        Err(PollError::Idle("LINEAR_API_KEY missing".into()))
    );
    write_secrets(
        &env.paths.secrets,
        "LINEAR_API_KEY=lin_api_example\n",
        0o644,
    );
    let refused = poll_once(&ctx, SourceKind::Linear).await.unwrap_err();
    assert!(
        matches!(&refused, PollError::Idle(r) if r.contains("refused") && !r.contains("lin_api_example")),
        "{refused:?}"
    );

    write_secrets(
        &env.paths.secrets,
        "LINEAR_API_KEY=lin_api_example\n",
        0o600,
    );
    assert_eq!(
        poll_once(&ctx, SourceKind::Linear).await,
        Ok(0),
        "first run sets cursors"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    let store = env.store();
    let cursor = store
        .cursor("example-app", "intake:linear:work")
        .unwrap()
        .unwrap();
    assert_eq!(cursor, "2026-01-15T09:30:00.000Z");

    assert_eq!(poll_once(&ctx, SourceKind::Linear).await, Ok(1));
    assert_eq!(
        poll_once(&ctx, SourceKind::Linear).await,
        Ok(0),
        "deduplicated"
    );
    assert_eq!(
        store
            .cursor("example-app", "intake:linear:work")
            .unwrap()
            .unwrap(),
        "2026-01-15T10:00:00.000Z"
    );
    let batch = store.take_batch("example-app").unwrap();
    assert_eq!(batch[0].kind, EventKind::Work);
    assert_eq!(batch[0].payload["ref"], "EX-1");
}

#[tokio::test]
async fn jira_polls_both_streams_with_the_personal_scope() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/api/3/myself"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accountId": "acc-me"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .and(body_string_contains("assignee = currentUser() AND (project = EX)"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": [
            {"key": "EX-1", "fields": {"summary": "Example", "updated": "2026-01-15T09:45:00.000+0000"}}
        ], "isLast": true})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .and(body_string_contains("watcher = currentUser()"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": [
            {"key": "EX-2", "fields": {"summary": "Shared", "updated": "2026-01-15T09:46:00.000+0000",
             "comment": {"comments": [{"id": "5", "author": {"accountId": "acc-other", "displayName": "Example Person"},
                          "created": "2026-01-15T09:46:00.000+0000",
                          "body": {"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "ping"}]}]}}]}}}
        ], "isLast": true})))
        .mount(&server)
        .await;
    let mut env = test_env();
    env.config.intake.jira.enabled = true;
    env.config.intake.jira.base_url = server.uri();
    env.config.intake.jira.jql = "project = EX".into();
    write_secrets(
        &env.paths.secrets,
        "JIRA_EMAIL=dev@example.com\nJIRA_API_TOKEN=token\n",
        0o600,
    );
    let (ctx, _) = context(&env);
    assert_eq!(poll_once(&ctx, SourceKind::Jira).await, Ok(0));
    assert_eq!(poll_once(&ctx, SourceKind::Jira).await, Ok(2));
    let batch = env.store().take_batch("example-app").unwrap();
    let kinds: Vec<EventKind> = batch.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, [EventKind::Work, EventKind::Discussion]);
}

#[tokio::test]
async fn notifications_are_stored_and_wake_the_dispatcher() {
    let mut env = test_env();
    env.config.intake.notifications.enabled = true;
    env.config.intake.notifications.match_ = vec!["#example-channel".into()];
    let records = vec![(
        1,
        SLACK_APP_ID,
        fixture_plist("Example Person", Some("#example-channel"), "first"),
    )];
    env.paths.notifications_db = fixture_db(env.dir.path(), &records);
    let (ctx, lines) = context(&env);
    assert_eq!(poll_once(&ctx, SourceKind::Notifications).await, Ok(0));

    let conn = rusqlite::Connection::open(&env.paths.notifications_db).unwrap();
    let data = fixture_plist("Example Person", Some("#example-channel"), "second");
    conn.execute(
        "INSERT INTO record (rec_id, app_id, data) VALUES (2, 1, ?1)",
        [data],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO record (rec_id, app_id, data) VALUES (3, 1, x'00')",
        [],
    )
    .unwrap();
    assert_eq!(poll_once(&ctx, SourceKind::Notifications).await, Ok(1));
    tokio::time::timeout(Duration::from_secs(1), ctx.wake.notified())
        .await
        .expect("dispatcher woken");
    let lines = lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("intake notifications skipped record 3"),
        "{lines:?}"
    );
    let batch = env.store().take_batch("example-app").unwrap();
    assert_eq!(batch[0].payload["body"], "second");
}

#[tokio::test(start_paused = true)]
async fn a_source_prints_one_line_per_change_of_state() {
    let mut env = test_env();
    env.config.intake.linear.enabled = true;
    // Refused connections: no request leaves the machine.
    env.config.intake.linear.api_url = "http://127.0.0.1:9".into();
    let (ctx, lines) = context(&env);
    let task = tokio::spawn(run_source(ctx, SourceKind::Linear));
    tokio::time::sleep(Duration::from_secs(5 * 60 + 1)).await;
    write_secrets(
        &env.paths.secrets,
        "LINEAR_API_KEY=lin_api_example\n",
        0o600,
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    task.abort();
    let lines = lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].ends_with("intake linear idle — LINEAR_API_KEY missing"));
    assert!(
        lines[1].ends_with("intake linear ready"),
        "first run only sets cursors: {lines:?}"
    );
    assert!(lines.iter().all(|l| !l.contains("lin_api_example")));
}

#[tokio::test]
async fn card_scope_follows_enabled_trackers() {
    let env = test_env();
    let secrets = Secrets::load(&env.paths.secrets, Arc::new(|_| None));
    assert_eq!(
        card_scope(&env.config, &secrets, "EX-1").await,
        CardScope::Unchecked
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/api/3/myself"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accountId": "acc-me"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/api/3/issue/EX-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"fields": {"assignee": {"accountId": "acc-me"}}})),
        )
        .mount(&server)
        .await;
    let mut config = env.config.clone();
    config.intake.jira.enabled = true;
    config.intake.jira.base_url = server.uri();
    let refused = card_scope(&config, &secrets, "EX-1").await;
    assert_eq!(
        refused,
        CardScope::Refused("jira: JIRA_EMAIL missing".into())
    );

    let env_keys: secrets::EnvLookup = Arc::new(|name| match name {
        "JIRA_EMAIL" => Some("dev@example.com".into()),
        "JIRA_API_TOKEN" => Some("token".into()),
        _ => None,
    });
    let secrets = Secrets::load(&env.paths.secrets, env_keys);
    assert_eq!(card_scope(&config, &secrets, "EX-1").await, CardScope::Mine);
    assert_eq!(
        card_scope(&config, &secrets, "EX-404").await,
        CardScope::Refused("not found in jira".into())
    );
}
