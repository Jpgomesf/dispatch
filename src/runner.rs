//! Heartbeat triage + card execution. Triage and each card run as their own tokio tasks; one
//! coordinator loop starts them, so triage keeps its cadence while cards are running.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, sleep, sleep_until};

use crate::config::{Config, Effort, UnknownWorkspace, Workspace};
use crate::paths::Paths;
use crate::prompts::{card_prompt, heartbeat_prompt};
use crate::results::{
    CardOutcome, CardResult, CardToWork, HeartbeatResult, card_schema, heartbeat_schema,
};
use crate::session::{Session, SessionRequest, Shutdown};
use crate::state::{CardStatus, State, StateStore};
use crate::worktree;

pub const BACKOFF_BASE: Duration = Duration::from_secs(30);
pub const KILL_SWITCH_POLL: Duration = Duration::from_secs(5);
const MAX_DOUBLINGS: u32 = 16;

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
pub type Output = Arc<dyn Fn(String) + Send + Sync>;

/// Delay before the next triage: the interval after a success, else 30s doubling per
/// consecutive failure, capped at the interval.
#[must_use]
pub fn backoff_delay(failures: u32, interval: Duration) -> Duration {
    if failures == 0 {
        return interval;
    }
    let doublings = (failures - 1).min(MAX_DOUBLINGS);
    interval.min(BACKOFF_BASE * (1u32 << doublings))
}

pub struct Runner<S> {
    pub config: Arc<Config>,
    pub paths: Arc<Paths>,
    session: Arc<S>,
    store: StateStore,
    clock: Clock,
    out: Output,
    shutdown: Arc<watch::Sender<Shutdown>>,
}

impl<S> Clone for Runner<S> {
    fn clone(&self) -> Self {
        Runner {
            config: self.config.clone(),
            paths: self.paths.clone(),
            session: self.session.clone(),
            store: self.store.clone(),
            clock: self.clock.clone(),
            out: self.out.clone(),
            shutdown: self.shutdown.clone(),
        }
    }
}

