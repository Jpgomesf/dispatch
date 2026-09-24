//! Card sessions: the personal-scope check, the `card:<ref>` claim, one attempt in the
//! card's worktree, and what follows it by the retry rules.

use chrono::{DateTime, Utc};

use super::{Runner, cost, describe, judge, one_line};
use crate::config::{UnknownWorkspace, Workspace};
use crate::intake::secrets::Secrets;
use crate::intake::{CardScope, card_scope};
use crate::outcome::{self, Next, Outcome};
use crate::prompts::{PREVIOUS_ATTEMPTS, PreviousAttempt, card_prompt};
use crate::results::{CardOutcome, CardResult, card_schema};
use crate::session::{Mode, Session, SessionReport};
use crate::state::{CardState, CardStatus};
use crate::worktree;

impl From<CardOutcome> for Outcome {
    fn from(outcome: CardOutcome) -> Self {
        match outcome {
            CardOutcome::Done => Outcome::Done,
            CardOutcome::Blocked => Outcome::Blocked,
            CardOutcome::Failed => Outcome::Failed,
        }
    }
}

/// Record `next` on the card: its status, the counted attempts of this run, and when it is
/// retried or why it needs a person.
fn apply(card: &mut CardState, outcome: Outcome, next: Next, now: DateTime<Utc>) {
    card.updated_at = now;
    card.retry_at = None;
    card.reason = None;
    if outcome.counts() {
        card.attempts = card.attempts.saturating_add(1);
    }
    match next {
        Next::Finished => {
            card.status = CardStatus::Done;
            card.attempts = 0;
        }
        Next::Blocked => card.status = CardStatus::Blocked,
        Next::Retry { at } => {
            card.status = CardStatus::Failed;
            card.retry_at = Some(at);
        }
        Next::NeedsHuman(reason) => {
            card.status = CardStatus::NeedsHuman;
            card.reason = Some(reason.as_str().to_string());
        }
    }
}

impl<S: Session> Runner<S> {
    pub fn resolve_workspace(
        &self,
        card_ref: &str,
        name: Option<&str>,
    ) -> Result<Option<Workspace>, UnknownWorkspace> {
        match name {
            Some(name) => self.config.workspace_named(name).map(|w| Some(w.clone())),
            None => Ok(self.config.workspace_for(card_ref).cloned()),
        }
    }

    /// `dispatch card <ref>`: a person asks for the card, so its attempt count starts over
    /// (a `needs_human` card becomes workable again), then one attempt.
    pub async fn run_card(
        &self,
        card_ref: &str,
        workspace_name: Option<&str>,
    ) -> Result<Option<CardResult>, UnknownWorkspace> {
        let workspace = self.resolve_workspace(card_ref, workspace_name)?;
        let (runner, owned_ref, now) = (self.name().to_string(), card_ref.to_string(), self.now());
        if let Err(error) = self
            .store
            .call(move |s| s.reset_card(&runner, &owned_ref, false, now))
            .await
        {
            self.emit("card", "failed", &format!("{card_ref} — store: {error}"));
            return Ok(None);
        }
        Ok(self.run_card_in(card_ref, workspace, &[]).await)
    }

    /// Personal scope, then the `card:<ref>` claim, then one attempt.
    pub(crate) async fn run_card_in(
        &self,
        card_ref: &str,
        workspace: Option<Workspace>,
        blocked_by: &[String],
    ) -> Option<CardResult> {
        let secrets = Secrets::load(&self.paths.secrets, self.env.clone());
        if let CardScope::Refused(reason) = card_scope(&self.config, &secrets, card_ref).await {
            self.emit(
                "card",
                "refused",
                &format!("{card_ref} — {}", one_line(&reason)),
            );
            self.cancel_retry(card_ref).await;
            return None;
        }
        let key = format!("card:{card_ref}");
        match self
            .claimed(&key, self.claimed_card(card_ref, workspace, blocked_by))
            .await
        {
            Ok(result) => result,
            Err(reason) => {
                self.emit("card", "skipped", &format!("{card_ref} — {reason}"));
                self.cancel_retry(card_ref).await;
                None
            }
        }
    }

    /// Best effort: a retry that could not start is not tried again every tick.
    async fn cancel_retry(&self, card_ref: &str) {
        let (runner, card_ref) = (self.name().to_string(), card_ref.to_string());
        let _ = self
            .store
            .call(move |s| s.cancel_retry(&runner, &card_ref))
            .await;
    }

