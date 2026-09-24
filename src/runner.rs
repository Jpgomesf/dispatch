//! Sessions: triage, cards and discussions, each one `claude` process. Scheduling (when they
//! start, batching of intake events, parallelism) lives in `coordinator`.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{Notify, watch};
use tokio::time::{Instant, interval_at};

use crate::config::{Config, Effort, UnknownWorkspace, Workspace};
use crate::intake::secrets::{EnvLookup, Secrets, process_env};
use crate::intake::{CardScope, Event, IntakeContext, card_scope};
use crate::paths::Paths;
use crate::prompts::{INTAKE_CURSOR_PREFIX, card_prompt, discussion_prompt, triage_prompt};
use crate::results::{
    CardOutcome, CardResult, DiscussionResult, DiscussionToRun, TriageResult, card_schema,
    discussion_schema, triage_schema,
};
use crate::session::{Mode, Session, SessionRequest, Shutdown};
use crate::state::{CardState, CardStatus};
use crate::store::{CLAIM_LEASE, CLAIM_RENEW, Claim, Store};
use crate::worktree;

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
pub type Output = Arc<dyn Fn(String) + Send + Sync>;

pub struct Runner<S> {
    pub config: Arc<Config>,
    pub paths: Arc<Paths>,
    session: Arc<S>,
    store: Store,
    env: EnvLookup,
    clock: Clock,
    out: Output,
    shutdown: Arc<watch::Sender<Shutdown>>,
    /// Raised by intake when events are stored, so the coordinator looks right away.
    pub(crate) wake: Arc<Notify>,
}

impl<S> Clone for Runner<S> {
    fn clone(&self) -> Self {
        Runner {
            config: self.config.clone(),
            paths: self.paths.clone(),
            session: self.session.clone(),
            store: self.store.clone(),
            env: self.env.clone(),
            clock: self.clock.clone(),
            out: self.out.clone(),
            shutdown: self.shutdown.clone(),
            wake: self.wake.clone(),
        }
    }
}

pub(crate) fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(300).collect()
}

fn cost(cost_usd: Option<f64>) -> String {
    cost_usd
        .map(|c| format!(" cost=${c:.2}"))
        .unwrap_or_default()
}

impl From<CardOutcome> for CardStatus {
    fn from(outcome: CardOutcome) -> Self {
        match outcome {
            CardOutcome::Done => CardStatus::Done,
            CardOutcome::Blocked => CardStatus::Blocked,
            CardOutcome::Failed => CardStatus::Failed,
        }
    }
}

