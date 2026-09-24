//! Per-runner cards and cursors.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::{Result, Store, StoreError, iso, parse_time};
use crate::state::{CardState, CardStatus, State};

const CARD_COLUMNS: &str =
    "ref, status, pr_url, updated_at, blocked_by, attempts, retry_at, reason";

/// A card row as stored; statuses and times are checked in `into_card`.
struct CardRow {
    card_ref: String,
    status: String,
    pr_url: Option<String>,
    updated_at: String,
    blocked_by: String,
    attempts: u32,
    retry_at: Option<String>,
    reason: Option<String>,
}

fn card_row(row: &Row<'_>) -> rusqlite::Result<CardRow> {
    Ok(CardRow {
        card_ref: row.get(0)?,
        status: row.get(1)?,
        pr_url: row.get(2)?,
        updated_at: row.get(3)?,
        blocked_by: row.get(4)?,
        attempts: row.get(5)?,
        retry_at: row.get(6)?,
        reason: row.get(7)?,
    })
}

impl CardRow {
    fn into_card(self, store: &Store) -> Result<(String, CardState)> {
        let invalid = |what: String| StoreError::Invalid(format!("card {}: {what}", self.card_ref));
        let status = CardStatus::parse(&self.status)
            .ok_or_else(|| invalid(format!("status {:?}", self.status)))?;
        let time = |text: &str| parse_time(text).map_err(|e| store.error(e));
        let card = CardState {
            status,
            updated_at: time(&self.updated_at)?,
            pr_url: self.pr_url,
            blocked_by: serde_json::from_str(&self.blocked_by)
                .map_err(|e| invalid(format!("blocked_by: {e}")))?,
            attempts: self.attempts,
            retry_at: self.retry_at.as_deref().map(time).transpose()?,
            reason: self.reason,
        };
        Ok((self.card_ref, card))
    }
}

fn put_card(
    tx: &Transaction<'_>,
    runner: &str,
    card_ref: &str,
    card: &CardState,
) -> rusqlite::Result<()> {
    let blocked_by = serde_json::to_string(&card.blocked_by).unwrap_or_else(|_| "[]".into());
    tx.execute(
        "INSERT INTO cards (runner, ref, status, blocked_by, pr_url, updated_at, attempts,
                            retry_at, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (runner, ref) DO UPDATE SET
             status = ?3, blocked_by = ?4, pr_url = ?5, updated_at = ?6, attempts = ?7,
             retry_at = ?8, reason = ?9",
        params![
            runner,
            card_ref,
            card.status.as_str(),
            blocked_by,
            card.pr_url,
            iso(card.updated_at),
            card.attempts,
            card.retry_at.map(iso),
            card.reason
        ],
    )?;
    Ok(())
}

fn put_cursor(tx: &Transaction<'_>, runner: &str, key: &str, value: &str) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO cursors (runner, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT (runner, key) DO UPDATE SET value = ?3",
        params![runner, key, value],
    )?;
    Ok(())
}

fn card_rows(
    c: &Connection,
    runner: &str,
    card_ref: Option<&str>,
) -> rusqlite::Result<Vec<CardRow>> {
    let sql =
        format!("SELECT {CARD_COLUMNS} FROM cards WHERE runner = ?1 AND (?2 IS NULL OR ref = ?2)");
    let mut statement = c.prepare(&sql)?;
    statement
        .query_map(params![runner, card_ref], card_row)?
        .collect()
}

impl Store {
    /// This runner's cards and cursors.
    pub fn load_state(&self, runner: &str) -> Result<State> {
        let (mut state, rows) = self.read(|c| {
            let mut state = State::default();
            let mut cursors = c.prepare("SELECT key, value FROM cursors WHERE runner = ?1")?;
            for row in cursors.query_map([runner], |r| Ok((r.get(0)?, r.get(1)?)))? {
                let (key, value): (String, String) = row?;
                state.cursors.insert(key, value);
            }
            Ok((state, card_rows(c, runner, None)?))
        })?;
        for row in rows {
            let (card_ref, card) = row.into_card(self)?;
            state.cards.insert(card_ref, card);
        }
        Ok(state)
    }

    pub fn card(&self, runner: &str, card_ref: &str) -> Result<Option<CardState>> {
        let rows = self.read(|c| card_rows(c, runner, Some(card_ref)))?;
        match rows.into_iter().next() {
            Some(row) => Ok(Some(row.into_card(self)?.1)),
            None => Ok(None),
        }
    }

    pub fn set_card(&self, runner: &str, card_ref: &str, card: &CardState) -> Result<()> {
        self.write(false, |tx| put_card(tx, runner, card_ref, card))
    }

    /// A card attempt starts: `in_progress` with the triage's `blocked_by`, any scheduled retry
    /// taken; its attempt count and history stay.
    pub fn start_card(
        &self,
        runner: &str,
        card_ref: &str,
        blocked_by: &[String],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let blocked_by = serde_json::to_string(blocked_by).unwrap_or_else(|_| "[]".into());
        self.write(false, |tx| {
            tx.execute(
                "INSERT INTO cards (runner, ref, status, blocked_by, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (runner, ref) DO UPDATE SET
                     status = ?3, blocked_by = ?4, updated_at = ?5, retry_at = NULL",
                params![
                    runner,
                    card_ref,
                    CardStatus::InProgress.as_str(),
                    blocked_by,
                    iso(now)
                ],
            )
            .map(|_| ())
        })
    }

