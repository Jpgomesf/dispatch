//! Linear over GraphQL with the owner's personal API key (`Authorization: <key>`).
//!
//! - work: `issues(filter:)` where the filter always carries `assignee: {isMe: {eq: true}}`;
//!   config (`projects`, `teams`, `labels`) only adds narrowing clauses next to it.
//! - discussion: the personal `notifications` feed (`IssueNotification`), minus issues
//!   assigned to me and my own actions; one event per comment.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use super::event::{EventKind, Fetched, IncomingEvent, preview};
use super::http::{HttpError, client};
use crate::config::LinearConfig;

pub const SOURCE: &str = "linear";
/// Pages of 50 per fetch; the cursor catches up over the next polls if there are more.
const MAX_PAGES: usize = 10;

/// Filter for `issues`: personal scope first, then narrowing. No config value can remove or
/// replace the `assignee` clause: config only fills `in` lists of other fields.
#[must_use]
pub fn work_filter(config: &LinearConfig, updated_after: DateTime<Utc>) -> Value {
    let mut filter = json!({
        "assignee": {"isMe": {"eq": true}},
        "updatedAt": {"gt": crate::store::iso(updated_after)},
    });
    if !config.projects.is_empty() {
        filter["project"] = json!({"name": {"in": config.projects}});
    }
    if !config.teams.is_empty() {
        filter["team"] = json!({"or": [
            {"key": {"in": config.teams}},
            {"name": {"in": config.teams}},
        ]});
    }
    if !config.labels.is_empty() {
        filter["labels"] = json!({"some": {"name": {"in": config.labels}}});
    }
    filter
}

const WORK_QUERY: &str = "query HarnessWork($filter: IssueFilter!, $after: String) {
  issues(filter: $filter, first: 50, after: $after, orderBy: updatedAt) {
    nodes { id identifier title url updatedAt state { name } }
    pageInfo { hasNextPage endCursor }
  }
}";

const DISCUSSION_QUERY: &str =
    "query HarnessDiscussion($filter: NotificationFilter, $after: String) {
  notifications(filter: $filter, first: 50, after: $after) {
    nodes {
      id type createdAt
      actor { name email isMe }
      ... on IssueNotification {
        issue {
          identifier title url
          assignee { isMe }
          project { name }
          team { key name }
          labels { nodes { name } }
        }
        comment { id body url user { name email isMe } }
      }
    }
    pageInfo { hasNextPage endCursor }
  }
}";

const ASSIGNEE_QUERY: &str = "query HarnessAssignee($id: String!) {
  issue(id: $id) { identifier assignee { isMe } }
}";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    nodes: Vec<T>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

/// A connection read without paging (an issue's labels).
#[derive(Debug, Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    state: Option<Named>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Person {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    is_me: bool,
}

impl Person {
    fn label(&self) -> Option<String> {
        self.name.clone().or_else(|| self.email.clone())
    }
}

#[derive(Debug, Deserialize)]
struct IsMe {
    #[serde(rename = "isMe")]
    is_me: bool,
}

