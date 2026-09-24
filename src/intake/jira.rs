//! Jira Cloud REST v3 with Basic auth (email + API token), via the enhanced search
//! endpoint `POST /rest/api/3/search/jql` (the old `/search` is removed).
//!
//! - work: `assignee = currentUser() AND (<jql>)`; `jql` only narrows.
//! - discussion: `watcher = currentUser() AND (assignee != currentUser() OR assignee is
//!   EMPTY) AND (<jql>)`; one event per comment newer than the cursor, excluding mine.
//!
//! JQL dates are minute-resolution and in the user's time zone, so the window is relative
//! (`updated >= -<n>m`, timezone-free) and exact filtering happens on the returned
//! timestamps; dedup by external id absorbs the overlap.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::event::{EventKind, Fetched, IncomingEvent, preview};
use super::http::{HttpError, client};

pub const SOURCE: &str = "jira";
const PAGE_SIZE: u32 = 50;
const MAX_PAGES: usize = 10;

/// `jql` narrowing must stay inside its parentheses: balanced, never closing below depth 0
/// outside quotes, no `ORDER BY`, and no `\` outside quotes (a JQL escape there would let a
/// quote or parenthesis mean something other than what this scanner sees). That is what
/// makes `AND (<jql>)` unable to widen.
pub fn validate_narrowing(jql: &str) -> Result<(), String> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut outside = String::new();
    for c in jql.chars() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == open {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '\\' => return Err("'\\' outside a quoted string is not allowed".into()),
            '(' => depth += 1,
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or("unbalanced ')' would escape the personal scope")?;
            }
            _ => {}
        }
        outside.push(c.to_ascii_lowercase());
    }
    if quote.is_some() {
        return Err("unterminated string".into());
    }
    if depth != 0 {
        return Err("unbalanced '('".into());
    }
    let words: Vec<&str> = outside.split_whitespace().collect();
    if words.windows(2).any(|w| w == ["order", "by"]) {
        return Err("ORDER BY is not allowed; the runner orders results".into());
    }
    Ok(())
}

fn narrowed(scope: &str, jql: &str) -> String {
    if jql.trim().is_empty() {
        scope.to_string()
    } else {
        format!("{scope} AND ({jql})")
    }
}

/// Minutes to look back to cover `since`, plus one for JQL's minute resolution.
fn window_minutes(since: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    (now - since).num_minutes().max(0) + 2
}

#[must_use]
pub fn work_jql(jql: &str, since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let scope = narrowed("assignee = currentUser()", jql);
    format!(
        "{scope} AND updated >= -{}m ORDER BY updated ASC",
        window_minutes(since, now)
    )
}

#[must_use]
pub fn discussion_jql(jql: &str, since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let scope = narrowed(
        "watcher = currentUser() AND (assignee != currentUser() OR assignee is EMPTY)",
        jql,
    );
    format!(
        "{scope} AND updated >= -{}m ORDER BY updated ASC",
        window_minutes(since, now)
    )
}

/// Jira timestamps look like `2026-01-15T09:30:00.000+0000`.
pub fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f%z")
        .or_else(|_| DateTime::parse_from_rfc3339(text))
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Plain text of an Atlassian Document Format body.
fn adf_text(node: &Value, out: &mut String) {
    if let Some(text) = node["text"].as_str() {
        out.push_str(text);
    }
    if node["type"] == "mention"
        && let Some(label) = node["attrs"]["text"].as_str()
    {
        out.push_str(label);
    }
    if let Some(children) = node["content"].as_array() {
        for child in children {
            adf_text(child, out);
        }
        if node["type"] == "paragraph" {
            out.push('\n');
        }
    }
}

/// An ADF mention node pointing at `account_id`.
fn mentions(node: &Value, account_id: &str) -> bool {
    (node["type"] == "mention" && node["attrs"]["id"] == account_id)
        || node["content"]
            .as_array()
            .is_some_and(|children| children.iter().any(|c| mentions(c, account_id)))
}

