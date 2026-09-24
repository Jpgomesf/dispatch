//! Sessions: triage, cards and discussions, each one `claude` process recorded as an attempt.
//! Scheduling (when they start, batching of intake events, parallelism) lives in
//! `coordinator`; what follows an attempt is decided by the rules in `outcome`.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Instant, interval_at};

use crate::config::{Config, Effort};
use crate::intake::secrets::{EnvLookup, process_env};
use crate::intake::{Event, IntakeContext};
use crate::outcome::{Next, Outcome};
use crate::paths::Paths;
use crate::prompts::{Escalation, INTAKE_CURSOR_PREFIX, triage_prompt};
use crate::results::{TriageResult, triage_schema};
use crate::session::{
    Control, Ended, Limits, Mode, Notice, Session, SessionReport, SessionRequest, Shutdown,
};
use crate::state::CardStatus;
use crate::store::{AttemptEnd, CLAIM_LEASE, CLAIM_RENEW, Claim, Store};

mod cards;
mod discussions;

pub(crate) use discussions::QueuedDiscussion;

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
pub type Output = Arc<dyn Fn(String) + Send + Sync>;

/// Summary of an attempt a stopped runner left open.
const STOPPED_MID_SESSION: &str = "the runner stopped before the session ended";

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

pub(crate) fn time(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// What follows an attempt, for its output line.
fn describe(next: &Result<Next, String>) -> String {
    match next {
        Ok(Next::Finished | Next::Blocked) => String::new(),
        Ok(Next::Retry { at }) => format!("; retry at {}", time(*at)),
        Ok(Next::NeedsHuman(reason)) => format!("; needs_human ({})", reason.as_str()),
        Err(error) => format!("; not recorded ({error})"),
    }
}

/// How an attempt ended: the agent's result when its structured output parses as `T` (with
/// the outcome and summary `verdict` reads from it), else the class of the failure.
fn judge<T: DeserializeOwned>(
    ended: &Ended,
    what: &str,
    verdict: impl Fn(&T) -> (Outcome, String),
) -> (Outcome, String, Option<T>) {
    match ended.output() {
        Ok(output) => match serde_json::from_value::<T>(output) {
            Ok(result) => {
                let (outcome, summary) = verdict(&result);
                (outcome, summary, Some(result))
            }
            Err(e) => (
                Outcome::InvalidOutput,
                format!("invalid {what} result: {e}"),
                None,
            ),
        },
        Err(detail) => (
            Outcome::of_ended(ended).unwrap_or(Outcome::Crash),
            detail,
            None,
        ),
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
        (self.out)(
            format!("{} {kind} {status} {detail}", time(self.now()))
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

    /// The mode's wall-clock timeout and the shared session limits.
    fn limits(&self, mode: Mode) -> Limits {
        let timeout = match mode {
            Mode::Triage => self.config.triage.timeout,
            Mode::Card => self.config.card.timeout,
            Mode::Discussion => self.config.discussion.timeout,
        };
        Limits {
            timeout,
            idle_timeout: self.config.sessions.idle_timeout,
        }
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
            limits: self.limits(mode),
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

    /// At startup, under the instance lock (no other live process of this runner): attempts
    /// left open are closed as `crash`, cards left `in_progress` follow the retry rules for a
    /// crash, this runner's claims are released, and batches a crashed triage held go back to
    /// `new`.
    pub async fn recover(&self) {
        let (runner, now) = (self.name().to_string(), self.now());
        let recovered = self
            .store
            .call(move |store| {
                let open = store.open_attempts(&runner)?;
                store.close_open_attempts(
                    &runner,
                    Outcome::Crash.as_str(),
                    STOPPED_MID_SESSION,
                    now,
                )?;
                store.release_all(&runner)?;
                store.requeue_batched(&runner)?;
                let state = store.load_state(&runner)?;
                let stranded: Vec<(String, Option<i64>)> = state
                    .cards
                    .iter()
                    .filter(|(_, card)| card.status == CardStatus::InProgress)
                    .map(|(card_ref, _)| {
                        let attempt = open
                            .iter()
                            .rfind(|a| a.mode == Mode::Card.as_str() && &a.reference == card_ref);
                        (card_ref.clone(), attempt.map(|a| a.id))
                    })
                    .collect();
                Ok(stranded)
            })
            .await;
        match recovered {
            Err(error) => self.emit("triage", "failed", &format!("store: {error}")),
            Ok(stranded) => {
                for (card_ref, attempt) in stranded {
                    let next = self
                        .settle_card(&card_ref, attempt, Outcome::Crash, None, None)
                        .await;
                    let detail = format!("{card_ref} — {STOPPED_MID_SESSION}{}", describe(&next));
                    self.emit("card", Outcome::Crash.as_str(), &detail);
                }
            }
        }
    }

    /// Open this session's attempt row. A store that cannot record it keeps the session from
    /// starting, like one that cannot record the card.
    async fn begin_attempt(
        &self,
        mode: Mode,
        reference: &str,
        cwd: &Path,
    ) -> Result<(i64, u32), String> {
        let (runner, reference, cwd, now) = (
            self.name().to_string(),
            reference.to_string(),
            cwd.to_path_buf(),
            self.now(),
        );
        self.store
            .call(move |s| s.begin_attempt(&runner, mode.as_str(), &reference, &cwd, now))
            .await
            .map_err(|e| format!("store: {e}"))
    }

    /// Run one session for attempt `id`, acting on its notices while it runs.
    async fn run_session(&self, request: SessionRequest, id: i64) -> SessionReport {
        let (notices, mut received) = mpsc::unbounded_channel();
        let control = Control {
            shutdown: self.shutdown.subscribe(),
            notices,
        };
        let mut run = pin!(self.session.run(request, control));
        loop {
            tokio::select! {
                report = &mut run => {
                    while let Ok(notice) = received.try_recv() {
                        self.on_notice(id, notice).await;
                    }
                    return report;
                }
                Some(notice) = received.recv() => self.on_notice(id, notice).await,
            }
        }
    }

    /// Best effort: a notice never fails the session.
    async fn on_notice(&self, id: i64, notice: Notice) {
        match notice {
            Notice::Started { session_id } => {
                let _ = self
                    .store
                    .call(move |s| s.set_attempt_session(id, &session_id))
                    .await;
            }
        }
    }

    /// How an attempt ended, as recorded; callers add `blocked_on` and `new_commits`.
    fn attempt_end(&self, outcome: Outcome, report: &SessionReport, summary: &str) -> AttemptEnd {
        AttemptEnd {
            outcome: outcome.as_str().to_string(),
            summary: one_line(summary),
            blocked_on: None,
            session_id: report.session_id.clone(),
            cost_usd: report.cost_usd,
            new_commits: None,
            ended_at: self.now(),
        }
    }

    /// Close an attempt row with how the session ended. Best effort: the session already ran,
    /// so a store failure costs only the record (one line).
    async fn end_attempt(&self, id: i64, end: AttemptEnd) {
        if let Err(error) = self.store.call(move |s| s.end_attempt(id, &end)).await {
            self.emit(
                "store",
                "failed",
                &one_line(&format!("attempt {id} not recorded — {error}")),
            );
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

    /// One triage session over `events` (empty: a fallback sweep). Cursors are persisted as
    /// soon as it returns. A failed triage is not retried here: its events go back to `new`
    /// and the coordinator backs off.
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
                self.emit("triage", Outcome::Ok.as_str(), &detail);
                Some(result)
            }
            Err((outcome, detail)) => {
                self.emit("triage", outcome.as_str(), &one_line(&detail));
                None
            }
        }
    }

    /// Cards waiting for a person, with the last attempt's summary.
    fn escalations(store: &Store, runner: &str) -> crate::store::Result<Vec<Escalation>> {
        let state = store.load_state(runner)?;
        let mut escalations = Vec::new();
        for (card_ref, card) in &state.cards {
            if card.status != CardStatus::NeedsHuman {
                continue;
            }
            let last = store.attempts(runner, Some(card_ref), 1)?;
            escalations.push(Escalation {
                card_ref: card_ref.clone(),
                reason: card.reason.clone().unwrap_or_default(),
                attempts: card.attempts,
                last_summary: last.into_iter().next().and_then(|a| a.summary),
            });
        }
        Ok(escalations)
    }

    async fn try_triage(
        &self,
        events: &[Event],
    ) -> Result<(TriageResult, Option<f64>), (Outcome, String)> {
        let store_failed = |e: crate::store::StoreError| (Outcome::Crash, format!("store: {e}"));
        let runner = self.name().to_string();
        let (state, escalations) = self
            .store
            .call(move |s| Ok((s.load_state(&runner)?, Self::escalations(s, &runner)?)))
            .await
            .map_err(store_failed)?;
        let prompt = triage_prompt(
            &self.config,
            &self.paths,
            &state.cursors,
            events,
            &escalations,
            self.now(),
        );
        let triage = &self.config.triage;
        let cwd = self.state_dir();
        let request = self.request(
            Mode::Triage,
            prompt,
            (&triage.model, triage.effort, triage.max_budget_usd),
            cwd.clone(),
            triage_schema(),
        );
        let (id, _) = self
            .begin_attempt(Mode::Triage, "", &cwd)
            .await
            .map_err(|e| (Outcome::Crash, e))?;
        let report = self.run_session(request, id).await;
        let (outcome, summary, result) = judge(&report.ended, "triage", |r: &TriageResult| {
            (Outcome::Ok, r.summary.clone())
        });
        self.end_attempt(id, self.attempt_end(outcome, &report, &summary))
            .await;
        let result = result.ok_or((outcome, summary))?;
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
        Ok((result, report.cost_usd))
    }

    pub(crate) fn batch_window(&self) -> Duration {
        self.config.intake.batch_window
    }
}

#[cfg(test)]
mod tests;
