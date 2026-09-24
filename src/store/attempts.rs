//! Session attempts: one row per triage, card or discussion session, opened when it starts
//! and closed with its outcome. `dispatch status` and `dispatch history` read them.

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{Row, params};

use super::{Result, Store, iso, parse_time};

/// A recorded session attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    pub id: i64,
    pub mode: String,
    /// Card or discussion ref; empty for triage.
    pub reference: String,
    /// 1-based, per runner, mode and ref.
    pub attempt: u32,
    /// Where the session ran; `claude --resume <session_id>` works from there.
    pub cwd: String,
    pub started_at: DateTime<Utc>,
    /// `None` while the session runs.
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
    pub summary: Option<String>,
    pub blocked_on: Option<String>,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptEnd {
    pub outcome: String,
    pub summary: String,
    pub blocked_on: Option<String>,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub ended_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, mode, ref, attempt, cwd, started_at, ended_at, outcome, summary, \
                       blocked_on, session_id, cost_usd";

fn attempt_from(row: &Row<'_>) -> rusqlite::Result<Attempt> {
    let started_at: String = row.get(5)?;
    let ended_at: Option<String> = row.get(6)?;
    Ok(Attempt {
        id: row.get(0)?,
        mode: row.get(1)?,
        reference: row.get(2)?,
        attempt: row.get(3)?,
        cwd: row.get(4)?,
        started_at: parse_time(&started_at)?,
        ended_at: ended_at.as_deref().map(parse_time).transpose()?,
        outcome: row.get(7)?,
        summary: row.get(8)?,
        blocked_on: row.get(9)?,
        session_id: row.get(10)?,
        cost_usd: row.get(11)?,
    })
}

impl Store {
    /// Open an attempt row as a session starts; returns its id and attempt number.
    pub fn begin_attempt(
        &self,
        runner: &str,
        mode: &str,
        reference: &str,
        cwd: &Path,
        now: DateTime<Utc>,
    ) -> Result<(i64, u32)> {
        self.write(true, |tx| {
            let attempt: u32 = tx.query_row(
                "SELECT COALESCE(MAX(attempt), 0) + 1 FROM attempts
                 WHERE runner = ?1 AND mode = ?2 AND ref = ?3",
                params![runner, mode, reference],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO attempts (runner, mode, ref, attempt, cwd, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    runner,
                    mode,
                    reference,
                    attempt,
                    cwd.display().to_string(),
                    iso(now)
                ],
            )?;
            Ok((tx.last_insert_rowid(), attempt))
        })
    }

    pub fn end_attempt(&self, id: i64, end: &AttemptEnd) -> Result<()> {
        self.write(false, |tx| {
            tx.execute(
                "UPDATE attempts SET ended_at = ?2, outcome = ?3, summary = ?4, blocked_on = ?5,
                     session_id = COALESCE(?6, session_id), cost_usd = ?7
                 WHERE id = ?1",
                params![
                    id,
                    iso(end.ended_at),
                    end.outcome,
                    end.summary,
                    end.blocked_on,
                    end.session_id,
                    end.cost_usd
                ],
            )
            .map(|_| ())
        })
    }

    /// Newest first: this runner's attempts, only those of `reference` when given; a negative
    /// `limit` returns them all.
    pub fn attempts(
        &self,
        runner: &str,
        reference: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Attempt>> {
        self.read(|c| {
            let sql = format!(
                "SELECT {COLUMNS} FROM attempts WHERE runner = ?1 AND (?2 IS NULL OR ref = ?2)
                 ORDER BY id DESC LIMIT ?3"
            );
            let mut statement = c.prepare(&sql)?;
            statement
                .query_map(params![runner, reference, limit], attempt_from)?
                .collect()
        })
    }

    /// This runner's attempts that have not ended, oldest first.
    pub fn open_attempts(&self, runner: &str) -> Result<Vec<Attempt>> {
        self.read(|c| {
            let sql = format!(
                "SELECT {COLUMNS} FROM attempts WHERE runner = ?1 AND ended_at IS NULL ORDER BY id"
            );
            let mut statement = c.prepare(&sql)?;
            statement.query_map([runner], attempt_from)?.collect()
        })
    }

    /// At startup (under the instance lock): attempts a stopped process left open are closed.
    pub fn close_open_attempts(
        &self,
        runner: &str,
        outcome: &str,
        summary: &str,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        self.write(false, |tx| {
            tx.execute(
                "UPDATE attempts SET ended_at = ?2, outcome = ?3, summary = ?4
                 WHERE runner = ?1 AND ended_at IS NULL",
                params![runner, iso(now), outcome, summary],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::temp_store;
    use crate::testing::now;

    fn end(outcome: &str) -> AttemptEnd {
        AttemptEnd {
            outcome: outcome.into(),
            summary: format!("{outcome} summary"),
            blocked_on: None,
            session_id: Some("session-1".into()),
            cost_usd: Some(0.25),
            ended_at: now(),
        }
    }

    #[test]
    fn attempts_are_numbered_per_runner_mode_and_ref() {
        let (_dir, store) = temp_store();
        let cwd = Path::new("/tmp/example/worktree");
        assert_eq!(
            store
                .begin_attempt("alpha", "card", "EX-1", cwd, now())
                .unwrap()
                .1,
            1
        );
        let (second, number) = store
            .begin_attempt("alpha", "card", "EX-1", cwd, now())
            .unwrap();
        assert_eq!(number, 2);
        let other = [
            ("beta", "card", "EX-1"),
            ("alpha", "discussion", "EX-1"),
            ("alpha", "card", "EX-2"),
            ("alpha", "triage", ""),
        ];
        for (runner, mode, reference) in other {
            let (_, number) = store
                .begin_attempt(runner, mode, reference, cwd, now())
                .unwrap();
            assert_eq!(number, 1, "{runner} {mode} {reference}");
        }

        store.end_attempt(second, &end("done")).unwrap();
        let ex1 = store.attempts("alpha", Some("EX-1"), -1).unwrap();
        assert_eq!(ex1.len(), 3, "both card attempts and the discussion");
        let latest = ex1
            .iter()
            .find(|a| a.id == second)
            .expect("second attempt listed");
        assert_eq!(latest.outcome.as_deref(), Some("done"));
        assert_eq!(latest.session_id.as_deref(), Some("session-1"));
        assert_eq!(latest.cost_usd, Some(0.25));
        assert_eq!(latest.cwd, "/tmp/example/worktree");
        assert_eq!(ex1[0].mode, "discussion", "newest first");
        assert_eq!(store.attempts("alpha", None, 2).unwrap().len(), 2);
        assert_eq!(store.attempts("alpha", None, -1).unwrap().len(), 5);
    }

    #[test]
    fn open_attempts_are_closed_at_startup() {
        let (_dir, store) = temp_store();
        let cwd = Path::new("/tmp/example");
        let (open, _) = store
            .begin_attempt("alpha", "card", "EX-1", cwd, now())
            .unwrap();
        let (ended, _) = store
            .begin_attempt("alpha", "card", "EX-2", cwd, now())
            .unwrap();
        store.end_attempt(ended, &end("blocked")).unwrap();
        store
            .begin_attempt("beta", "card", "EX-3", cwd, now())
            .unwrap();
        let running = store.open_attempts("alpha").unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, open);
        assert_eq!(running[0].ended_at, None);

        let closed = store
            .close_open_attempts("alpha", "crash", "runner stopped", now())
            .unwrap();
        assert_eq!(closed, 1);
        assert!(store.open_attempts("alpha").unwrap().is_empty());
        assert_eq!(store.open_attempts("beta").unwrap().len(), 1, "untouched");
        let listed = store.attempts("alpha", Some("EX-1"), -1).unwrap();
        assert_eq!(listed[0].outcome.as_deref(), Some("crash"));
    }
}
