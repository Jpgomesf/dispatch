//! Intake queue: `new` → `batched` (handed to one triage) → `done`, or back to `new` when
//! that triage fails.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};

use super::claims::{CLAIM_LEASE, claim_in};
use super::{Result, Store, iso, parse_time};
use crate::intake::{Event, EventKind, IncomingEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enqueued {
    Inserted(i64),
    /// Already stored (by this or another runner): dedup per source and external id.
    Duplicate,
    /// Another runner is inserting or handling it right now.
    HeldBy(String),
}

const EVENT_COLUMNS: &str = "id, source, kind, mentions_me, sender, occurred_at, payload";

fn event_from(row: &Row<'_>) -> rusqlite::Result<Event> {
    let kind: String = row.get(2)?;
    let occurred_at: String = row.get(5)?;
    let payload: String = row.get(6)?;
    let invalid = |e: Box<dyn std::error::Error + Send + Sync>| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e)
    };
    Ok(Event {
        id: row.get(0)?,
        source: row.get(1)?,
        kind: EventKind::parse(&kind).ok_or_else(|| invalid(format!("kind {kind:?}").into()))?,
        mentions_me: row.get(3)?,
        sender: row.get(4)?,
        occurred_at: parse_time(&occurred_at)?,
        payload: serde_json::from_str(&payload).map_err(|e| invalid(e.into()))?,
    })
}