    /// A retry that cannot start (the card is no longer assigned to me, or another runner
    /// holds it) is dropped: the card stays `failed` and triage decides.
    pub fn cancel_retry(&self, runner: &str, card_ref: &str) -> Result<()> {
        self.write(false, |tx| {
            tx.execute(
                "UPDATE cards SET retry_at = NULL WHERE runner = ?1 AND ref = ?2",
                params![runner, card_ref],
            )
            .map(|_| ())
        })
    }

    /// A person asked for the card (`dispatch card`), or a new event mentions a `needs_human`
    /// card: its attempt count starts over and it can be started again. Returns whether a
    /// card was reset.
    pub fn reset_card(
        &self,
        runner: &str,
        card_ref: &str,
        only_needs_human: bool,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.write(false, |tx| {
            let changed = tx.execute(
                "UPDATE cards SET attempts = 0, retry_at = NULL, reason = NULL, updated_at = ?3,
                     status = CASE WHEN status = ?4 THEN ?5 ELSE status END
                 WHERE runner = ?1 AND ref = ?2 AND (NOT ?6 OR status = ?4)",
                params![
                    runner,
                    card_ref,
                    iso(now),
                    CardStatus::NeedsHuman.as_str(),
                    CardStatus::Failed.as_str(),
                    only_needs_human
                ],
            )?;
            Ok(changed == 1)
        })
    }

    pub fn cursor(&self, runner: &str, key: &str) -> Result<Option<String>> {
        self.read(|c| {
            c.query_row(
                "SELECT value FROM cursors WHERE runner = ?1 AND key = ?2",
                params![runner, key],
                |r| r.get(0),
            )
            .optional()
        })
    }

    pub fn set_cursors(&self, runner: &str, cursors: &BTreeMap<String, String>) -> Result<()> {
        self.write(false, |tx| {
            cursors
                .iter()
                .try_for_each(|(key, value)| put_cursor(tx, runner, key, value))
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::store::tests::temp_store;
    use crate::testing::now;

    fn card(status: CardStatus) -> CardState {
        CardState::new(status, now())
    }

    #[test]
    fn cards_and_cursors_are_per_runner() {
        let (_dir, store) = temp_store();
        store
            .set_card("alpha", "EX-1", &card(CardStatus::Done))
            .unwrap();
        store
            .start_card("beta", "EX-1", &["EX-0".into()], now())
            .unwrap();
        let cursors = BTreeMap::from([("slack:C0000000001".to_string(), "c1".to_string())]);
        store.set_cursors("alpha", &cursors).unwrap();

        let alpha = store.load_state("alpha").unwrap();
        assert_eq!(alpha.status("EX-1"), Some(CardStatus::Done));
        assert_eq!(alpha.cursors, cursors);
        let beta = store.load_state("beta").unwrap();
        assert!(beta.in_progress("EX-1"));
        assert_eq!(beta.cards["EX-1"].blocked_by, ["EX-0"]);
        assert!(beta.cursors.is_empty());
        assert_eq!(
            store
                .cursor("alpha", "slack:C0000000001")
                .unwrap()
                .as_deref(),
            Some("c1")
        );
        assert_eq!(store.cursor("beta", "slack:C0000000001").unwrap(), None);
    }

    #[test]
    fn retry_fields_round_trip_and_a_start_takes_the_retry() {
        let (_dir, store) = temp_store();
        let mut failed = card(CardStatus::Failed);
        failed.attempts = 2;
        failed.retry_at = Some(now() + TimeDelta::minutes(2));
        failed.pr_url = Some("https://example.com/pr/1".into());
        store.set_card("alpha", "EX-1", &failed).unwrap();
        assert_eq!(store.card("alpha", "EX-1").unwrap(), Some(failed.clone()));
        assert_eq!(store.card("alpha", "EX-2").unwrap(), None);

        store.start_card("alpha", "EX-1", &[], now()).unwrap();
        let started = store.card("alpha", "EX-1").unwrap().unwrap();
        assert_eq!(started.status, CardStatus::InProgress);
        assert_eq!(started.retry_at, None);
        assert_eq!(started.attempts, 2, "the count survives a start");
        assert_eq!(started.pr_url, failed.pr_url);
    }

    #[test]
    fn a_reset_starts_the_count_over() {
        let (_dir, store) = temp_store();
        let mut stuck = card(CardStatus::NeedsHuman);
        stuck.attempts = 3;
        stuck.reason = Some("max_attempts".into());
        store.set_card("alpha", "EX-1", &stuck).unwrap();
        let mut blocked = card(CardStatus::Blocked);
        blocked.attempts = 1;
        store.set_card("alpha", "EX-2", &blocked).unwrap();

        assert!(!store.reset_card("alpha", "EX-2", true, now()).unwrap());
        assert!(store.reset_card("alpha", "EX-1", true, now()).unwrap());
        let reset = store.card("alpha", "EX-1").unwrap().unwrap();
        assert_eq!(
            (reset.status, reset.attempts, reset.reason),
            (CardStatus::Failed, 0, None)
        );
        assert!(store.reset_card("alpha", "EX-2", false, now()).unwrap());
        let reset = store.card("alpha", "EX-2").unwrap().unwrap();
        assert_eq!((reset.status, reset.attempts), (CardStatus::Blocked, 0));
        assert!(!store.reset_card("alpha", "EX-9", false, now()).unwrap());
    }
}
