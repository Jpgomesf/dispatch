use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::config::{Config, SourcesConfig, Workspace};
use crate::intake::Event;
use crate::paths::Paths;
use crate::results::DiscussionToRun;

pub const WORKFLOW_COMMAND: &str = "/claude-harness:workflow";
/// Cursor keys the runner's own pollers use; never shown to the skill.
pub const INTAKE_CURSOR_PREFIX: &str = "intake:";

#[derive(Serialize)]
struct WorkspaceContext<'a> {
    name: &'a str,
    path: String,
    #[serde(rename = "match")]
    match_: &'a [String],
}

impl<'a> WorkspaceContext<'a> {
    fn of(workspace: &'a Workspace, path: &Path) -> Self {
        WorkspaceContext {
            name: &workspace.name,
            path: path.display().to_string(),
            match_: &workspace.match_,
        }
    }
}

#[derive(Serialize)]
struct TriageContext<'a> {
    now: String,
    runner: &'a str,
    cursors: BTreeMap<&'a str, &'a str>,
    sources: &'a SourcesConfig,
    workspaces: Vec<WorkspaceContext<'a>>,
    outreach_file: String,
    /// Empty: a fallback sweep.
    events: &'a [Event],
}

#[derive(Serialize)]
struct CardContext<'a> {
    now: String,
    runner: &'a str,
    #[serde(rename = "ref")]
    card_ref: &'a str,
    workspace: Option<WorkspaceContext<'a>>,
    workspaces: Vec<WorkspaceContext<'a>>,
    outreach_file: String,
}

#[derive(Serialize)]
struct DiscussionContext<'a> {
    now: String,
    runner: &'a str,
    #[serde(rename = "ref")]
    discussion_ref: &'a str,
    thread: &'a str,
    question: &'a str,
    workspace: Option<WorkspaceContext<'a>>,
    workspaces: Vec<WorkspaceContext<'a>>,
    outreach_file: String,
}

fn render(invocation: &str, context: &impl Serialize) -> String {
    let json = serde_json::to_string_pretty(context).expect("context serializes");
    format!("{invocation}\n\n```json\n{json}\n```\n")
}

fn iso(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::AutoSi, false)
}

fn all_workspaces(config: &Config) -> Vec<WorkspaceContext<'_>> {
    config
        .workspaces
        .iter()
        .map(|w| WorkspaceContext::of(w, &w.path))
        .collect()
}

pub fn triage_prompt(
    config: &Config,
    paths: &Paths,
    cursors: &BTreeMap<String, String>,
    events: &[Event],
    now: DateTime<Utc>,
) -> String {
    let skill_cursors = cursors
        .iter()
        .filter(|(key, _)| !key.starts_with(INTAKE_CURSOR_PREFIX))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let context = TriageContext {
        now: iso(now),
        runner: &config.name,
        cursors: skill_cursors,
        sources: &config.sources,
        workspaces: all_workspaces(config),
        outreach_file: paths.outreach_file.display().to_string(),
        events,
    };
    render(&format!("{WORKFLOW_COMMAND} triage"), &context)
}

/// `checkout` is where this card runs: the card's own worktree, or the workspace path itself.
pub fn card_prompt(
    card_ref: &str,
    workspace: Option<(&Workspace, &Path)>,
    config: &Config,
    paths: &Paths,
    now: DateTime<Utc>,
) -> String {
    let context = CardContext {
        now: iso(now),
        runner: &config.name,
        card_ref,
        workspace: workspace.map(|(w, checkout)| WorkspaceContext::of(w, checkout)),
        workspaces: all_workspaces(config),
        outreach_file: paths.outreach_file.display().to_string(),
    };
    render(&format!("{WORKFLOW_COMMAND} card {card_ref}"), &context)
}

/// `checkout` is the discussion's detached worktree, or the workspace path itself.
pub fn discussion_prompt(
    discussion: &DiscussionToRun,
    workspace: Option<(&Workspace, &Path)>,
    config: &Config,
    paths: &Paths,
    now: DateTime<Utc>,
) -> String {
    let context = DiscussionContext {
        now: iso(now),
        runner: &config.name,
        discussion_ref: &discussion.discussion_ref,
        thread: &discussion.thread,
        question: &discussion.question,
        workspace: workspace.map(|(w, checkout)| WorkspaceContext::of(w, checkout)),
        workspaces: all_workspaces(config),
        outreach_file: paths.outreach_file.display().to_string(),
    };
    let invocation = format!(
        "{WORKFLOW_COMMAND} discussion {}",
        discussion.discussion_ref
    );
    render(&invocation, &context)
}