impl<S: Session> Runner<S> {
    pub fn new(config: Config, paths: Paths, store: Store, session: S) -> Self {
        Runner {
            config: Arc::new(config),
            paths: Arc::new(paths),
            session: Arc::new(session),
            store,
            env: process_env(),
            clock: Arc::new(Utc::now),
            out: Arc::new(|line| println!("{line}")),
            shutdown: Arc::new(watch::channel(Shutdown::Run).0),
            wake: Arc::new(Notify::new()),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_output(mut self, out: Output) -> Self {
        self.out = out;
        self
    }

    pub fn with_env(mut self, env: EnvLookup) -> Self {
        self.env = env;
        self
    }

    pub fn session(&self) -> &S {
        &self.session
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// Signal handlers raise the level; every running session watches it.
    pub fn shutdown_handle(&self) -> Arc<watch::Sender<Shutdown>> {
        self.shutdown.clone()
    }

    pub(crate) fn subscribe_shutdown(&self) -> watch::Receiver<Shutdown> {
        self.shutdown.subscribe()
    }

    #[must_use]
    pub fn killed(&self) -> bool {
        self.paths.kill_switch().exists()
    }

    #[must_use]
    pub fn should_stop(&self) -> bool {
        *self.shutdown.borrow() != Shutdown::Run || self.killed()
    }

    pub(crate) fn begin_graceful_shutdown(&self) {
        self.shutdown.send_if_modified(|level| {
            let starting = *level == Shutdown::Run;
            if starting {
                *level = Shutdown::Graceful;
            }
            starting
        });
    }

    pub(crate) fn emit(&self, kind: &str, status: &str, detail: &str) {
        let now = self.now().format("%Y-%m-%dT%H:%M:%SZ");
        (self.out)(
            format!("{now} {kind} {status} {detail}")
                .trim_end()
                .to_string(),
        );
    }

    pub(crate) fn intake_context(&self) -> IntakeContext {
        IntakeContext {
            config: self.config.clone(),
            paths: self.paths.clone(),
            store: self.store.clone(),
            env: self.env.clone(),
            clock: self.clock.clone(),
            out: self.out.clone(),
            wake: self.wake.clone(),
        }
    }

    fn state_dir(&self) -> PathBuf {
        let _ = std::fs::create_dir_all(&self.paths.state_dir);
        self.paths.state_dir.clone()
    }

    fn request(
        &self,
        mode: Mode,
        prompt: String,
        (model, effort, max_budget_usd): (&str, Effort, f64),
        cwd: PathBuf,
        output_schema: serde_json::Value,
    ) -> SessionRequest {
        SessionRequest {
            mode,
            prompt,
            model: model.to_string(),
            effort,
            max_budget_usd,
            cwd,
            plugin_dir: self.paths.plugin_dir.clone(),
            output_schema,
        }
    }

    /// At startup, under the instance lock (no other live process of this runner): cards left
    /// `in_progress` become `failed` so triage can queue them again, this runner's claims are
    /// released, and batches a crashed triage held go back to `new`.
    pub async fn recover(&self) {
        let runner = self.name().to_string();
        let now = self.now();
        let recovered = self
            .store
            .call(move |store| {
                store.fail_in_progress(&runner, now)?;
                store.release_all(&runner)?;
                store.requeue_batched(&runner)
            })
            .await;
        if let Err(error) = recovered {
            self.emit("triage", "failed", &format!("store: {error}"));
        }
    }

    /// Hold `key` for as long as `work` runs: claimed first, renewed every minute, released
    /// at the end. `Err(holder)` when another runner holds it; nothing runs then.
    async fn claimed<T>(&self, key: &str, work: impl Future<Output = T>) -> Result<T, String> {
        let (runner, owned_key, now) = (self.name().to_string(), key.to_string(), self.now());
        let claim = self
            .store
            .call(move |s| s.claim(&owned_key, &runner, now, CLAIM_LEASE))
            .await
            .map_err(|e| format!("store: {e}"))?;
        if let Claim::HeldBy(holder) = claim {
            return Err(format!("held by {holder}"));
        }
        let mut work = pin!(work);
        let mut renew = interval_at(Instant::now() + CLAIM_RENEW, CLAIM_RENEW);
        let mut lost = false;
        let output = loop {
            tokio::select! {
                output = &mut work => break output,
                _ = renew.tick() => {
                    let (runner, owned_key, now) = (self.name().to_string(), key.to_string(), self.now());
                    let renewed = self.store
                        .call(move |s| s.renew(&owned_key, &runner, now, CLAIM_LEASE))
                        .await;
                    if !matches!(renewed, Ok(true)) && !lost {
                        lost = true;
                        self.emit("claim", "lost", key);
                    }
                }
            }
        };
        let (runner, owned_key) = (self.name().to_string(), key.to_string());
        let _ = self
            .store
            .call(move |s| s.release(&owned_key, &runner))
            .await;
        Ok(output)
    }

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

    pub async fn run_card(
        &self,
        card_ref: &str,
        workspace_name: Option<&str>,
    ) -> Result<Option<CardResult>, UnknownWorkspace> {
        let workspace = self.resolve_workspace(card_ref, workspace_name)?;
        Ok(self.run_card_in(card_ref, workspace, &[]).await)
    }

    /// Personal scope, then the `card:<ref>` claim, then the session.
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
                None
            }
        }
    }

    async fn claimed_card(
        &self,
        card_ref: &str,
        workspace: Option<Workspace>,
        blocked_by: &[String],
    ) -> Option<CardResult> {
        let started = self
            .record_card(card_ref, CardStatus::InProgress, None, blocked_by)
            .await;
        let worked = match started {
            Ok(()) => self.work_card(card_ref, workspace.as_ref()).await,
            Err(error) => Err(error),
        };
        match worked {
            Err(error) => {
                let _ = self
                    .record_card(card_ref, CardStatus::Failed, None, blocked_by)
                    .await;
                self.emit(
                    "card",
                    "failed",
                    &format!("{card_ref} — {}", one_line(&error)),
                );
                None
            }
            Ok((result, cost_usd, checkout)) => {
                let status = result.status.into();
                let pr_url = result.pr_url.clone();
                if let Err(error) = self.record_card(card_ref, status, pr_url, blocked_by).await {
                    self.emit("card", "failed", &format!("{card_ref} — {error}"));
                    return None;
                }
                let detail = format!("{card_ref}{} — {}", cost(cost_usd), result.summary);
                self.emit("card", result.status.as_str(), &detail);
                if let (CardOutcome::Done, Some(workspace)) = (result.status, &workspace) {
                    // Best effort: a dirty worktree is kept for a person to look at.
                    let _ = worktree::release_checkout(workspace, &checkout).await;
                }
                Some(result)
            }
        }
    }

    async fn record_card(
        &self,
        card_ref: &str,
        status: CardStatus,
        pr_url: Option<String>,
        blocked_by: &[String],
    ) -> Result<(), String> {
        let card = CardState {
            status,
            updated_at: self.now(),
            pr_url,
        };
        let (runner, card_ref, blocked_by) = (
            self.name().to_string(),
            card_ref.to_string(),
            blocked_by.to_vec(),
        );
        self.store
            .call(move |s| s.set_card(&runner, &card_ref, &card, &blocked_by))
            .await
            .map_err(|e| format!("store: {e}"))
    }

