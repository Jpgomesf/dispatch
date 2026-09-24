//! Card state types and the per-runner instance lock.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardState {
    pub status: CardStatus,
    pub updated_at: DateTime<Utc>,
    pub pr_url: Option<String>,
}

/// One runner's cards and cursors, as loaded from `dispatch.db` for a scheduling decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_status_strings_round_trip() {
        for status in CardStatus::ALL {
            assert_eq!(CardStatus::parse(status.as_str()), Some(status));
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
