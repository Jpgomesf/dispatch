//! Discussion sessions: no assignee check (talking is never gated), claimed by
//! `discussion:<thread>`, in a detached worktree removed afterwards when clean. The retry
//! rules apply as for cards, with `[card] max_attempts`, but the retries live in the
//! coordinator's queue: a discussion is a one-off request, not tracker state.

use chrono::{DateTime, Utc};

use super::{Runner, cost, describe, judge, one_line};
use crate::config::Workspace;
use crate::outcome::{self, Next, Outcome, Prior};
use crate::prompts::discussion_prompt;
use crate::results::{DiscussionOutcome, DiscussionResult, DiscussionToRun, discussion_schema};
use crate::session::{Ended, Mode, Session, SessionReport};
use crate::worktree;

impl From<DiscussionOutcome> for Outcome {
    fn from(outcome: DiscussionOutcome) -> Self {
        match outcome {
            DiscussionOutcome::Replied => Outcome::Replied,
            DiscussionOutcome::Drafted => Outcome::Drafted,
            DiscussionOutcome::Skipped => Outcome::Skipped,
            DiscussionOutcome::Failed => Outcome::Failed,
        }
    }
}

/// A discussion waiting for a slot: new, or a retry after an attempt that did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedDiscussion {
    pub discussion: DiscussionToRun,
    /// Counted attempts so far, newest first.
    pub chain: Vec<Prior>,
    /// Not started before this.
    pub not_before: Option<DateTime<Utc>>,
}

impl QueuedDiscussion {
    pub fn new(discussion: DiscussionToRun) -> Self {
        QueuedDiscussion {
            discussion,
            chain: Vec::new(),
            not_before: None,
        }
    }

    #[must_use]
    pub fn ready(&self, now: DateTime<Utc>) -> bool {
        self.not_before.is_none_or(|at| at <= now)
    }
}

impl<S: Session> Runner<S> {
    /// The workspace a discussion runs in: `match` against its ref, then its thread.
    fn discussion_workspace(&self, discussion: &DiscussionToRun) -> Option<Workspace> {
        self.config
            .workspace_for(&discussion.discussion_ref)
            .or_else(|| self.config.workspace_for(&discussion.thread))
            .cloned()
    }

    /// One attempt at a new discussion.
    pub async fn run_discussion(&self, discussion: DiscussionToRun) -> Option<DiscussionResult> {
        self.attempt_discussion(QueuedDiscussion::new(discussion))
            .await
            .0
    }

    /// One attempt; also returns the discussion to queue again when the retry rules say so.
    pub(crate) async fn attempt_discussion(
        &self,
        queued: QueuedDiscussion,
    ) -> (Option<DiscussionResult>, Option<QueuedDiscussion>) {
        let key = queued.discussion.claim_key();
        let label = queued.discussion.discussion_ref.clone();
        let attempted = self
            .claimed(&key, self.work_discussion(&key, &queued.discussion))
            .await;
        let (outcome, summary, result, report) = match attempted {
            Ok(attempted) => attempted,
            Err(reason) => {
                self.emit("discussion", "skipped", &format!("{label} — {reason}"));
                return (None, None);
            }
        };
        let now = self.now();
        let mut next = outcome::next(
            outcome,
            None,
            &queued.chain,
            u32::try_from(queued.chain.len()).unwrap_or(u32::MAX),
            self.config.card.max_attempts,
            now,
        );
        if outcome == Outcome::RateLimited
            && let Some(pause) = self.paused().await
        {
            next = outcome::not_before(next, pause.until);
        }
        let retry = match next {
            Next::Retry { at } => {
                let mut chain = queued.chain;
                if outcome.counts() {
                    chain.insert(
                        0,
                        Prior {
                            outcome,
                            new_commits: None,
                        },
                    );
                }
                Some(QueuedDiscussion {
                    discussion: queued.discussion,
                    chain,
                    not_before: Some(at),
                })
            }
            _ => None,
        };
        let detail = format!(
            "{label}{} — {}{}",
            cost(report.cost_usd),
            one_line(&summary),
            describe(&Ok(next))
        );
        self.emit("discussion", outcome.as_str(), &detail);
        (result, retry)
    }

    async fn work_discussion(
        &self,
        key: &str,
        discussion: &DiscussionToRun,
    ) -> (Outcome, String, Option<DiscussionResult>, SessionReport) {
        let workspace = self.discussion_workspace(discussion);
        let checkout = match &workspace {
            Some(w) => worktree::card_checkout(w, key, &self.paths.worktrees_dir()).await,
            None => Ok(self.state_dir()),
        };
        let checkout = match checkout {
            Ok(checkout) => checkout,
            Err(error) => {
                let report = SessionReport::ended(Ended::Crash(error.clone()));
                return (Outcome::Crash, error, None, report);
            }
        };
        let prompt = discussion_prompt(
            discussion,
            workspace.as_ref().map(|w| (w, checkout.as_path())),
            &self.config,
            &self.paths,
            self.now(),
        );
        let card = &self.config.card;
        let request = self.request(
            Mode::Discussion,
            prompt,
            (&card.model, card.effort, card.max_budget_usd),
            checkout.clone(),
            discussion_schema(),
        );
        let attempt = self
            .begin_attempt(Mode::Discussion, &discussion.discussion_ref, &checkout)
            .await;
        let report = match &attempt {
            Ok((id, _)) => {
                self.run_session(request, *id, &discussion.discussion_ref)
                    .await
            }
            Err(error) => SessionReport::ended(Ended::Crash(error.clone())),
        };
        if let Some(workspace) = &workspace {
            // Discussions never commit; a dirty tree is kept for a person to look at.
            let _ = worktree::release_checkout(workspace, &checkout).await;
        }
        let (outcome, summary, result) =
            judge(&report.ended, "discussion", |r: &DiscussionResult| {
                (r.status.into(), r.summary.clone())
            });
        if let Ok((id, _)) = attempt {
            self.end_attempt(id, self.attempt_end(outcome, &report, &summary))
                .await;
        }
        (outcome, summary, result, report)
    }
}