    async fn work_card(
        &self,
        card_ref: &str,
        workspace: Option<&Workspace>,
    ) -> Result<(CardResult, Option<f64>, PathBuf), String> {
        let checkout = match workspace {
            Some(w) => worktree::card_checkout(w, card_ref, &self.paths.worktrees_dir()).await?,
            None => self.state_dir(),
        };
        let prompt = card_prompt(
            card_ref,
            workspace.map(|w| (w, checkout.as_path())),
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
        let outcome = self
            .session
            .run(request, self.shutdown.subscribe())
            .await
            .map_err(|e| e.0)?;
        let result: CardResult = serde_json::from_value(outcome.output)
            .map_err(|e| format!("invalid card result: {e}"))?;
        Ok((result, outcome.cost_usd, checkout))
    }

    /// The workspace a discussion runs in: `match` against its ref, then its thread.
    fn discussion_workspace(&self, discussion: &DiscussionToRun) -> Option<Workspace> {
        self.config
            .workspace_for(&discussion.discussion_ref)
            .or_else(|| self.config.workspace_for(&discussion.thread))
            .cloned()
    }

    /// A discussion session: no assignee check (talking is never gated), claimed by
    /// `discussion:<thread>`, in a detached worktree removed afterwards when clean.
    pub async fn run_discussion(&self, discussion: DiscussionToRun) -> Option<DiscussionResult> {
        let key = discussion.claim_key();
        let label = discussion.discussion_ref.clone();
        match self
            .claimed(&key, self.work_discussion(&key, discussion))
            .await
        {
            Ok(Ok((result, cost_usd))) => {
                let detail = format!("{label}{} — {}", cost(cost_usd), result.summary);
                self.emit("discussion", result.status.as_str(), &detail);
                Some(result)
            }
            Ok(Err(error)) => {
                self.emit(
                    "discussion",
                    "failed",
                    &format!("{label} — {}", one_line(&error)),
                );
                None
            }
            Err(reason) => {
                self.emit("discussion", "skipped", &format!("{label} — {reason}"));
                None
            }
        }
    }

    async fn work_discussion(
        &self,
        key: &str,
        discussion: DiscussionToRun,
    ) -> Result<(DiscussionResult, Option<f64>), String> {
        let workspace = self.discussion_workspace(&discussion);
        let checkout = match &workspace {
            Some(w) => worktree::card_checkout(w, key, &self.paths.worktrees_dir()).await?,
            None => self.state_dir(),
        };
        let prompt = discussion_prompt(
            &discussion,
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
        let outcome = self.session.run(request, self.shutdown.subscribe()).await;
        if let Some(workspace) = &workspace {
            // Discussions never commit; a dirty tree is kept for a person to look at.
            let _ = worktree::release_checkout(workspace, &checkout).await;
        }
        let outcome = outcome.map_err(|e| e.0)?;
        let result: DiscussionResult = serde_json::from_value(outcome.output)
            .map_err(|e| format!("invalid discussion result: {e}"))?;
        Ok((result, outcome.cost_usd))
    }

    /// One triage session over `events` (empty: a fallback sweep). Cursors are persisted as
    /// soon as it returns.
    pub async fn triage(&self, events: Vec<Event>) -> Option<TriageResult> {
        match self.try_triage(&events).await {
            Ok((result, cost_usd)) => {
                let detail = format!(
                    "events={} handled={} cards={} discussions={}{} — {}",
                    events.len(),
                    result.handled.len(),
                    result.cards_to_work.len(),
                    result.discussions_to_run.len(),
                    cost(cost_usd),
                    result.summary
                );
                self.emit("triage", "ok", &detail);
                Some(result)
            }
            Err(error) => {
                self.emit("triage", "failed", &one_line(&error));
                None
            }
        }
    }

    async fn try_triage(&self, events: &[Event]) -> Result<(TriageResult, Option<f64>), String> {
        let runner = self.name().to_string();
        let state = self
            .store
            .call(move |s| s.load_state(&runner))
            .await
            .map_err(|e| format!("store: {e}"))?;
        let prompt = triage_prompt(
            &self.config,
            &self.paths,
            &state.cursors,
            events,
            self.now(),
        );
        let triage = &self.config.triage;
        let request = self.request(
            Mode::Triage,
            prompt,
            (&triage.model, triage.effort, triage.max_budget_usd),
            self.state_dir(),
            triage_schema(),
        );
        let outcome = self
            .session
            .run(request, self.shutdown.subscribe())
            .await
            .map_err(|e| e.0)?;
        let result: TriageResult = serde_json::from_value(outcome.output)
            .map_err(|e| format!("invalid triage result: {e}"))?;
        // The pollers' own cursors are never the skill's to change.
        let cursors: BTreeMap<String, String> = result
            .cursors
            .iter()
            .filter(|(key, _)| !key.starts_with(INTAKE_CURSOR_PREFIX))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let runner = self.name().to_string();
        let saved = self
            .store
            .call(move |s| s.set_cursors(&runner, &cursors))
            .await;
        // The session already acted (maybe replied); failing the batch would repeat that.
        if let Err(error) = saved {
            self.emit(
                "triage",
                "failed",
                &one_line(&format!("cursors not saved — store: {error}")),
            );
        }
        Ok((result, outcome.cost_usd))
    }

    pub(crate) fn batch_window(&self) -> Duration {
        self.config.intake.batch_window
    }
}

#[cfg(test)]
mod tests;
