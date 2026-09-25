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
    /// The agent reported it blocked: started again only when triage lists it.
    Blocked,
    /// The last attempt did not finish; `retry_at` says when the runner tries again.
    Failed,
    /// Past `max_attempts`, no progress, a second failure or a broken environment: shown to
    /// triage as an escalation, never started again until a person resets it.
    NeedsHuman,
}

impl CardStatus {
    const ALL: [CardStatus; 5] = [
        CardStatus::InProgress,
        CardStatus::Done,
        CardStatus::Blocked,
        CardStatus::Failed,
        CardStatus::NeedsHuman,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CardStatus::InProgress => "in_progress",
            CardStatus::Done => "done",
            CardStatus::Blocked => "blocked",
            CardStatus::Failed => "failed",
            CardStatus::NeedsHuman => "needs_human",
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
    /// The triage's `blocked_by` when the card last started.
    pub blocked_by: Vec<String>,
    /// Counted attempts since the card was last done or reset.
    pub attempts: u32,
    /// When a `failed` card is retried.
    pub retry_at: Option<DateTime<Utc>>,
    /// Why a card is `needs_human`.
    pub reason: Option<String>,
}

impl CardState {
    #[must_use]
    pub fn new(status: CardStatus, updated_at: DateTime<Utc>) -> CardState {
        CardState {
            status,
            updated_at,
            pr_url: None,
            blocked_by: Vec::new(),
            attempts: 0,
            retry_at: None,
            reason: None,
        }
    }

    /// A retry is scheduled and not yet due: the runner, not triage, starts it.
    #[must_use]
    pub fn awaits_retry(&self, now: DateTime<Utc>) -> bool {
        self.status == CardStatus::Failed && self.retry_at.is_some_and(|at| at > now)
    }

    /// A retry is scheduled and due.
    #[must_use]
    pub fn retry_due(&self, now: DateTime<Utc>) -> bool {
        self.status == CardStatus::Failed && self.retry_at.is_some_and(|at| at <= now)
    }
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

    /// Triage may not start it: a person must reset it, or the runner retries it itself.
    #[must_use]
    pub fn held_back(&self, card_ref: &str, now: DateTime<Utc>) -> bool {
        self.cards
            .get(card_ref)
            .is_some_and(|card| card.status == CardStatus::NeedsHuman || card.awaits_retry(now))
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
    fn retries_and_escalations_hold_a_card_back_from_triage() {
        let now = crate::testing::now();
        let later = now + chrono::TimeDelta::minutes(5);
        let mut state = State::default();
        let card = |status, retry_at| {
            let mut card = CardState::new(status, now);
            card.retry_at = retry_at;
            card
        };
        state
            .cards
            .insert("A".into(), card(CardStatus::Failed, Some(later)));
        state
            .cards
            .insert("B".into(), card(CardStatus::Failed, Some(now)));
        state
            .cards
            .insert("C".into(), card(CardStatus::Failed, None));
        state
            .cards
            .insert("D".into(), card(CardStatus::NeedsHuman, None));
        state
            .cards
            .insert("E".into(), card(CardStatus::Blocked, None));
        let held: Vec<&str> = ["A", "B", "C", "D", "E", "F"]
            .into_iter()
            .filter(|r| state.held_back(r, now))
            .collect();
        assert_eq!(held, ["A", "D"]);
        assert!(state.cards["B"].retry_due(now));
        assert!(!state.cards["A"].retry_due(now));
        assert!(!state.cards["C"].retry_due(now), "no retry scheduled");
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
