//! Per-runner cards and cursors.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Result, Store, StoreError, iso, parse_time};
use crate::state::{CardState, CardStatus, State};

fn put_card(
    tx: &Transaction<'_>,
    runner: &str,
    card_ref: &str,
    card: &CardState,
    blocked_by: &[String],
) -> rusqlite::Result<()> {
    let blocked_by = serde_json::to_string(blocked_by).unwrap_or_else(|_| "[]".into());
    tx.execute(
        "INSERT INTO cards (runner, ref, status, blocked_by, pr_url, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (runner, ref) DO UPDATE SET
             status = ?3, blocked_by = ?4, pr_url = ?5, updated_at = ?6",
        params![
            runner,
            card_ref,
            card.status.as_str(),
            blocked_by,
            card.pr_url,
            iso(card.updated_at)
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

impl Store {
    /// This runner's cards and cursors.
    pub fn load_state(&self, runner: &str) -> Result<State> {
        let rows = self.read(|c| {
            let mut state = State::default();
            let mut cursors = c.prepare("SELECT key, value FROM cursors WHERE runner = ?1")?;
            for row in cursors.query_map([runner], |r| Ok((r.get(0)?, r.get(1)?)))? {
                let (key, value): (String, String) = row?;
                state.cursors.insert(key, value);
            }
            let mut cards =
                c.prepare("SELECT ref, status, pr_url, updated_at FROM cards WHERE runner = ?1")?;
            let rows = cards
                .query_map([runner], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((state, rows))
        })?;
        let (mut state, cards) = rows;
        for (card_ref, status, pr_url, updated_at) in cards {
            let status = CardStatus::parse(&status).ok_or_else(|| {
                StoreError::Invalid(format!("card {card_ref}: status {status:?}"))
            })?;
            let updated_at = parse_time(&updated_at).map_err(|e| self.error(e))?;
            state.cards.insert(
                card_ref,
                CardState {
                    status,
                    updated_at,
                    pr_url,
                },
            );
        }
        Ok(state)
    }

    pub fn set_card(
        &self,
        runner: &str,
        card_ref: &str,
        card: &CardState,
        blocked_by: &[String],
    ) -> Result<()> {
        self.write(false, |tx| put_card(tx, runner, card_ref, card, blocked_by))
    }

    /// Cards left `in_progress` by a crashed run become `failed`; returns their refs.
    pub fn fail_in_progress(&self, runner: &str, now: DateTime<Utc>) -> Result<usize> {
        self.write(false, |tx| {
            tx.execute(
                "UPDATE cards SET status = ?2, updated_at = ?3 WHERE runner = ?1 AND status = ?4",
                params![
                    runner,
                    CardStatus::Failed.as_str(),
                    iso(now),
                    CardStatus::InProgress.as_str()
                ],
            )
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
    use super::*;
    use crate::store::tests::temp_store;
    use crate::testing::now;

    fn card(status: CardStatus) -> CardState {
        CardState {
            status,
            updated_at: now(),
            pr_url: None,
        }
    }

    #[test]
    fn cards_and_cursors_are_per_runner() {
        let (_dir, store) = temp_store();
        store
            .set_card("alpha", "EX-1", &card(CardStatus::Done), &[])
            .unwrap();
        store
            .set_card(
                "beta",
                "EX-1",
                &card(CardStatus::InProgress),
                &["EX-0".into()],
            )
            .unwrap();
        let cursors = BTreeMap::from([("slack:C0000000001".to_string(), "c1".to_string())]);
        store.set_cursors("alpha", &cursors).unwrap();

        let alpha = store.load_state("alpha").unwrap();
        assert_eq!(alpha.status("EX-1"), Some(CardStatus::Done));
        assert_eq!(alpha.cursors, cursors);
        let beta = store.load_state("beta").unwrap();
        assert!(beta.in_progress("EX-1"));
        assert!(beta.cursors.is_empty());
        assert_eq!(
            store
                .cursor("alpha", "slack:C0000000001")
                .unwrap()
                .as_deref(),
            Some("c1")
        );
        assert_eq!(store.cursor("beta", "slack:C0000000001").unwrap(), None);

        assert_eq!(store.fail_in_progress("beta", now()).unwrap(), 1);
        assert_eq!(
            store.load_state("beta").unwrap().status("EX-1"),
            Some(CardStatus::Failed)
        );
        assert_eq!(
            store.load_state("alpha").unwrap().status("EX-1"),
            Some(CardStatus::Done)
        );
    }
}
