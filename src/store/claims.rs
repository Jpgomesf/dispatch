//! Level 1 claims: cross-runner exclusivity on one machine. A claim is a row with a lease;
//! an expired lease can be taken over, so a crashed runner never holds work forever.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Result, Store, iso};

pub const CLAIM_LEASE: Duration = Duration::from_secs(10 * 60);
pub const CLAIM_RENEW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    Acquired,
    HeldBy(String),
}

fn millis(time: DateTime<Utc>) -> i64 {
    time.timestamp_millis()
}

fn lease_until(now: DateTime<Utc>, lease: Duration) -> i64 {
    millis(now).saturating_add(i64::try_from(lease.as_millis()).unwrap_or(i64::MAX))
}

/// Inside a write transaction: insert, or take over an expired lease.
pub(super) fn claim_in(
    tx: &Transaction<'_>,
    key: &str,
    runner: &str,
    now: DateTime<Utc>,
    lease: Duration,
) -> rusqlite::Result<Claim> {
    let holder: Option<(String, i64)> = tx
        .query_row(
            "SELECT runner, lease_until FROM claims WHERE key = ?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((holder, until)) = holder
        && until > millis(now)
    {
        return Ok(Claim::HeldBy(holder));
    }
    tx.execute(
        "INSERT INTO claims (key, runner, lease_until, claimed_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (key) DO UPDATE SET runner = ?2, lease_until = ?3, claimed_at = ?4",
        params![key, runner, lease_until(now, lease), iso(now)],
    )?;
    Ok(Claim::Acquired)
}

impl Store {
    /// Atomic under `BEGIN IMMEDIATE`: exactly one of several contending runners acquires.
    pub fn claim(
        &self,
        key: &str,
        runner: &str,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> Result<Claim> {
        self.write(true, |tx| claim_in(tx, key, runner, now, lease))
    }

    /// Extend a claim this runner still holds; `false` when it was lost (expired and taken).
    pub fn renew(
        &self,
        key: &str,
        runner: &str,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> Result<bool> {
        self.write(true, |tx| {
            let changed = tx.execute(
                "UPDATE claims SET lease_until = ?3 WHERE key = ?1 AND runner = ?2",
                params![key, runner, lease_until(now, lease)],
            )?;
            Ok(changed == 1)
        })
    }

    /// Release a claim this runner holds; a claim taken over by another runner is left alone.
    pub fn release(&self, key: &str, runner: &str) -> Result<()> {
        self.write(false, |tx| {
            tx.execute(
                "DELETE FROM claims WHERE key = ?1 AND runner = ?2",
                params![key, runner],
            )
            .map(|_| ())
        })
    }

    /// At startup: the instance lock guarantees no live process of this runner holds any.
    pub fn release_all(&self, runner: &str) -> Result<usize> {
        self.write(false, |tx| {
            tx.execute("DELETE FROM claims WHERE runner = ?1", [runner])
        })
    }

    pub fn holder(&self, key: &str, now: DateTime<Utc>) -> Result<Option<String>> {
        self.read(|c| {
            c.query_row(
                "SELECT runner FROM claims WHERE key = ?1 AND lease_until > ?2",
                params![key, millis(now)],
                |row| row.get(0),
            )
            .optional()
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::store::tests::temp_store;
    use crate::testing::now;

    fn at(minutes: i64) -> DateTime<Utc> {
        now() + TimeDelta::minutes(minutes)
    }

    #[test]
    fn claim_is_exclusive_until_the_lease_expires() {
        let (_dir, store) = temp_store();
        let key = "card:EX-1";
        assert_eq!(
            store.claim(key, "alpha", at(0), CLAIM_LEASE).unwrap(),
            Claim::Acquired
        );
        assert_eq!(
            store.claim(key, "beta", at(9), CLAIM_LEASE).unwrap(),
            Claim::HeldBy("alpha".into())
        );
        assert_eq!(
            store.claim(key, "alpha", at(9), CLAIM_LEASE).unwrap(),
            Claim::HeldBy("alpha".into()),
            "a claim is not re-entrant"
        );
        assert_eq!(store.holder(key, at(9)).unwrap().as_deref(), Some("alpha"));
        // alpha crashed: its lease runs out and beta takes over.
        assert_eq!(
            store.claim(key, "beta", at(10), CLAIM_LEASE).unwrap(),
            Claim::Acquired
        );
        assert!(
            !store.renew(key, "alpha", at(11), CLAIM_LEASE).unwrap(),
            "alpha lost it"
        );
        store.release(key, "alpha").unwrap();
        assert_eq!(store.holder(key, at(11)).unwrap().as_deref(), Some("beta"));
    }

    #[test]
    fn renewal_keeps_the_claim_and_release_frees_it() {
        let (_dir, store) = temp_store();
        let key = "card:EX-2";
        store.claim(key, "alpha", at(0), CLAIM_LEASE).unwrap();
        for minute in 1..=15 {
            assert!(store.renew(key, "alpha", at(minute), CLAIM_LEASE).unwrap());
        }
        assert_eq!(
            store.claim(key, "beta", at(20), CLAIM_LEASE).unwrap(),
            Claim::HeldBy("alpha".into()),
            "renewed at 15m, so held until 25m"
        );
        store.release(key, "alpha").unwrap();
        assert_eq!(
            store.claim(key, "beta", at(20), CLAIM_LEASE).unwrap(),
            Claim::Acquired
        );
        assert_eq!(store.release_all("beta").unwrap(), 1);
        assert_eq!(store.holder(key, at(20)).unwrap(), None);
    }

    #[test]
    fn two_runners_contending_for_one_card_get_exactly_one_claim() {
        let (dir, _store) = temp_store();
        let path = dir.path().join("nested/harness.db");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                // Separate connections, like separate runner processes.
                let store = Store::open(&path).unwrap();
                std::thread::spawn(move || {
                    let runner = if i % 2 == 0 { "alpha" } else { "beta" };
                    store
                        .claim("card:EX-3", runner, at(0), CLAIM_LEASE)
                        .unwrap()
                })
            })
            .collect();
        let results: Vec<Claim> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let acquired = results.iter().filter(|c| **c == Claim::Acquired).count();
        assert_eq!(acquired, 1, "{results:?}");
    }
}
