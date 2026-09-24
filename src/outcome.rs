//! How an attempt ended, and what follows: the retry rules. Pure, so every rule is tested
//! without sessions or a store.
//!
//! | Outcome | Next |
//! |---|---|
//! | `done` (and a triage `ok`, a discussion `replied` / `drafted` / `skipped`) | finished |
//! | `timeout`, `stuck`, `api_error`, `crash`, `invalid_output` | retry in a fresh session, backoff 1m doubling to 30m |
//! | `failed` (the agent's own report) | one retry, then `needs_human` |
//! | `blocked` | none: eligible again when triage lists it |
//! | `interrupted` (kill switch, signal) | retry when the runner runs again; not counted |
//!
//! Counted attempts are capped at `max_attempts`; past it the card is `needs_human`. Two
//! counted attempts in a row that end unfinished with no new commits on the card's worktree
//! are `needs_human` too (`no_progress`): progress is judged from git, not from the agent.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

use crate::session::Ended;

pub const RETRY_BASE: Duration = Duration::from_secs(60);
pub const RETRY_CAP: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Triage returned its result.
    Ok,
    Done,
    Blocked,
    /// The agent reported the work failed.
    Failed,
    Replied,
    Drafted,
    Skipped,
    /// The wall-clock limit passed.
    Timeout,
    /// No stream event for the idle limit.
    Stuck,
    /// The result reported an error.
    ApiError,
    /// The process ended without a result.
    Crash,
    /// A successful result without a valid structured output.
    InvalidOutput,
    /// Stopped by the kill switch or a signal.
    Interrupted,
}

impl Outcome {
    const ALL: [Outcome; 13] = [
        Outcome::Ok,
        Outcome::Done,
        Outcome::Blocked,
        Outcome::Failed,
        Outcome::Replied,
        Outcome::Drafted,
        Outcome::Skipped,
        Outcome::Timeout,
        Outcome::Stuck,
        Outcome::ApiError,
        Outcome::Crash,
        Outcome::InvalidOutput,
        Outcome::Interrupted,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Done => "done",
            Outcome::Blocked => "blocked",
            Outcome::Failed => "failed",
            Outcome::Replied => "replied",
            Outcome::Drafted => "drafted",
            Outcome::Skipped => "skipped",
            Outcome::Timeout => "timeout",
            Outcome::Stuck => "stuck",
            Outcome::ApiError => "api_error",
            Outcome::Crash => "crash",
            Outcome::InvalidOutput => "invalid_output",
            Outcome::Interrupted => "interrupted",
        }
    }

    pub fn parse(text: &str) -> Option<Outcome> {
        Self::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == text)
    }

    /// How a session that produced no usable result ended; `None` for a structured output,
    /// which the caller parses into its own result.
    #[must_use]
    pub fn of_ended(ended: &Ended) -> Option<Outcome> {
        match ended {
            Ended::Output(_) => None,
            Ended::ApiError(_) => Some(Outcome::ApiError),
            Ended::InvalidOutput(_) => Some(Outcome::InvalidOutput),
            Ended::Crash(_) => Some(Outcome::Crash),
            Ended::Timeout(_) => Some(Outcome::Timeout),
            Ended::Stuck(_) => Some(Outcome::Stuck),
            Ended::Interrupted => Some(Outcome::Interrupted),
        }
    }

    /// The work is finished; nothing follows.
    #[must_use]
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            Outcome::Ok | Outcome::Done | Outcome::Replied | Outcome::Drafted | Outcome::Skipped
        )
    }

    /// A failure of the session rather than of the work: retried automatically.
    #[must_use]
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Outcome::Timeout
                | Outcome::Stuck
                | Outcome::ApiError
                | Outcome::Crash
                | Outcome::InvalidOutput
        )
    }

    /// Counts toward `max_attempts`.
    #[must_use]
    pub fn counts(self) -> bool {
        !matches!(self, Outcome::Interrupted)
    }
}

