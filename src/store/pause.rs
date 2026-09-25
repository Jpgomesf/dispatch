//! The machine-wide pause: after a usage or rate limit, every runner on the machine starts
//! no session until it ends. One row; a later end extends it, an earlier one never shortens it.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

use super::{Result, Store, iso, parse_time};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pause {
    pub until: DateTime<Utc>,
    pub reason: String,
    /// The runner that set it.
    pub runner: String,
    pub set_at: DateTime<Utc>,
}

impl Store {
    /// Pause every runner until `until`, unless a pause already runs longer. Returns whether
    /// this call set or extended it.
    pub fn set_pause(
        &self,
        until: DateTime<Utc>,
        reason: &str,
        runner: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.write(true, |tx| {
            let current: Option<String> = tx
                .query_row("SELECT until FROM pause WHERE id = 1", [], |row| row.get(0))
                .optional()?;
            if let Some(current) = current
                && parse_time(&current)? >= until
            {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO pause (id, until, reason, runner, set_at) VALUES (1, ?1, ?2, ?3, ?4)
                 ON CONFLICT (id) DO UPDATE SET until = ?1, reason = ?2, runner = ?3, set_at = ?4",
                params![iso(until), reason, runner, iso(now)],
            )?;
            Ok(true)
        })
    }

    /// The pause in force at `now`, if any.
    pub fn pause(&self, now: DateTime<Utc>) -> Result<Option<Pause>> {
        let row: Option<(String, String, String, String)> = self.read(|c| {
            c.query_row(
                "SELECT until, reason, runner, set_at FROM pause WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
        })?;
        let Some((until, reason, runner, set_at)) = row else {
            return Ok(None);
        };
        let until = parse_time(&until).map_err(|e| self.error(e))?;
        if until <= now {
            return Ok(None);
        }
        Ok(Some(Pause {
            until,
            reason,
            runner,
            set_at: parse_time(&set_at).map_err(|e| self.error(e))?,
        }))
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
    fn a_pause_holds_every_runner_until_it_ends_and_only_grows() {
        let (_dir, store) = temp_store();
        assert_eq!(store.pause(now()).unwrap(), None);
        assert!(
            store
                .set_pause(at(15), "rate limit", "alpha", now())
                .unwrap()
        );
        let pause = store.pause(at(1)).unwrap().unwrap();
        assert_eq!(
            (pause.until, pause.runner.as_str(), pause.reason.as_str()),
            (at(15), "alpha", "rate limit")
        );
        assert!(
            !store.set_pause(at(10), "shorter", "beta", now()).unwrap(),
            "never shortened"
        );
        assert!(store.set_pause(at(60), "reset", "beta", now()).unwrap());
        assert_eq!(store.pause(at(30)).unwrap().unwrap().runner, "beta");
        assert_eq!(store.pause(at(60)).unwrap(), None, "over at its end");
    }
}