/// The JSON context block of a rendered prompt (tests and debugging).
pub fn context_of(prompt: &str) -> serde_json::Value {
    let body = prompt.split_once("```json\n").map_or("", |(_, rest)| rest);
    let body = body.rsplit_once("```").map_or(body, |(json, _)| json);
    serde_json::from_str(body).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::intake::EventKind;
    use crate::testing::{now, test_env};

    #[test]
    fn triage_prompt_carries_context_and_events() {
        let env = test_env();
        let cursors = BTreeMap::from([
            ("tracker:linear".to_string(), "old".to_string()),
            ("intake:linear:work".to_string(), "hidden".to_string()),
        ]);
        let events = vec![Event {
            id: 7,
            source: "manual".into(),
            kind: EventKind::Message,
            mentions_me: true,
            sender: None,
            occurred_at: now(),
            payload: json!({"body": "test event"}),
        }];
        let prompt = triage_prompt(&env.config, &env.paths, &cursors, &events, now());
        assert!(prompt.starts_with("/claude-harness:workflow triage\n\n```json\n"));
        let context = context_of(&prompt);
        assert_eq!(context["now"], "2026-01-15T09:30:00+00:00");
        assert_eq!(context["runner"], "example-app");
        assert_eq!(context["cursors"], json!({"tracker:linear": "old"}));
        assert_eq!(context["sources"]["slack_channels"][0], "C0000000001");
        assert_eq!(context["workspaces"][0]["name"], "example-app");
        assert_eq!(context["workspaces"][0]["match"][0], "EX-");
        assert_eq!(
            context["outreach_file"],
            env.paths.outreach_file.display().to_string()
        );
        assert_eq!(
            context["events"][0],
            json!({"id": "7", "source": "manual", "kind": "message", "mentions_me": true,
                   "sender": null, "occurred_at": "2026-01-15T09:30:00Z", "payload": {"body": "test event"}})
        );
        let sweep = context_of(&triage_prompt(
            &env.config,
            &env.paths,
            &cursors,
            &[],
            now(),
        ));
        assert_eq!(sweep["events"], json!([]));
    }

    #[test]
    fn card_prompt_carries_checkout_and_all_workspaces() {
        let env = test_env();
        let workspace = &env.config.workspaces[0];
        let checkout = Path::new("/tmp/example/worktrees/example-app/ex-3");
        let prompt = card_prompt(
            "EX-3",
            Some((workspace, checkout)),
            &env.config,
            &env.paths,
            now(),
        );
        assert!(prompt.starts_with("/claude-harness:workflow card EX-3\n"));
        let context = context_of(&prompt);
        assert_eq!(context["ref"], "EX-3");
        assert_eq!(context["runner"], "example-app");
        assert_eq!(context["workspace"]["name"], "example-app");
        assert_eq!(context["workspace"]["path"], checkout.display().to_string());
        assert_eq!(
            context["workspaces"][0]["path"],
            workspace.path.display().to_string()
        );
        let unmatched = card_prompt("ZZ-1", None, &env.config, &env.paths, now());
        assert!(context_of(&unmatched)["workspace"].is_null());
    }

    #[test]
    fn discussion_prompt_carries_the_question() {
        let env = test_env();
        let discussion = DiscussionToRun {
            discussion_ref: "EX-9".into(),
            thread: "https://example.com/EX-9/c1".into(),
            question: "Why does the example fail?".into(),
        };
        let prompt = discussion_prompt(&discussion, None, &env.config, &env.paths, now());
        assert!(prompt.starts_with("/claude-harness:workflow discussion EX-9\n"));
        let context = context_of(&prompt);
        let keys: Vec<&String> = context.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "now",
                "outreach_file",
                "question",
                "ref",
                "runner",
                "thread",
                "workspace",
                "workspaces"
            ]
        );
        assert_eq!(context["thread"], "https://example.com/EX-9/c1");
        assert_eq!(context["question"], "Why does the example fail?");
        assert!(context["workspace"].is_null());
    }
}
