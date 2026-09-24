//! What intake sources produce and what triage receives.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// Chat or notification: triage reads the thread and decides.
    Message,
    /// A ticket assigned to me changed; may become a card.
    Work,
    /// Activity on a ticket I take part in but am not assigned; never a card.
    Discussion,
}

impl EventKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Message => "message",
            EventKind::Work => "work",
            EventKind::Discussion => "discussion",
        }
    }

    pub fn parse(text: &str) -> Option<EventKind> {
        match text {
            "message" => Some(EventKind::Message),
            "work" => Some(EventKind::Work),
            "discussion" => Some(EventKind::Discussion),
            _ => None,
        }
    }
}

/// An event as a source reports it, before it is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingEvent {
    pub source: String,
    /// Dedup key within `source`: per message / comment, or per issue update.
    pub external_id: String,
    pub kind: EventKind,
    pub mentions_me: bool,
    pub sender: Option<String>,
    pub occurred_at: DateTime<Utc>,
    /// Source-specific: title / subtitle / body / url / ref where available.
    pub payload: Value,
}

impl IncomingEvent {
    /// Cross-runner claim key for this event.
    #[must_use]
    pub fn claim_key(&self) -> String {
        format!("event:{}:{}", self.source, self.external_id)
    }
}

/// A stored event as it goes into the triage context; `id` (a string there) is what
/// `handled[].item` echoes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    #[serde(serialize_with = "as_string")]
    pub id: i64,
    pub source: String,
    pub kind: EventKind,
    pub mentions_me: bool,
    pub sender: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub payload: Value,
}

fn as_string<S: serde::Serializer>(id: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(id)
}

/// What one fetch of a tracker stream returns: the events to store and the newest
/// timestamp seen (including items filtered out), which becomes the stream's cursor.
#[derive(Debug, Default)]
pub struct Fetched {
    pub events: Vec<IncomingEvent>,
    pub latest: Option<DateTime<Utc>>,
}

const PREVIEW_CHARS: usize = 500;

/// The first characters of a message or comment body for the triage context.
#[must_use]
pub fn preview(text: &str) -> String {
    text.chars().take(PREVIEW_CHARS).collect()
}