#[derive(Debug, Deserialize)]
struct Team {
    key: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct NotifiedIssue {
    identifier: String,
    title: String,
    url: String,
    assignee: Option<IsMe>,
    project: Option<Named>,
    team: Option<Team>,
    labels: Option<Nodes<Named>>,
}

#[derive(Debug, Deserialize)]
struct Comment {
    id: String,
    body: String,
    url: Option<String>,
    user: Option<Person>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Notification {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    created_at: DateTime<Utc>,
    actor: Option<Person>,
    /// Absent for notifications that are not `IssueNotification`.
    #[serde(default)]
    issue: Option<NotifiedIssue>,
    #[serde(default)]
    comment: Option<Comment>,
}

#[must_use]
pub fn work_event(issue: &Issue) -> IncomingEvent {
    IncomingEvent {
        source: SOURCE.into(),
        external_id: format!("{}@{}", issue.id, crate::store::iso(issue.updated_at)),
        kind: EventKind::Work,
        mentions_me: false,
        sender: None,
        occurred_at: issue.updated_at,
        payload: json!({
            "ref": issue.identifier,
            "title": issue.title,
            "subtitle": issue.state.as_ref().map(|s| s.name.as_str()),
            "url": issue.url,
        }),
    }
}

/// Narrowing applies to discussions too, so each runner only takes its own project's.
fn in_scope(config: &LinearConfig, issue: &NotifiedIssue) -> bool {
    let project_ok = config.projects.is_empty()
        || issue
            .project
            .as_ref()
            .is_some_and(|p| config.projects.contains(&p.name));
    let team_ok = config.teams.is_empty()
        || issue
            .team
            .as_ref()
            .is_some_and(|t| config.teams.contains(&t.key) || config.teams.contains(&t.name));
    let labels_ok = config.labels.is_empty()
        || issue
            .labels
            .as_ref()
            .is_some_and(|l| l.nodes.iter().any(|n| config.labels.contains(&n.name)));
    project_ok && team_ok && labels_ok
}

fn discussion_event(config: &LinearConfig, notification: Notification) -> Option<IncomingEvent> {
    let kind = notification.kind.as_str();
    let relevant =
        kind.contains("Mention") || (kind.contains("Comment") && !kind.contains("Reaction"));
    let issue = notification.issue?;
    let by_me = notification.actor.as_ref().is_some_and(|a| a.is_me)
        || notification
            .comment
            .as_ref()
            .and_then(|c| c.user.as_ref())
            .is_some_and(|u| u.is_me);
    let assigned_to_me = issue.assignee.as_ref().is_some_and(|a| a.is_me);
    if !relevant || by_me || assigned_to_me || !in_scope(config, &issue) {
        return None;
    }
    let sender = notification
        .comment
        .as_ref()
        .and_then(|c| c.user.as_ref())
        .or(notification.actor.as_ref())
        .and_then(Person::label);
    let (external_id, body, url) = match &notification.comment {
        Some(comment) => (
            comment.id.clone(),
            preview(&comment.body),
            comment.url.clone().unwrap_or_else(|| issue.url.clone()),
        ),
        None => (notification.id.clone(), String::new(), issue.url.clone()),
    };
    Some(IncomingEvent {
        source: SOURCE.into(),
        external_id,
        kind: EventKind::Discussion,
        mentions_me: kind.contains("Mention"),
        sender,
        occurred_at: notification.created_at,
        payload: json!({
            "ref": issue.identifier,
            "title": issue.title,
            "subtitle": notification.kind,
            "body": body,
            "url": url,
        }),
    })
}

pub struct LinearClient {
    http: reqwest::Client,
    url: String,
    key: String,
}

impl LinearClient {
    pub fn new(url: &str, key: String) -> Result<LinearClient, HttpError> {
        Ok(LinearClient {
            http: client()?,
            url: url.to_string(),
            key,
        })
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value, HttpError> {
        let response = self
            .http
            .post(&self.url)
            .header(reqwest::header::AUTHORIZATION, &self.key)
            .json(&json!({"query": query, "variables": variables}))
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if let Some(message) = body["errors"][0]["message"].as_str() {
            return Err(HttpError::Api(format!("linear: {message}")));
        }
        if !status.is_success() {
            return Err(HttpError::Status(status.as_u16()));
        }
        Ok(body["data"].clone())
    }

    async fn pages<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        field: &str,
        filter: Value,
    ) -> Result<Vec<T>, HttpError> {
        let mut nodes = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let data = self
                .graphql(query, json!({"filter": filter, "after": after}))
                .await?;
            let page: Page<T> = serde_json::from_value(data[field].clone())
                .map_err(|e| HttpError::Api(format!("linear {field}: {e}")))?;
            nodes.extend(page.nodes);
            match page.page_info {
                PageInfo {
                    has_next_page: true,
                    end_cursor: Some(cursor),
                } => after = Some(cursor),
                _ => break,
            }
        }
        Ok(nodes)
    }

    /// Work events: issues assigned to me (and narrowed by config) updated after `since`.
    pub async fn work_since(
        &self,
        config: &LinearConfig,
        since: DateTime<Utc>,
    ) -> Result<Fetched, HttpError> {
        let issues: Vec<Issue> = self
            .pages(WORK_QUERY, "issues", work_filter(config, since))
            .await?;
        Ok(Fetched {
            latest: issues.iter().map(|i| i.updated_at).max(),
            events: issues.iter().map(work_event).collect(),
        })
    }

