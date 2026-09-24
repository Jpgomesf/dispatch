//! Structured-output contract with the workflow skill.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HandledAction {
    Replied,
    Drafted,
    Ignored,
    Escalated,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HandledItem {
    pub source: String,
    /// The event `id` when the item came from an event.
    pub item: String,
    pub action: HandledAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CardToWork {
    #[serde(rename = "ref")]
    pub card_ref: String,
    /// Refs that must be `done` before this card starts.
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

/// A mention that needs investigation before replying: runs as a discussion session.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DiscussionToRun {
    /// Tracker ref, or an event id when the thread has no ticket.
    #[serde(rename = "ref")]
    pub discussion_ref: String,
    /// Permalink of the triggering comment or message.
    #[serde(default)]
    pub thread: String,
    pub question: String,
}

impl DiscussionToRun {
    /// `discussion:<thread>`, or `discussion:<ref>` when there is no thread.
    #[must_use]
    pub fn claim_key(&self) -> String {
        let id = if self.thread.trim().is_empty() {
            &self.discussion_ref
        } else {
            &self.thread
        };
        format!("discussion:{id}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TriageResult {
    #[serde(default)]
    pub cursors: BTreeMap<String, String>,
    #[serde(default)]
    pub handled: Vec<HandledItem>,
    #[serde(default)]
    pub cards_to_work: Vec<CardToWork>,
    #[serde(default)]
    pub discussions_to_run: Vec<DiscussionToRun>,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardOutcome {
    Done,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CardResult {
    #[serde(rename = "ref")]
    pub card_ref: String,
    pub status: CardOutcome,
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub blocked_on: Option<String>,
    pub summary: String,
}

impl CardOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CardOutcome::Done => "done",
            CardOutcome::Blocked => "blocked",
            CardOutcome::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscussionOutcome {
    Replied,
    Drafted,
    Skipped,
    Failed,
}

impl DiscussionOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DiscussionOutcome::Replied => "replied",
            DiscussionOutcome::Drafted => "drafted",
            DiscussionOutcome::Skipped => "skipped",
            DiscussionOutcome::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DiscussionResult {
    #[serde(rename = "ref")]
    pub discussion_ref: String,
    pub status: DiscussionOutcome,
    pub summary: String,
}

fn nullable_string() -> Value {
    json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
}

pub fn triage_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cursors": {"type": "object", "additionalProperties": {"type": "string"}},
            "handled": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "source": {"type": "string"},
                        "item": {"type": "string"},
                        "action": {"enum": ["replied", "drafted", "ignored", "escalated"]}
                    },
                    "required": ["source", "item", "action"]
                }
            },
            "cards_to_work": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string"},
                        "blocked_by": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["ref", "blocked_by"]
                }
            },
            "discussions_to_run": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string"},
                        "thread": {"type": "string"},
                        "question": {"type": "string"}
                    },
                    "required": ["ref", "thread", "question"]
                }
            },
            "summary": {"type": "string"}
        },
        "required": ["cursors", "handled", "cards_to_work", "discussions_to_run", "summary"]
    })
}

pub fn card_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ref": {"type": "string"},
            "status": {"enum": ["done", "blocked", "failed"]},
            "pr_url": nullable_string(),
            "blocked_on": nullable_string(),
            "summary": {"type": "string"}
        },
        "required": ["ref", "status", "pr_url", "blocked_on", "summary"]
    })
}

pub fn discussion_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ref": {"type": "string"},
            "status": {"enum": ["replied", "drafted", "skipped", "failed"]},
            "summary": {"type": "string"}
        },
        "required": ["ref", "status", "summary"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_triage_with_dependencies() {
        let result: TriageResult = serde_json::from_value(json!({
            "cursors": {"slack:C0000000001": "1"},
            "handled": [{"source": "slack:C0000000001", "item": "m1", "action": "drafted"}],
            "cards_to_work": [{"ref": "EX-2", "blocked_by": ["EX-1"]}, {"ref": "EX-1"}],
            "summary": "ok",
            "extra": "ignored"
        }))
        .unwrap();
        assert_eq!(result.cards_to_work[0].blocked_by, vec!["EX-1"]);
        assert!(result.cards_to_work[1].blocked_by.is_empty());
        assert_eq!(result.handled[0].action, HandledAction::Drafted);
        assert!(result.discussions_to_run.is_empty());
    }

    #[test]
    fn parses_discussions_and_their_claim_keys() {
        let result: TriageResult = serde_json::from_value(json!({
            "cursors": {}, "handled": [], "cards_to_work": [], "summary": "s",
            "discussions_to_run": [
                {"ref": "EX-9", "thread": "https://example.com/EX-9/c1", "question": "why?"},
                {"ref": "42", "thread": "", "question": "what?"}
            ]
        }))
        .unwrap();
        let keys: Vec<String> = result
            .discussions_to_run
            .iter()
            .map(DiscussionToRun::claim_key)
            .collect();
        assert_eq!(
            keys,
            ["discussion:https://example.com/EX-9/c1", "discussion:42"]
        );
        let done: DiscussionResult =
            serde_json::from_value(json!({"ref": "EX-9", "status": "drafted", "summary": "s"}))
                .unwrap();
        assert_eq!(done.status, DiscussionOutcome::Drafted);
        let bad = json!({"ref": "EX-9", "status": "done", "summary": "s"});
        assert!(serde_json::from_value::<DiscussionResult>(bad).is_err());
    }

    #[test]
    fn rejects_invalid_results() {
        assert!(serde_json::from_value::<TriageResult>(json!({"unexpected": true})).is_err());
        let bad_status = json!({"ref": "EX-1", "status": "maybe", "summary": "s"});
        assert!(serde_json::from_value::<CardResult>(bad_status).is_err());
    }

    #[test]
    fn schemas_name_every_field() {
        let triage = triage_schema();
        let items = &triage["properties"]["cards_to_work"]["items"];
        assert_eq!(items["required"], json!(["ref", "blocked_by"]));
        let discussions = &triage["properties"]["discussions_to_run"]["items"];
        assert_eq!(
            discussions["required"],
            json!(["ref", "thread", "question"])
        );
        assert_eq!(card_schema()["properties"]["status"]["enum"][0], "done");
        assert_eq!(
            discussion_schema()["properties"]["status"]["enum"][0],
            "replied"
        );
    }
}