impl Store {
    /// Claim `event:<source>:<external_id>` and insert, in one `BEGIN IMMEDIATE` transaction,
    /// so only one runner stores an event even when routing filters overlap.
    pub fn enqueue(
        &self,
        runner: &str,
        event: &IncomingEvent,
        now: DateTime<Utc>,
    ) -> Result<Enqueued> {
        let payload = event.payload.to_string();
        self.write(true, |tx| {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM events WHERE source = ?1 AND external_id = ?2",
                    params![event.source, event.external_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                return Ok(Enqueued::Duplicate);
            }
            let key = event.claim_key();
            if let super::Claim::HeldBy(other) = claim_in(tx, &key, runner, now, CLAIM_LEASE)? {
                return Ok(Enqueued::HeldBy(other));
            }
            tx.execute(
                "INSERT INTO events (runner, source, external_id, kind, mentions_me, sender,
                                     occurred_at, payload, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'new', ?9)",
                params![
                    runner,
                    event.source,
                    event.external_id,
                    event.kind.as_str(),
                    event.mentions_me,
                    event.sender,
                    iso(event.occurred_at),
                    payload,
                    iso(now),
                ],
            )?;
            Ok(Enqueued::Inserted(tx.last_insert_rowid()))
        })
    }

    pub fn new_event_count(&self, runner: &str) -> Result<i64> {
        self.read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM events WHERE runner = ?1 AND status = 'new'",
                [runner],
                |row| row.get(0),
            )
        })
    }

    /// Every `new` event of this runner becomes `batched` and is returned, oldest first.
    pub fn take_batch(&self, runner: &str) -> Result<Vec<Event>> {
        self.write(true, |tx| {
            let events = {
                let sql = format!(
                    "SELECT {EVENT_COLUMNS} FROM events WHERE runner = ?1 AND status = 'new' ORDER BY id"
                );
                let mut statement = tx.prepare(&sql)?;
                statement
                    .query_map([runner], event_from)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            tx.execute(
                "UPDATE events SET status = 'batched' WHERE runner = ?1 AND status = 'new'",
                [runner],
            )?;
            Ok(events)
        })
    }

    /// After triage: `done` (and their claims released) on success, else back to `new`.
    pub fn finish_batch(&self, runner: &str, ids: &[i64], succeeded: bool) -> Result<()> {
        self.write(false, |tx| {
            for id in ids {
                if succeeded {
                    tx.execute(
                        "UPDATE events SET status = 'done' WHERE id = ?1 AND runner = ?2",
                        params![id, runner],
                    )?;
                    tx.execute(
                        "DELETE FROM claims WHERE runner = ?2 AND key = (
                             SELECT 'event:' || source || ':' || external_id FROM events WHERE id = ?1)",
                        params![id, runner],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE events SET status = 'new' WHERE id = ?1 AND runner = ?2",
                        params![id, runner],
                    )?;
                }
            }
            Ok(())
        })
    }

    /// At startup: batches of a crashed run go back to `new`.
    pub fn requeue_batched(&self, runner: &str) -> Result<usize> {
        self.write(false, |tx| {
            tx.execute(
                "UPDATE events SET status = 'new' WHERE runner = ?1 AND status = 'batched'",
                [runner],
            )
        })
    }

    pub fn event_status(&self, id: i64) -> Result<Option<String>> {
        self.read(|c| {
            c.query_row("SELECT status FROM events WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::store::tests::temp_store;
    use crate::testing::now;

    pub(crate) fn message(external_id: &str) -> IncomingEvent {
        IncomingEvent {
            source: "notifications".into(),
            external_id: external_id.into(),
            kind: EventKind::Message,
            mentions_me: false,
            sender: Some("Example Person".into()),
            occurred_at: now(),
            payload: json!({"title": "#example-channel", "body": "hello"}),
        }
    }

    #[test]
    fn dedups_per_source_and_external_id() {
        let (_dir, store) = temp_store();
        let first = store.enqueue("alpha", &message("101"), now()).unwrap();
        assert!(matches!(first, Enqueued::Inserted(_)));
        assert_eq!(
            store.enqueue("alpha", &message("101"), now()).unwrap(),
            Enqueued::Duplicate
        );
        let other_source = IncomingEvent {
            source: "manual".into(),
            ..message("101")
        };
        assert!(matches!(
            store.enqueue("alpha", &other_source, now()).unwrap(),
            Enqueued::Inserted(_)
        ));
    }

    #[test]
    fn two_runners_contending_for_one_event_store_it_once() {
        let (dir, _store) = temp_store();
        let path = dir.path().join("nested/dispatch.db");
        let threads: Vec<_> = ["alpha", "beta", "alpha", "beta"]
            .into_iter()
            .map(|runner| {
                let store = Store::open(&path).unwrap();
                std::thread::spawn(move || store.enqueue(runner, &message("202"), now()).unwrap())
            })
            .collect();
        let results: Vec<Enqueued> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let inserted = results
            .iter()
            .filter(|r| matches!(r, Enqueued::Inserted(_)))
            .count();
        assert_eq!(inserted, 1, "{results:?}");
        let store = Store::open(&path).unwrap();
        let owners =
            store.new_event_count("alpha").unwrap() + store.new_event_count("beta").unwrap();
        assert_eq!(owners, 1);
    }

    #[test]
    fn a_held_event_claim_blocks_another_runner() {
        let (_dir, store) = temp_store();
        let event = message("303");
        store
            .claim(&event.claim_key(), "beta", now(), CLAIM_LEASE)
            .unwrap();
        assert_eq!(
            store.enqueue("alpha", &event, now()).unwrap(),
            Enqueued::HeldBy("beta".into())
        );
    }

    #[test]
    fn batch_lifecycle() {
        let (_dir, store) = temp_store();
        store.enqueue("alpha", &message("1"), now()).unwrap();
        store.enqueue("alpha", &message("2"), now()).unwrap();
        store.enqueue("beta", &message("3"), now()).unwrap();
        assert_eq!(store.new_event_count("alpha").unwrap(), 2);

        let batch = store.take_batch("alpha").unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].kind, EventKind::Message);
        assert_eq!(batch[0].payload["body"], "hello");
        assert_eq!(
            serde_json::to_value(&batch[0]).unwrap()["id"],
            batch[0].id.to_string()
        );
        assert_eq!(store.new_event_count("alpha").unwrap(), 0);
        let ids: Vec<i64> = batch.iter().map(|e| e.id).collect();

        store.finish_batch("alpha", &ids, false).unwrap();
        assert_eq!(
            store.new_event_count("alpha").unwrap(),
            2,
            "failed: back to new"
        );
        let batch = store.take_batch("alpha").unwrap();
        store.finish_batch("alpha", &ids, true).unwrap();
        assert_eq!(
            store.event_status(batch[0].id).unwrap().as_deref(),
            Some("done")
        );
        assert_eq!(store.holder("event:notifications:1", now()).unwrap(), None);
        assert_eq!(
            store.new_event_count("beta").unwrap(),
            1,
            "other runner untouched"
        );
    }

    #[test]
    fn crashed_batches_are_requeued() {
        let (_dir, store) = temp_store();
        store.enqueue("alpha", &message("1"), now()).unwrap();
        store.take_batch("alpha").unwrap();
        assert_eq!(store.requeue_batched("alpha").unwrap(), 1);
        assert_eq!(store.new_event_count("alpha").unwrap(), 1);
    }
}