    /// Discussion events from my notifications created after `since`.
    pub async fn discussion_since(
        &self,
        config: &LinearConfig,
        since: DateTime<Utc>,
    ) -> Result<Fetched, HttpError> {
        let filter = json!({"createdAt": {"gt": crate::store::iso(since)}});
        let notifications: Vec<Notification> = self
            .pages(DISCUSSION_QUERY, "notifications", filter)
            .await?;
        Ok(Fetched {
            latest: notifications.iter().map(|n| n.created_at).max(),
            events: notifications
                .into_iter()
                .filter_map(|n| discussion_event(config, n))
                .collect(),
        })
    }

    /// `Some(true)` when the issue is assigned to the key's owner; `None` when not found.
    pub async fn assigned_to_me(&self, card_ref: &str) -> Result<Option<bool>, HttpError> {
        let data = match self.graphql(ASSIGNEE_QUERY, json!({"id": card_ref})).await {
            Err(HttpError::Api(message)) if message.to_lowercase().contains("not found") => {
                return Ok(None);
            }
            other => other?,
        };
        let issue = &data["issue"];
        if issue.is_null() {
            return Ok(None);
        }
        Ok(Some(issue["assignee"]["isMe"].as_bool().unwrap_or(false)))
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::testing::now;

    fn narrowed() -> LinearConfig {
        LinearConfig {
            projects: vec!["Example App".into()],
            teams: vec!["EX".into()],
            labels: vec!["agent".into()],
            ..LinearConfig::default()
        }
    }

    #[test]
    fn work_filter_always_scopes_to_me() {
        let plain = work_filter(&LinearConfig::default(), now());
        assert_eq!(
            plain,
            json!({"assignee": {"isMe": {"eq": true}}, "updatedAt": {"gt": "2026-01-15T09:30:00.000Z"}})
        );
        let filter = work_filter(&narrowed(), now());
        assert_eq!(filter["assignee"], json!({"isMe": {"eq": true}}));
        assert_eq!(filter["project"], json!({"name": {"in": ["Example App"]}}));
        assert_eq!(
            filter["labels"],
            json!({"some": {"name": {"in": ["agent"]}}})
        );
        assert_eq!(filter["team"]["or"][0], json!({"key": {"in": ["EX"]}}));
    }

    #[test]
    fn config_values_cannot_widen_the_scope() {
        // Values that look like filter syntax stay string members of an `in` list.
        let hostile = LinearConfig {
            projects: vec!["\"}, \"assignee\": {\"null\": true".into()],
            teams: vec!["or".into()],
            labels: vec!["assignee".into()],
            ..LinearConfig::default()
        };
        let filter = work_filter(&hostile, now());
        assert_eq!(filter["assignee"], json!({"isMe": {"eq": true}}));
        let keys: Vec<&String> = filter.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["assignee", "labels", "project", "team", "updatedAt"]);
        assert_eq!(filter["project"]["name"]["in"][0], hostile.projects[0]);
    }

    fn page(nodes: Value) -> Value {
        json!({"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}})
    }

    async fn server_with(data: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "lin_api_example"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": data})))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn fetches_work_with_the_personal_filter() {
        let issue = json!({"id": "uuid-1", "identifier": "EX-1", "title": "Example task",
            "url": "https://linear.app/example/issue/EX-1", "updatedAt": "2026-01-15T10:00:00.000Z",
            "state": {"name": "Todo"}});
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"isMe\":{\"eq\":true}"))
            .and(body_string_contains("HarnessWork"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": {"issues": page(json!([issue]))}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = LinearClient::new(&server.uri(), "lin_api_example".into()).unwrap();
        let fetched = client
            .work_since(&LinearConfig::default(), now())
            .await
            .unwrap();
        assert_eq!(
            fetched.latest.unwrap().to_rfc3339(),
            "2026-01-15T10:00:00+00:00"
        );
        let event = &fetched.events[0];
        assert_eq!(event.external_id, "uuid-1@2026-01-15T10:00:00.000Z");
        assert_eq!(event.kind, EventKind::Work);
        assert_eq!(event.payload["ref"], "EX-1");
        assert_eq!(event.payload["subtitle"], "Todo");
    }

    #[tokio::test]
    async fn follows_pages() {
        let server = MockServer::start().await;
        let issue = |n: u32| {
            json!({"id": format!("uuid-{n}"), "identifier": format!("EX-{n}"), "title": "t",
            "url": "https://linear.app/example", "updatedAt": "2026-01-15T10:00:00.000Z"})
        };
        Mock::given(method("POST"))
            .and(body_string_contains("\"after\":null"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"issues":
                {"nodes": [issue(1)], "pageInfo": {"hasNextPage": true, "endCursor": "c1"}}}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"after\":\"c1\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": {"issues": page(json!([issue(2)]))}})),
            )
            .mount(&server)
            .await;
        let client = LinearClient::new(&server.uri(), "k".into()).unwrap();
        let fetched = client
            .work_since(&LinearConfig::default(), now())
            .await
            .unwrap();
        let refs: Vec<&str> = fetched
            .events
            .iter()
            .map(|e| e.payload["ref"].as_str().unwrap())
            .collect();
        assert_eq!(refs, ["EX-1", "EX-2"]);
    }