fn latest_updated(issues: &[Value]) -> Option<DateTime<Utc>> {
    issues
        .iter()
        .filter_map(|issue| issue["fields"]["updated"].as_str().and_then(parse_time))
        .max()
}

pub struct JiraClient {
    http: reqwest::Client,
    base_url: String,
    email: String,
    token: String,
}

impl JiraClient {
    pub fn new(base_url: &str, email: String, token: String) -> Result<JiraClient, HttpError> {
        Ok(JiraClient {
            http: client()?,
            base_url: base_url.trim_end_matches('/').to_string(),
            email,
            token,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base_url))
            .basic_auth(&self.email, Some(&self.token))
            .header(reqwest::header::ACCEPT, "application/json")
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Option<Value>, HttpError> {
        let response = request.send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(HttpError::Status(status.as_u16()));
        }
        Ok(Some(response.json().await?))
    }

    /// The key owner's `accountId`.
    pub async fn myself(&self) -> Result<String, HttpError> {
        let body = self
            .send(self.request(reqwest::Method::GET, "/rest/api/3/myself"))
            .await?
            .unwrap_or(Value::Null);
        body["accountId"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| HttpError::Api("jira: /myself has no accountId".into()))
    }

    async fn search(&self, jql: &str, fields: &[&str]) -> Result<Vec<Value>, HttpError> {
        let mut issues = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let body = json!({"jql": jql, "fields": fields, "maxResults": PAGE_SIZE, "nextPageToken": token});
            let request = self
                .request(reqwest::Method::POST, "/rest/api/3/search/jql")
                .json(&body);
            let page = self.send(request).await?.ok_or(HttpError::Status(404))?;
            if let Some(found) = page["issues"].as_array() {
                issues.extend(found.iter().cloned());
            }
            match page["nextPageToken"].as_str() {
                Some(next) if page["isLast"] != true => token = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(issues)
    }

    fn browse_url(&self, key: &str) -> String {
        format!("{}/browse/{key}", self.base_url)
    }

    /// Work events for issues assigned to me updated after `since`.
    pub async fn work_since(
        &self,
        jql: &str,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Fetched, HttpError> {
        let issues = self
            .search(
                &work_jql(jql, since, now),
                &["summary", "status", "updated"],
            )
            .await?;
        let events = issues
            .iter()
            .filter_map(|issue| {
                let key = issue["key"].as_str()?;
                let updated_text = issue["fields"]["updated"].as_str()?;
                let updated = parse_time(updated_text)?;
                (updated > since).then(|| IncomingEvent {
                    source: SOURCE.into(),
                    external_id: format!("{key}@{updated_text}"),
                    kind: EventKind::Work,
                    mentions_me: false,
                    sender: None,
                    occurred_at: updated,
                    payload: json!({
                        "ref": key,
                        "title": issue["fields"]["summary"],
                        "subtitle": issue["fields"]["status"]["name"],
                        "url": self.browse_url(key),
                    }),
                })
            })
            .collect();
        Ok(Fetched {
            events,
            latest: latest_updated(&issues),
        })
    }

    /// Discussion events: comments newer than `since` on issues I watch but am not
    /// assigned, excluding my own comments.
    pub async fn discussion_since(
        &self,
        jql: &str,
        me: &str,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Fetched, HttpError> {
        let issues = self
            .search(
                &discussion_jql(jql, since, now),
                &["summary", "comment", "updated"],
            )
            .await?;
        let mut events = Vec::new();
        for issue in &issues {
            let Some(key) = issue["key"].as_str() else {
                continue;
            };
            let comments = issue["fields"]["comment"]["comments"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            for comment in comments {
                let author = &comment["author"];
                let created = comment["created"].as_str().and_then(parse_time);
                let (Some(id), Some(created)) = (comment["id"].as_str(), created) else {
                    continue;
                };
                if created <= since || author["accountId"] == me {
                    continue;
                }
                let mut text = String::new();
                adf_text(&comment["body"], &mut text);
                let sender = author["displayName"]
                    .as_str()
                    .or(author["emailAddress"].as_str())
                    .map(str::to_string);
                events.push(IncomingEvent {
                    source: SOURCE.into(),
                    external_id: format!("{key}#{id}"),
                    kind: EventKind::Discussion,
                    mentions_me: mentions(&comment["body"], me),
                    sender,
                    occurred_at: created,
                    payload: json!({
                        "ref": key,
                        "title": issue["fields"]["summary"],
                        "body": preview(text.trim()),
                        "url": format!("{}?focusedCommentId={id}", self.browse_url(key)),
                    }),
                });
            }
        }
        Ok(Fetched {
            events,
            latest: latest_updated(&issues),
        })
    }

    /// `Some(true)` when the issue's assignee is the key's owner; `None` when not found.
    pub async fn assigned_to_me(&self, key: &str, me: &str) -> Result<Option<bool>, HttpError> {
        let valid_key = !key.is_empty()
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid_key {
            return Ok(None);
        }
        let path = format!("/rest/api/3/issue/{key}?fields=assignee");
        let issue = self.send(self.request(reqwest::Method::GET, &path)).await?;
        Ok(issue.map(|issue| issue["fields"]["assignee"]["accountId"] == me))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::testing::now;

    const BASIC: &str = "Basic ZGV2QGV4YW1wbGUuY29tOnRva2Vu"; // dev@example.com:token

    fn client(server: &MockServer) -> JiraClient {
        JiraClient::new(&server.uri(), "dev@example.com".into(), "token".into()).unwrap()
    }

    #[test]
    fn narrowing_cannot_widen_the_scope() {
        for ok in [
            "",
            "project = EX",
            "project = EX AND (labels = agent OR labels = bot)",
            "summary ~ \"a ) OR (b\"",
        ] {
            assert!(validate_narrowing(ok).is_ok(), "{ok}");
        }
        for bad in [
            "project = EX) OR (project = OTHER",
            ") OR assignee is EMPTY OR (",
            "project = EX ORDER BY created",
            "(project = EX",
            "summary ~ \"open",
            r#"summary ~ \"x) OR (summary ~ "y\"" OR project = OTHER"#,
            r"summary ~ a\)b",
        ] {
            assert!(validate_narrowing(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn queries_always_carry_the_personal_scope() {
        let since = now() - TimeDelta::minutes(10);
        assert_eq!(
            work_jql("project = EX", since, now()),
            "assignee = currentUser() AND (project = EX) AND updated >= -12m ORDER BY updated ASC"
        );
        assert_eq!(
            work_jql("", since, now()),
            "assignee = currentUser() AND updated >= -12m ORDER BY updated ASC"
        );
        assert!(discussion_jql("project = EX", since, now()).starts_with(
            "watcher = currentUser() AND (assignee != currentUser() OR assignee is EMPTY) AND (project = EX) AND"
        ));
    }

    #[test]
    fn parses_jira_timestamps() {
        let parsed = parse_time("2026-01-15T11:30:00.000+0200").unwrap();
        assert_eq!(parsed, now());
        assert!(parse_time("15/01/2026").is_none());
    }

    fn issue(key: &str, updated: &str) -> Value {
        json!({"key": key, "fields": {"summary": "Example task", "status": {"name": "To Do"}, "updated": updated}})
    }

    #[tokio::test]
    async fn work_uses_the_enhanced_search_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/search/jql"))
            .and(header("authorization", BASIC))
            .and(body_partial_json(json!({"fields": ["summary", "status", "updated"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [issue("EX-1", "2026-01-15T09:31:00.000+0000"), issue("EX-2", "2026-01-15T09:29:00.000+0000")],
                "isLast": true
            })))
            .expect(1)
            .mount(&server)
            .await;
        let fetched = client(&server)
            .work_since("project = EX", now(), now())
            .await
            .unwrap();
        assert_eq!(
            fetched.latest.unwrap().to_rfc3339(),
            "2026-01-15T09:31:00+00:00"
        );
        let events = fetched.events;
        assert_eq!(events.len(), 1, "older than the cursor: filtered");
        assert_eq!(events[0].external_id, "EX-1@2026-01-15T09:31:00.000+0000");
        assert_eq!(
            events[0].payload["url"],
            format!("{}/browse/EX-1", server.uri())
        );
        let requests = server.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            sent["jql"]
                .as_str()
                .unwrap()
                .starts_with("assignee = currentUser() AND (project = EX)")
        );
    }

    #[tokio::test]
    async fn follows_next_page_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"nextPageToken": null})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [issue("EX-1", "2026-01-15T09:31:00.000+0000")], "nextPageToken": "t2", "isLast": false})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"nextPageToken": "t2"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [issue("EX-2", "2026-01-15T09:32:00.000+0000")], "isLast": true})))
            .mount(&server)
            .await;
        let fetched = client(&server).work_since("", now(), now()).await.unwrap();
        assert_eq!(fetched.events.len(), 2);
    }

    fn comment(id: &str, account: &str, created: &str, body: Value) -> Value {
        json!({"id": id, "author": {"accountId": account, "displayName": format!("Person {account}")},
               "created": created, "body": body})
    }

    fn adf(content: Value) -> Value {
        json!({"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": content}]})
    }

    #[tokio::test]
    async fn discussion_events_per_comment() {
        let server = MockServer::start().await;
        let comments = json!({"comments": [
            comment("10", "acc-other", "2026-01-15T09:20:00.000+0000", adf(json!([{"type": "text", "text": "old"}]))),
            comment("11", "acc-other", "2026-01-15T09:40:00.000+0000", adf(json!([
                {"type": "mention", "attrs": {"id": "acc-me", "text": "@Example User"}},
                {"type": "text", "text": " can you check?"}]))),
            comment("12", "acc-me", "2026-01-15T09:41:00.000+0000", adf(json!([{"type": "text", "text": "mine"}]))),
            comment("13", "acc-other", "2026-01-15T09:42:00.000+0000", adf(json!([{"type": "text", "text": "follow-up"}]))),
        ], "maxResults": 4, "startAt": 0, "total": 4});
        Mock::given(method("POST"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [{"key": "EX-7", "fields": {"summary": "Shared ticket", "comment": comments,
                            "updated": "2026-01-15T09:42:00.000+0000"}}], "isLast": true})))
            .mount(&server)
            .await;
        let events = client(&server)
            .discussion_since("", "acc-me", now(), now())
            .await
            .unwrap()
            .events;
        let ids: Vec<&str> = events.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            ["EX-7#11", "EX-7#13"],
            "one per comment, newer than cursor, not mine"
        );
        assert!(events[0].mentions_me && !events[1].mentions_me);
        assert_eq!(events[0].payload["body"], "@Example User can you check?");
        assert_eq!(events[0].sender.as_deref(), Some("Person acc-other"));
        assert_eq!(events[0].kind, EventKind::Discussion);
    }

    #[tokio::test]
    async fn myself_and_assignee_check() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/myself"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accountId": "acc-me"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/EX-1"))
            .and(query_param("fields", "assignee"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"key": "EX-1", "fields": {"assignee": {"accountId": "acc-me"}}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/EX-2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"key": "EX-2", "fields": {"assignee": null}})),
            )
            .mount(&server)
            .await;
        let jira = client(&server);
        let me = jira.myself().await.unwrap();
        assert_eq!(me, "acc-me");
        assert_eq!(jira.assigned_to_me("EX-1", &me).await.unwrap(), Some(true));
        assert_eq!(jira.assigned_to_me("EX-2", &me).await.unwrap(), Some(false));
        assert_eq!(jira.assigned_to_me("EX-3", &me).await.unwrap(), None, "404");
        assert_eq!(jira.assigned_to_me("../x", &me).await.unwrap(), None);
    }

    #[tokio::test]
    async fn http_errors_surface() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;
        let error = client(&server)
            .work_since("", now(), now())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "HTTP 410");
    }
}
