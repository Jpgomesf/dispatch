use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::config::{Config, SourcesConfig, Workspace};
use crate::intake::Event;
use crate::paths::Paths;
use crate::results::DiscussionToRun;
use crate::store::Attempt;

/// How many earlier attempts a card session sees.
pub const PREVIOUS_ATTEMPTS: i64 = 3;

/// Cursor keys the runner's own pollers use; never shown to the skill.
pub const INTAKE_CURSOR_PREFIX: &str = "intake:";

/// Appended to Claude Code's system prompt in every session (`--append-system-prompt`). Fixed
/// and short on purpose: how the work gets done is Claude's call, with the user's own skills.
pub const RUNNER_RULES: &str = "You are running under dispatch, unattended. These are its \
only non-negotiable rules; everything else is yours to decide with the user's skills and \
settings.
- Return your result as the JSON the output schema requires.
- Discussion sessions never create branches, commit, push or open pull requests.
- Only work cards assigned to the user.
- Nobody can answer questions during this session: decide, or report the work blocked.";

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

/// A card waiting for a person (`needs_human`): shown to triage so it can escalate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Escalation {
    #[serde(rename = "ref")]
    pub card_ref: String,
    /// `max_attempts`, `no_progress`, `failed` or `environment`.
    pub reason: String,
    /// Counted attempts in the run that ended here.
    pub attempts: u32,
    pub last_summary: Option<String>,
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
    escalations: &'a [Escalation],
}

/// An earlier attempt at the card, so a fresh session starts from what was learned rather
/// than from a resumed transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviousAttempt {
    pub attempt: u32,
    pub outcome: String,
    pub summary: Option<String>,
    pub blocked_on: Option<String>,
    pub session_id: Option<String>,
    /// Commits it added to the worktree's HEAD; `null` when the card has no worktree.
    pub new_commits: Option<u32>,
}

impl From<&Attempt> for PreviousAttempt {
    fn from(attempt: &Attempt) -> Self {
        PreviousAttempt {
            attempt: attempt.attempt,
            outcome: attempt.outcome.clone().unwrap_or_default(),
            summary: attempt.summary.clone(),
            blocked_on: attempt.blocked_on.clone(),
            session_id: attempt.session_id.clone(),
            new_commits: attempt.new_commits,
        }
    }
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
    /// The last few attempts, oldest first.
    previous_attempts: &'a [PreviousAttempt],
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

/// The objective, a blank line, then the JSON context block.
fn render(objective: &str, context: &impl Serialize) -> String {
    let json = serde_json::to_string_pretty(context).expect("context serializes");
    format!("{}\n\n```json\n{json}\n```\n", objective.trim())
}

/// A configured objective with `{ref}` filled in.
fn objective_for(template: &str, reference: &str) -> String {
    template.replace("{ref}", reference)
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
    escalations: &[Escalation],
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
        escalations,
    };
    render(&config.triage.objective, &context)
}

/// `checkout` is where this card runs: the card's own worktree, or the workspace path itself.
pub fn card_prompt(
    card_ref: &str,
    workspace: Option<(&Workspace, &Path)>,
    previous_attempts: &[PreviousAttempt],
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
        previous_attempts,
    };
    render(&objective_for(&config.card.objective, card_ref), &context)
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
    let objective = objective_for(&config.discussion.objective, &discussion.discussion_ref);
    render(&objective, &context)
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
        let escalations = vec![Escalation {
            card_ref: "EX-4".into(),
            reason: "max_attempts".into(),
            attempts: 3,
            last_summary: Some("no result within 3h".into()),
        }];
        let prompt = triage_prompt(
            &env.config,
            &env.paths,
            &cursors,
            &events,
            &escalations,
            now(),
        );
        let expected_start = format!("{}\n\n```json\n", crate::config::TRIAGE_OBJECTIVE);
        assert!(prompt.starts_with(&expected_start), "{prompt}");
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
        assert_eq!(
            context["escalations"],
            json!([{"ref": "EX-4", "reason": "max_attempts", "attempts": 3,
                    "last_summary": "no result within 3h"}])
        );
        let sweep = context_of(&triage_prompt(
            &env.config,
            &env.paths,
            &cursors,
            &[],
            &[],
            now(),
        ));
        assert_eq!(sweep["events"], json!([]));
        assert_eq!(sweep["escalations"], json!([]));
    }

    #[test]
    fn card_prompt_carries_checkout_and_all_workspaces() {
        let env = test_env();
        let workspace = &env.config.workspaces[0];
        let checkout = Path::new("/tmp/example/worktrees/example-app/ex-3");
        let previous = [PreviousAttempt {
            attempt: 1,
            outcome: "timeout".into(),
            summary: Some("no result within 3h".into()),
            blocked_on: None,
            session_id: Some("session-1".into()),
            new_commits: Some(2),
        }];
        let prompt = card_prompt(
            "EX-3",
            Some((workspace, checkout)),
            &previous,
            &env.config,
            &env.paths,
            now(),
        );
        assert!(prompt.starts_with("Work card EX-3 to completion in this workspace.\n\n```json\n"));
        let context = context_of(&prompt);
        assert_eq!(context["ref"], "EX-3");
        assert_eq!(context["runner"], "example-app");
        assert_eq!(context["workspace"]["name"], "example-app");
        assert_eq!(context["workspace"]["path"], checkout.display().to_string());
        assert_eq!(
            context["workspaces"][0]["path"],
            workspace.path.display().to_string()
        );
        assert_eq!(
            context["previous_attempts"],
            json!([{"attempt": 1, "outcome": "timeout", "summary": "no result within 3h",
                    "blocked_on": null, "session_id": "session-1", "new_commits": 2}])
        );
        let unmatched = card_prompt("ZZ-1", None, &[], &env.config, &env.paths, now());
        assert!(context_of(&unmatched)["workspace"].is_null());
        assert_eq!(context_of(&unmatched)["previous_attempts"], json!([]));
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
        assert!(prompt.starts_with(
            "You were mentioned in a discussion about EX-9. Investigate the question"
        ));
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

    #[test]
    fn configured_objectives_replace_the_defaults() {
        let mut env = test_env();
        env.config.card.objective = "Ship {ref}; then report on {ref}.".into();
        env.config.triage.objective = "  Look around.  ".into();
        let card = card_prompt("EX-4", None, &[], &env.config, &env.paths, now());
        assert!(card.starts_with("Ship EX-4; then report on EX-4.\n\n```json\n"));
        assert_eq!(context_of(&card)["ref"], "EX-4");
        let triage = triage_prompt(&env.config, &env.paths, &BTreeMap::new(), &[], &[], now());
        assert!(triage.starts_with("Look around.\n\n```json\n"));
    }

    #[test]
    fn runner_rules_state_the_contract() {
        for rule in [
            "only non-negotiable rules",
            "JSON the output schema requires",
            "Discussion sessions never create branches, commit, push or open pull requests",
            "Only work cards assigned to the user",
            "unattended",
            "Nobody can answer questions",
        ] {
            assert!(RUNNER_RULES.contains(rule), "{rule}");
        }
    }
}