/// An earlier counted attempt in the current run of attempts (since the card was last done
/// or reset), newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prior {
    pub outcome: Outcome,
    /// Commits it added to the worktree's HEAD; `None` when there is no worktree to measure.
    pub new_commits: Option<u32>,
}

/// Why a card waits for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    MaxAttempts,
    NoProgress,
    Failed,
}

impl Reason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::MaxAttempts => "max_attempts",
            Reason::NoProgress => "no_progress",
            Reason::Failed => "failed",
        }
    }
}

/// What follows an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Finished,
    /// Waits until triage lists it again.
    Blocked,
    /// A fresh session at `at`.
    Retry {
        at: DateTime<Utc>,
    },
    NeedsHuman(Reason),
}

/// Delay before retry number `counted` (1-based): 1m, 2m, 4m... capped at 30m.
#[must_use]
pub fn retry_backoff(counted: u32) -> Duration {
    let doublings = counted.saturating_sub(1).min(16);
    RETRY_CAP.min(RETRY_BASE.saturating_mul(1u32 << doublings))
}

fn after(now: DateTime<Utc>, delay: Duration) -> DateTime<Utc> {
    now + TimeDelta::from_std(delay).unwrap_or(TimeDelta::MAX)
}

/// What follows `outcome`, given the counted attempts before it in this run (`chain`, newest
/// first) and the cap.
#[must_use]
pub fn next(
    outcome: Outcome,
    new_commits: Option<u32>,
    chain: &[Prior],
    max_attempts: u32,
    now: DateTime<Utc>,
) -> Next {
    if outcome.is_finished() {
        return Next::Finished;
    }
    if !outcome.counts() {
        return Next::Retry { at: now };
    }
    let stalled = |commits: Option<u32>| commits == Some(0);
    if stalled(new_commits)
        && chain
            .first()
            .is_some_and(|prior| stalled(prior.new_commits))
    {
        return Next::NeedsHuman(Reason::NoProgress);
    }
    let counted = u32::try_from(chain.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    if counted >= max_attempts {
        return Next::NeedsHuman(Reason::MaxAttempts);
    }
    match outcome {
        Outcome::Blocked => Next::Blocked,
        Outcome::Failed if chain.iter().any(|p| p.outcome == Outcome::Failed) => {
            Next::NeedsHuman(Reason::Failed)
        }
        _ => Next::Retry {
            at: after(now, retry_backoff(counted)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::now;

    fn chain(outcomes: &[Outcome]) -> Vec<Prior> {
        outcomes
            .iter()
            .map(|&outcome| Prior {
                outcome,
                new_commits: None,
            })
            .collect()
    }

    fn minutes(m: i64) -> DateTime<Utc> {
        now() + TimeDelta::minutes(m)
    }

    #[test]
    fn outcome_strings_round_trip() {
        for outcome in Outcome::ALL {
            assert_eq!(Outcome::parse(outcome.as_str()), Some(outcome));
        }
        assert_eq!(Outcome::parse("maybe"), None);
    }

    #[test]
    fn backoff_doubles_from_a_minute_to_half_an_hour() {
        let minutes: Vec<u64> = (1..=7).map(|n| retry_backoff(n).as_secs() / 60).collect();
        assert_eq!(minutes, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(retry_backoff(u32::MAX), RETRY_CAP);
    }

    #[test]
    fn transient_outcomes_retry_with_backoff_until_the_cap() {
        for outcome in [
            Outcome::Timeout,
            Outcome::Stuck,
            Outcome::ApiError,
            Outcome::Crash,
            Outcome::InvalidOutput,
        ] {
            assert!(outcome.is_transient());
            assert_eq!(
                next(outcome, None, &[], 3, now()),
                Next::Retry { at: minutes(1) }
            );
            assert_eq!(
                next(outcome, None, &chain(&[Outcome::Crash]), 3, now()),
                Next::Retry { at: minutes(2) }
            );
            assert_eq!(
                next(
                    outcome,
                    None,
                    &chain(&[Outcome::Crash, Outcome::Stuck]),
                    3,
                    now()
                ),
                Next::NeedsHuman(Reason::MaxAttempts)
            );
        }
    }

    #[test]
    fn an_agent_failure_retries_once() {
        assert_eq!(
            next(Outcome::Failed, None, &[], 5, now()),
            Next::Retry { at: minutes(1) }
        );
        let again = chain(&[Outcome::Timeout, Outcome::Failed]);
        assert_eq!(
            next(Outcome::Failed, None, &again, 5, now()),
            Next::NeedsHuman(Reason::Failed)
        );
    }

    #[test]
    fn blocked_waits_for_triage_and_counts() {
        assert_eq!(next(Outcome::Blocked, None, &[], 3, now()), Next::Blocked);
        let twice = chain(&[Outcome::Blocked, Outcome::Blocked]);
        assert_eq!(
            next(Outcome::Blocked, None, &twice, 3, now()),
            Next::NeedsHuman(Reason::MaxAttempts)
        );
    }

    #[test]
    fn two_unfinished_attempts_in_a_row_without_commits_are_no_progress() {
        let idle = |outcome| Prior {
            outcome,
            new_commits: Some(0),
        };
        let busy = Prior {
            outcome: Outcome::Timeout,
            new_commits: Some(3),
        };
        let unknown = Prior {
            outcome: Outcome::Timeout,
            new_commits: None,
        };
        let no_progress = Next::NeedsHuman(Reason::NoProgress);
        for outcome in [Outcome::Timeout, Outcome::Blocked, Outcome::Failed] {
            assert_eq!(
                next(outcome, Some(0), &[idle(Outcome::Crash)], 9, now()),
                no_progress
            );
        }
        assert_ne!(
            next(Outcome::Timeout, Some(0), &[busy], 9, now()),
            no_progress
        );
        assert_ne!(
            next(Outcome::Timeout, Some(2), &[idle(Outcome::Crash)], 9, now()),
            no_progress
        );
        assert_ne!(next(Outcome::Timeout, Some(0), &[], 9, now()), no_progress);
        assert_ne!(
            next(Outcome::Timeout, None, &[unknown], 9, now()),
            no_progress,
            "without a worktree nothing is measured"
        );
        assert_eq!(
            next(Outcome::Done, Some(0), &[idle(Outcome::Crash)], 9, now()),
            Next::Finished
        );
        assert_eq!(
            next(
                Outcome::Interrupted,
                Some(0),
                &[idle(Outcome::Crash)],
                9,
                now()
            ),
            Next::Retry { at: now() },
            "an interruption is not an attempt at the work"
        );
    }

    #[test]
    fn interruptions_retry_at_once_and_are_not_counted() {
        assert!(!Outcome::Interrupted.counts());
        let full = chain(&[Outcome::Crash, Outcome::Crash, Outcome::Crash]);
        assert_eq!(
            next(Outcome::Interrupted, None, &full, 3, now()),
            Next::Retry { at: now() }
        );
    }

    #[test]
    fn finished_work_ends_the_run() {
        let full = chain(&[Outcome::Crash, Outcome::Crash, Outcome::Crash]);
        for outcome in [
            Outcome::Done,
            Outcome::Ok,
            Outcome::Replied,
            Outcome::Drafted,
            Outcome::Skipped,
        ] {
            assert_eq!(next(outcome, None, &full, 3, now()), Next::Finished);
        }
    }

    #[test]
    fn ended_sessions_map_to_outcomes() {
        use serde_json::json;
        let cases = [
            (Ended::Output(json!({})), None),
            (Ended::ApiError("e".into()), Some(Outcome::ApiError)),
            (
                Ended::InvalidOutput("e".into()),
                Some(Outcome::InvalidOutput),
            ),
            (Ended::Crash("e".into()), Some(Outcome::Crash)),
            (Ended::Timeout("e".into()), Some(Outcome::Timeout)),
            (Ended::Stuck("e".into()), Some(Outcome::Stuck)),
            (Ended::Interrupted, Some(Outcome::Interrupted)),
        ];
        for (ended, outcome) in cases {
            assert_eq!(Outcome::of_ended(&ended), outcome, "{ended:?}");
        }
    }
}