    fn notification(
        id: &str,
        kind: &str,
        comment_by_me: bool,
        assigned_to_me: bool,
        project: &str,
    ) -> Value {
        json!({
            "id": id, "type": kind, "createdAt": "2026-01-15T10:00:00.000Z",
            "actor": {"name": "Example Person", "email": "person@example.com", "isMe": false},
            "issue": {"identifier": "EX-9", "title": "Example discussion", "url": "https://linear.app/example/issue/EX-9",
                      "assignee": {"isMe": assigned_to_me}, "project": {"name": project},
                      "team": {"key": "EX", "name": "Example"}, "labels": {"nodes": []}},
            "comment": {"id": format!("comment-{id}"), "body": "What do you think?", "url": format!("https://linear.app/example/issue/EX-9#comment-{id}"),
                        "user": {"name": "Example Person", "isMe": comment_by_me}}
        })
    }

    #[tokio::test]
    async fn discussion_events_per_comment_excluding_mine_and_assigned() {
        let nodes = json!([
            notification("n1", "issueCommentMention", false, false, "Example App"),
            notification("n2", "issueNewComment", false, false, "Example App"),
            notification("n3", "issueNewComment", true, false, "Example App"),
            notification("n4", "issueNewComment", false, true, "Example App"),
            notification("n5", "issueCommentReaction", false, false, "Example App"),
            notification("n6", "issueNewComment", false, false, "Other Project"),
            {"id": "n7", "type": "projectUpdateCreated", "createdAt": "2026-01-15T10:00:00.000Z", "actor": null},
        ]);
        let server = server_with(json!({"notifications": page(nodes)})).await;
        let client = LinearClient::new(&server.uri(), "lin_api_example".into()).unwrap();
        let config = LinearConfig {
            projects: vec!["Example App".into()],
            ..LinearConfig::default()
        };
        let fetched = client.discussion_since(&config, now()).await.unwrap();
        assert!(
            fetched.latest.is_some(),
            "filtered notifications still move the cursor"
        );
        let events = fetched.events;
        let ids: Vec<&str> = events.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(ids, ["comment-n1", "comment-n2"]);
        assert!(events[0].mentions_me && !events[1].mentions_me);
        assert_eq!(events[0].kind, EventKind::Discussion);
        assert_eq!(events[0].sender.as_deref(), Some("Example Person"));
        assert_eq!(
            events[0].payload["url"],
            "https://linear.app/example/issue/EX-9#comment-n1"
        );
    }

    #[tokio::test]
    async fn assignee_check() {
        let mine =
            server_with(json!({"issue": {"identifier": "EX-1", "assignee": {"isMe": true}}})).await;
        let client = LinearClient::new(&mine.uri(), "lin_api_example".into()).unwrap();
        assert_eq!(client.assigned_to_me("EX-1").await.unwrap(), Some(true));

        let unassigned =
            server_with(json!({"issue": {"identifier": "EX-1", "assignee": null}})).await;
        let client = LinearClient::new(&unassigned.uri(), "lin_api_example".into()).unwrap();
        assert_eq!(client.assigned_to_me("EX-1").await.unwrap(), Some(false));

        let missing = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"data": null, "errors": [{"message": "Entity not found: Issue"}]}),
            ))
            .mount(&missing)
            .await;
        let client = LinearClient::new(&missing.uri(), "k".into()).unwrap();
        assert_eq!(client.assigned_to_me("EX-404").await.unwrap(), None);
    }

    #[tokio::test]
    async fn errors_surface() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        let client = LinearClient::new(&server.uri(), "bad".into()).unwrap();
        let error = client
            .work_since(&LinearConfig::default(), now())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "HTTP 401");
    }
}
