use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::config::{Config, SourcesConfig, Workspace};
use crate::paths::Paths;

pub const WORKFLOW_COMMAND: &str = "/claude-harness:workflow";

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
struct HeartbeatContext<'a> {
    now: String,
    cursors: &'a BTreeMap<String, String>,
    sources: &'a SourcesConfig,
    workspaces: Vec<WorkspaceContext<'a>>,
    outreach_file: String,
}

#[derive(Serialize)]
struct CardContext<'a> {
    now: String,
    #[serde(rename = "ref")]
    card_ref: &'a str,
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

pub fn heartbeat_prompt(
    config: &Config,
    paths: &Paths,
    cursors: &BTreeMap<String, String>,
    now: DateTime<Utc>,
) -> String {
    let context = HeartbeatContext {
        now: iso(now),
        cursors,
        sources: &config.sources,
        workspaces: all_workspaces(config),
        outreach_file: paths.outreach_file.display().to_string(),
    };
    render(&format!("{WORKFLOW_COMMAND} heartbeat"), &context)
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
        card_ref,
        workspace: workspace.map(|(w, checkout)| WorkspaceContext::of(w, checkout)),
        workspaces: all_workspaces(config),
        outreach_file: paths.outreach_file.display().to_string(),
    };
    render(&format!("{WORKFLOW_COMMAND} card {card_ref}"), &context)
}

/// The JSON context block of a rendered prompt (tests and debugging).
pub fn context_of(prompt: &str) -> serde_json::Value {
    let body = prompt.split_once("```json\n").map_or("", |(_, rest)| rest);
    let body = body.rsplit_once("```").map_or(body, |(json, _)| json);
    serde_json::from_str(body).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{now, test_env};

    #[test]
    fn heartbeat_prompt_carries_context() {
        let env = test_env();
        let cursors = BTreeMap::from([("tracker:linear".to_string(), "old".to_string())]);
        let prompt = heartbeat_prompt(&env.config, &env.paths, &cursors, now());
        assert!(prompt.starts_with("/claude-harness:workflow heartbeat\n\n```json\n"));
        let context = context_of(&prompt);
        assert_eq!(context["now"], "2026-01-15T09:30:00+00:00");
        assert_eq!(context["cursors"]["tracker:linear"], "old");
        assert_eq!(context["sources"]["slack_channels"][0], "C0000000001");
        assert_eq!(context["workspaces"][0]["name"], "example-app");
        assert_eq!(context["workspaces"][0]["match"][0], "EX-");
        assert_eq!(
            context["outreach_file"],
            env.paths.outreach_file.display().to_string()
        );
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
        assert_eq!(context["workspace"]["name"], "example-app");
        assert_eq!(context["workspace"]["path"], checkout.display().to_string());
        assert_eq!(
            context["workspaces"][0]["path"],
            workspace.path.display().to_string()
        );
        let unmatched = card_prompt("ZZ-1", None, &env.config, &env.paths, now());
        assert!(context_of(&unmatched)["workspace"].is_null());
    }
}
