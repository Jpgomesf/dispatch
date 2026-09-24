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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HeartbeatResult {
    #[serde(default)]
    pub cursors: BTreeMap<String, String>,
    #[serde(default)]
    pub handled: Vec<HandledItem>,
    #[serde(default)]
    pub cards_to_work: Vec<CardToWork>,
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
    pub fn as_str(self) -> &'static str {
        match self {
            CardOutcome::Done => "done",
            CardOutcome::Blocked => "blocked",
            CardOutcome::Failed => "failed",
        }
    }
}

fn nullable_string() -> Value {
    json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
}

pub fn heartbeat_schema() -> Value {
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
            "summary": {"type": "string"}
        },
        "required": ["cursors", "handled", "cards_to_work", "summary"]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_heartbeat_with_dependencies() {
        let result: HeartbeatResult = serde_json::from_value(json!({
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
    }

    #[test]
    fn rejects_invalid_results() {
        assert!(serde_json::from_value::<HeartbeatResult>(json!({"unexpected": true})).is_err());
        let bad_status = json!({"ref": "EX-1", "status": "maybe", "summary": "s"});
        assert!(serde_json::from_value::<CardResult>(bad_status).is_err());
    }

    #[test]
    fn schemas_name_every_field() {
        let heartbeat = heartbeat_schema();
        let items = &heartbeat["properties"]["cards_to_work"]["items"];
        assert_eq!(items["required"], json!(["ref", "blocked_by"]));
        assert_eq!(card_schema()["properties"]["status"]["enum"][0], "done");
    }
}