fn one_line(text: &str) -> String {
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

/// Every `blocked_by` ref is done in state, or unknown to state and not part of the current
/// batch (queued or running) — an external dependency the runner cannot track.
#[must_use]
pub fn is_ready(card: &CardToWork, state: &State, batch: &HashSet<String>) -> bool {
    card.blocked_by
        .iter()
        .all(|blocker| match state.status(blocker) {
            Some(status) => status == CardStatus::Done,
            None => !batch.contains(blocker),
        })
}

async fn wait_triage<T>(handle: &mut Option<JoinHandle<T>>) -> Result<T, JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

enum Event {
    Triaged(Option<HeartbeatResult>),
    CardDone(Id),
    Wake,
}

impl<S: Session> Runner<S> {
    pub fn new(config: Config, paths: Paths, session: S) -> Self {
        let store = StateStore::new(&paths.state_dir);
        Runner {
            config: Arc::new(config),
            paths: Arc::new(paths),
            session: Arc::new(session),
            store,
            clock: Arc::new(Utc::now),
            out: Arc::new(|line| println!("{line}")),
            shutdown: Arc::new(watch::channel(Shutdown::Run).0),
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

    pub fn session(&self) -> &S {
        &self.session
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    /// Signal handlers raise the level; every running session watches it.
    pub fn shutdown_handle(&self) -> Arc<watch::Sender<Shutdown>> {
        self.shutdown.clone()
    }

    #[must_use]
    pub fn killed(&self) -> bool {
        self.paths.kill_switch().exists()
    }

    #[must_use]
    pub fn should_stop(&self) -> bool {
        *self.shutdown.borrow() != Shutdown::Run || self.killed()
    }

    fn begin_graceful_shutdown(&self) {
        self.shutdown.send_if_modified(|level| {
            let starting = *level == Shutdown::Run;
            if starting {
                *level = Shutdown::Graceful;
            }
            starting
        });
    }

    fn emit(&self, kind: &str, status: &str, detail: &str) {
        let now = (self.clock)().format("%Y-%m-%dT%H:%M:%SZ");
        (self.out)(
            format!("{now} {kind} {status} {detail}")
                .trim_end()
                .to_string(),
        );
    }

    fn state_dir(&self) -> PathBuf {
        let _ = std::fs::create_dir_all(&self.paths.state_dir);
        self.paths.state_dir.clone()
    }

    fn request(
        &self,
        prompt: String,
        (model, effort, max_budget_usd): (&str, Effort, f64),
        cwd: PathBuf,
        output_schema: serde_json::Value,
    ) -> SessionRequest {
        SessionRequest {
            prompt,
            model: model.to_string(),
            effort,
            max_budget_usd,
            cwd,
            plugin_dir: self.paths.plugin_dir.clone(),
            output_schema,
        }
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
        Ok(self.run_card_in(card_ref, workspace).await)
    }

    async fn run_card_in(
        &self,
        card_ref: &str,
        workspace: Option<Workspace>,
    ) -> Option<CardResult> {
        let started = (self.clock)();
        let owned_ref = card_ref.to_string();
        // Compare-and-set under the state lock: never start a card that is already running.
        let claimed = self
            .store
            .update_async(move |state| {
                if state.in_progress(&owned_ref) {
                    return false;
                }
                state.set_card(&owned_ref, CardStatus::InProgress, started, None);
                true
            })
            .await;
        let worked = match claimed {
            Ok(true) => self.work_card(card_ref, workspace.as_ref()).await,
            Ok(false) => {
                self.emit(
                    "card",
                    "skipped",
                    &format!("{card_ref} — already in progress"),
                );
                return None;
            }
            Err(error) => Err(format!("state: {error:#}")),
        };
        match worked {
            Err(error) => {
                let _ = self.record_card(card_ref, CardStatus::Failed, None).await;
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
                if let Err(error) = self.record_card(card_ref, status, pr_url).await {
                    self.emit("card", "failed", &format!("{card_ref} — state: {error:#}"));
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
    ) -> anyhow::Result<()> {
        let now = (self.clock)();
        let card_ref = card_ref.to_string();
        self.store
            .update_async(move |state| state.set_card(&card_ref, status, now, pr_url))
            .await
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
            (self.clock)(),
        );
        let card = &self.config.card;
        let request = self.request(
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

    pub async fn triage(&self) -> Option<HeartbeatResult> {
        match self.try_triage().await {
            Ok((result, cost_usd)) => {
                let detail = format!(
                    "handled={} cards={}{} — {}",
                    result.handled.len(),
                    result.cards_to_work.len(),
                    cost(cost_usd),
                    result.summary
                );
                self.emit("heartbeat", "ok", &detail);
                Some(result)
            }
            Err(error) => {
                self.emit("heartbeat", "failed", &one_line(&error));
                None
            }
        }
    }

    async fn try_triage(&self) -> Result<(HeartbeatResult, Option<f64>), String> {
        let state = self
            .store
            .load_async()
            .await
            .map_err(|e| format!("state: {e:#}"))?;
        let prompt = heartbeat_prompt(&self.config, &self.paths, &state.cursors, (self.clock)());
        let hb = &self.config.heartbeat;
        let request = self.request(
            prompt,
            (&hb.model, hb.effort, hb.max_budget_usd),
            self.state_dir(),
            heartbeat_schema(),
        );
        let outcome = self
            .session
            .run(request, self.shutdown.subscribe())
            .await
            .map_err(|e| e.0)?;
        let result: HeartbeatResult = serde_json::from_value(outcome.output)
            .map_err(|e| format!("invalid heartbeat result: {e}"))?;
        let cursors = result.cursors.clone();
        self.store
            .update_async(move |state| state.cursors.extend(cursors))
            .await
            .map_err(|e| format!("state: {e:#}"))?;
        Ok((result, outcome.cost_usd))
    }

    /// Cards left `in_progress` by a crashed run become `failed`, so triage can pick them up.
    pub async fn release_stale_cards(&self) {
        let now = (self.clock)();
        let released = self.store.update_async(move |state| {
            let stale: Vec<String> = state
                .cards
                .keys()
                .filter(|r| state.in_progress(r))
                .cloned()
                .collect();
            for card_ref in stale {
                state.set_card(&card_ref, CardStatus::Failed, now, None);
            }
        });
        if let Err(error) = released.await {
            self.emit("heartbeat", "failed", &format!("state: {error:#}"));
        }
    }

    pub async fn heartbeat(&self, interval: Duration, once: bool) {
        if !self.should_stop() {
            self.release_stale_cards().await;
        }
        self.schedule(interval, once).await;
    }

    /// Triage replaces the queue with its latest list: cards it no longer lists are dropped,
    /// cards already queued keep their place (with fresh `blocked_by`), running or
    /// `in_progress` cards are skipped, and at most `max_cards_per_tick` new cards join.
    fn merge_queue(
        &self,
        queue: &mut Vec<CardToWork>,
        listed: Vec<CardToWork>,
        running: &HashSet<String>,
        state: &State,
    ) {
        let limit = self.config.heartbeat.max_cards_per_tick as usize;
        let mut seen = HashSet::new();
        let mut added = 0;
        let mut next = Vec::new();
        for card in listed {
            let card_ref = card.card_ref.clone();
            if !seen.insert(card_ref.clone())
                || running.contains(&card_ref)
                || state.in_progress(&card_ref)
            {
                continue;
            }
            if queue.iter().any(|q| q.card_ref == card_ref) {
                next.push(card);
            } else if added < limit {
                next.push(card);
                added += 1;
            }
        }
        *queue = next;
    }

    fn has_free_slot(&self, queue: &[CardToWork], running: &HashMap<Id, String>) -> bool {
        !queue.is_empty()
            && running.len() < self.config.card.max_parallel as usize
            && !self.should_stop()
    }

    fn start_ready(
        &self,
        queue: &mut Vec<CardToWork>,
        cards: &mut JoinSet<()>,
        running: &mut HashMap<Id, String>,
        state: &State,
    ) {
        let max_parallel = self.config.card.max_parallel as usize;
        let batch: HashSet<String> = queue
            .iter()
            .map(|c| c.card_ref.clone())
            .chain(running.values().cloned())
            .collect();
        let mut index = 0;
        while index < queue.len() && running.len() < max_parallel {
            if !is_ready(&queue[index], state, &batch) {
                index += 1;
                continue;
            }
            let card = queue.remove(index);
            let workspace = self.config.workspace_for(&card.card_ref).cloned();
            let me = self.clone();
            let card_ref = card.card_ref.clone();
            let handle = cards.spawn(async move {
                me.run_card_in(&card_ref, workspace).await;
            });
            running.insert(handle.id(), card.card_ref);
        }
    }

    /// State for a scheduling decision. On failure, prints one line (once per distinct error,
    /// since the loop retries every few seconds) and returns `None` so the caller skips.
    async fn load_for_scheduling(&self, last_error: &mut Option<String>) -> Option<State> {
        match self.store.load_async().await {
            Ok(state) => {
                *last_error = None;
                Some(state)
            }
            Err(error) => {
                let message = one_line(&format!("state: {error:#}; scheduling skipped"));
                if last_error.as_deref() != Some(message.as_str()) {
                    self.emit("heartbeat", "failed", &message);
                    *last_error = Some(message);
                }
                None
            }
        }
    }

    /// The coordinator loop. `once`: one triage, then run its cards (respecting
    /// `blocked_by` and `max_parallel`) and return; cards whose blockers never finish are
    /// left for the next run.
    pub async fn schedule(&self, interval: Duration, once: bool) {
        let mut queue: Vec<CardToWork> = Vec::new();
        let mut cards: JoinSet<()> = JoinSet::new();
        let mut running: HashMap<Id, String> = HashMap::new();
        let mut triage: Option<JoinHandle<Option<HeartbeatResult>>> = None;
        let mut triaged = false;
        let mut failures = 0u32;
        let mut next_triage = Instant::now();
        let mut shutdown = self.shutdown.subscribe();
        let mut state_error: Option<String> = None;

        loop {
            if self.should_stop() {
                self.begin_graceful_shutdown();
                break;
            }
            let triage_allowed = !(once && triaged);
            if triage.is_none() && triage_allowed && Instant::now() >= next_triage {
                let me = self.clone();
                triage = Some(tokio::spawn(async move { me.triage().await }));
            }
            if self.has_free_slot(&queue, &running) {
                // An unreadable state file skips this tick's scheduling; never an empty state.
                if let Some(state) = self.load_for_scheduling(&mut state_error).await {
                    self.start_ready(&mut queue, &mut cards, &mut running, &state);
                }
            }
            if once && triaged && triage.is_none() && cards.is_empty() {
                break;
            }
            let triage_idle = triage.is_none() && triage_allowed;
            let event = tokio::select! {
                joined = wait_triage(&mut triage) => Event::Triaged(joined.ok().flatten()),
                Some(done) = cards.join_next_with_id(), if !cards.is_empty() => {
                    Event::CardDone(match done {
                        Ok((id, ())) => id,
                        Err(error) => error.id(),
                    })
                }
                () = sleep_until(next_triage), if triage_idle => Event::Wake,
                () = sleep(KILL_SWITCH_POLL) => Event::Wake,
                _ = shutdown.changed() => Event::Wake,
            };
            match event {
                Event::Triaged(result) => {
                    triage = None;
                    triaged = true;
                    failures = if result.is_some() { 0 } else { failures + 1 };
                    next_triage = Instant::now() + backoff_delay(failures, interval);
                    if let Some(result) = result
                        && let Some(state) = self.load_for_scheduling(&mut state_error).await
                    {
                        let busy: HashSet<String> = running.values().cloned().collect();
                        self.merge_queue(&mut queue, result.cards_to_work, &busy, &state);
                    }
                }
                Event::CardDone(id) => {
                    running.remove(&id);
                }
                Event::Wake => {}
            }
        }

        if let Some(handle) = triage.take() {
            let _ = handle.await;
        }
        while cards.join_next().await.is_some() {}
        if self.killed() {
            self.emit("heartbeat", "stopped", "kill switch present");
        }
    }
}

#[cfg(test)]
mod tests;