    async fn claimed_card(
        &self,
        card_ref: &str,
        workspace: Option<Workspace>,
        blocked_by: &[String],
    ) -> Option<CardResult> {
        let (runner, owned_ref, blocked, now) = (
            self.name().to_string(),
            card_ref.to_string(),
            blocked_by.to_vec(),
            self.now(),
        );
        if let Err(error) = self
            .store
            .call(move |s| s.start_card(&runner, &owned_ref, &blocked, now))
            .await
        {
            self.emit("card", "failed", &format!("{card_ref} — store: {error}"));
            return None;
        }
        let checkout = match &workspace {
            Some(w) => worktree::card_checkout(w, card_ref, &self.paths.worktrees_dir()).await,
            None => Ok(self.state_dir()),
        };
        let checkout = match checkout {
            Ok(checkout) => checkout,
            Err(error) => {
                // No session ran; the card follows the rules for a crash, so it is retried.
                let next = self
                    .settle_card(card_ref, None, Outcome::Crash, None, None)
                    .await;
                let detail = format!("{card_ref} — {}{}", one_line(&error), describe(&next));
                self.emit("card", Outcome::Crash.as_str(), &detail);
                return None;
            }
        };
        // Progress is judged from the worktree, never from what the agent says.
        let head_before = match &workspace {
            Some(w) => worktree::head(w, &checkout).await,
            None => None,
        };
        // Best effort: without its history a session still starts, from git and the tracker.
        let (runner, owned_ref) = (self.name().to_string(), card_ref.to_string());
        let previous: Vec<PreviousAttempt> = self
            .store
            .call(move |s| s.recent_card_attempts(&runner, &owned_ref, PREVIOUS_ATTEMPTS))
            .await
            .map(|attempts| attempts.iter().map(PreviousAttempt::from).collect())
            .unwrap_or_default();
        let prompt = card_prompt(
            card_ref,
            workspace.as_ref().map(|w| (w, checkout.as_path())),
            &previous,
            &self.config,
            &self.paths,
            self.now(),
        );
        let card = &self.config.card;
        let request = self.request(
            Mode::Card,
            prompt,
            (&card.model, card.effort, card.max_budget_usd),
            checkout.clone(),
            card_schema(),
        );
        let (id, report) = match self.begin_attempt(Mode::Card, card_ref, &checkout).await {
            Ok((id, _)) => (Some(id), self.run_session(request, id).await),
            Err(error) => (
                None,
                SessionReport::ended(crate::session::Ended::Crash(error)),
            ),
        };
        let new_commits = match (&workspace, head_before) {
            (Some(w), Some(before)) => match worktree::head(w, &checkout).await {
                Some(after) => worktree::commits_between(&checkout, &before, &after).await,
                None => None,
            },
            _ => None,
        };
        let (outcome, summary, result) = judge(&report.ended, "card", |r: &CardResult| {
            (r.status.into(), r.summary.clone())
        });
        if let Some(id) = id {
            let mut end = self.attempt_end(outcome, &report, &summary);
            end.blocked_on = result.as_ref().and_then(|r| r.blocked_on.clone());
            end.new_commits = new_commits;
            self.end_attempt(id, end).await;
        }
        let pr_url = result.as_ref().and_then(|r| r.pr_url.clone());
        let next = self
            .settle_card(card_ref, id, outcome, new_commits, pr_url)
            .await;
        let detail = format!(
            "{card_ref}{} — {}{}",
            cost(report.cost_usd),
            one_line(&summary),
            describe(&next)
        );
        self.emit("card", outcome.as_str(), &detail);
        if let (Outcome::Done, Some(workspace)) = (outcome, &workspace) {
            // Best effort: a dirty worktree is kept for a person to look at.
            let _ = worktree::release_checkout(workspace, &checkout).await;
        }
        result
    }

    /// What follows a card attempt (`current`, when one was recorded), by the retry rules,
    /// recorded on the card.
    pub(crate) async fn settle_card(
        &self,
        card_ref: &str,
        current: Option<i64>,
        outcome: Outcome,
        new_commits: Option<u32>,
        pr_url: Option<String>,
    ) -> Result<Next, String> {
        let (runner, card_ref, now) = (self.name().to_string(), card_ref.to_string(), self.now());
        let max_attempts = self.config.card.max_attempts;
        self.store
            .call(move |s| {
                let mut card = s
                    .card(&runner, &card_ref)?
                    .unwrap_or_else(|| CardState::new(CardStatus::InProgress, now));
                let chain = s.card_chain(&runner, &card_ref, current, card.attempts)?;
                let next = outcome::next(outcome, new_commits, &chain, max_attempts, now);
                apply(&mut card, outcome, next, now);
                if pr_url.is_some() {
                    card.pr_url = pr_url;
                }
                s.set_card(&runner, &card_ref, &card)?;
                Ok(next)
            })
            .await
            .map_err(|e| format!("store: {e}"))
    }
}
