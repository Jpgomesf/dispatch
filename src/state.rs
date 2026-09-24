//! Card state types, the per-runner instance lock and the phase 1 `state.json` reader
//! (kept only to import it into `harness.db`).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardStatus {
    InProgress,
    Done,
    Blocked,
    Failed,
}

impl CardStatus {
    const ALL: [CardStatus; 4] = [
        CardStatus::InProgress,
        CardStatus::Done,
        CardStatus::Blocked,
        CardStatus::Failed,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CardStatus::InProgress => "in_progress",
            CardStatus::Done => "done",
            CardStatus::Blocked => "blocked",
            CardStatus::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<CardStatus> {
        Self::ALL.into_iter().find(|status| status.as_str() == text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardState {
    pub status: CardStatus,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub pr_url: Option<String>,
}

/// One runner's cards and cursors, as loaded from `harness.db` for a scheduling decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub cursors: BTreeMap<String, String>,
    pub cards: BTreeMap<String, CardState>,
}

impl State {
    #[must_use]
    pub fn status(&self, card_ref: &str) -> Option<CardStatus> {
        self.cards.get(card_ref).map(|card| card.status)
    }

    #[must_use]
    pub fn in_progress(&self, card_ref: &str) -> bool {
        self.status(card_ref) == Some(CardStatus::InProgress)
    }
}

/// Held for the life of a process that runs sessions (`heartbeat`, `card`), so two processes
/// of one runner never work the same cards or release each other's claims and cards.
#[derive(Debug)]
pub struct InstanceLock {
    _file: std::fs::File,
}

impl InstanceLock {
    /// `Ok(None)` when another process holds the lock.
    pub fn try_acquire(path: &Path) -> Result<Option<InstanceLock>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create state dir {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(InstanceLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("lock {}", path.display()))
            }
        }
    }
}

/// Phase 1 `state.json`; a missing file is an empty state.
pub fn load_state(path: &Path) -> Result<State> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_file_is_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_state(&dir.path().join("state.json")).unwrap(),
            State::default()
        );
    }

    #[test]
    fn reads_state_written_by_the_python_runner() {
        let text = r#"{"cursors": {"tracker:linear": "c1"}, "sends": [],
            "cards": {"EX-1": {"status": "in_progress", "updated_at": "2026-01-15T09:30:00Z", "pr_url": null}}}"#;
        let state: State = serde_json::from_str(text).unwrap();
        assert!(state.in_progress("EX-1"));
        assert_eq!(state.cursors["tracker:linear"], "c1");
    }

    #[test]
    fn card_status_strings_round_trip() {
        for status in CardStatus::ALL {
            assert_eq!(CardStatus::parse(status.as_str()), Some(status));
            assert_eq!(serde_json::to_value(status).unwrap(), status.as_str());
        }
        assert_eq!(CardStatus::parse("maybe"), None);
    }

    #[test]
    fn instance_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let lock_file = dir.path().join("example-app.lock");
        let first = InstanceLock::try_acquire(&lock_file).unwrap();
        assert!(first.is_some());
        assert!(InstanceLock::try_acquire(&lock_file).unwrap().is_none());
        drop(first);
        assert!(InstanceLock::try_acquire(&lock_file).unwrap().is_some());
    }
}
