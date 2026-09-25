//! The coordinator loop: starts triage (on a batch of intake events, or as the fallback
//! sweep) and card / discussion sessions as separate tokio tasks, so triage keeps its
//! cadence while sessions run. Intake sources run beside it as their own tasks. Cards whose
//! retry is due are queued by the runner itself; triage never restarts a card that awaits a
//! retry or needs a person.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, sleep, sleep_until};

use crate::intake::{self, Event};
use crate::results::{CardToWork, DiscussionToRun, TriageResult};
use crate::runner::{QueuedDiscussion, Runner, one_line, time};
use crate::session::Session;
use crate::state::{CardStatus, State};

pub const BACKOFF_BASE: Duration = Duration::from_secs(30);
pub const KILL_SWITCH_POLL: Duration = Duration::from_secs(5);
const MAX_DOUBLINGS: u32 = 16;

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

/// `base` plus up to half of it again, picked by `random`: spreads session starts so a
/// burst (several runners, or all of them right after a pause) does not hit the API at once.
#[must_use]
pub fn jittered(base: Duration, random: u64) -> Duration {
    let millis = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let extra = millis.saturating_mul(random % 501) / 1000;
    Duration::from_millis(millis.saturating_add(extra))
}

/// A random number from the standard library's per-process hasher keys; enough for jitter.
fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// `text` mentions `card_ref` as a whole token: `EX-1` is not in `EX-12` or `PREX-1`.
#[must_use]
pub fn mentions_ref(text: &str, card_ref: &str) -> bool {
    !card_ref.is_empty()
        && text.match_indices(card_ref).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + card_ref.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
}

async fn wait_triage<T>(handle: &mut Option<JoinHandle<T>>) -> Result<T, JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

/// What a finished session task hands back.
enum Finished {
    Card,
    /// A discussion to queue again, when the retry rules say so.
    Discussion(Option<QueuedDiscussion>),
}

enum Wake {
    /// `None`: the triage failed or its task panicked.
    Triaged(Option<TriageResult>),
    /// `None` when the task panicked.
    SessionDone(Id, Option<Finished>),
    Tick,
}

/// What a running session holds a slot for.
enum Slot {
    Card(String),
    Discussion(String),
}

/// Pending intake events: a window opens when the first `new` event is seen and a triage
/// takes them all when it closes. Failed batches wait out a backoff.
#[derive(Default)]
struct Batching {
    window_closes: Option<Instant>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl Batching {
    fn observe(&mut self, pending: bool, window: Duration) {
        if !pending {
            self.window_closes = None;
        } else if self.window_closes.is_none() {
            let opens = Instant::now() + window;
            self.window_closes = Some(self.retry_at.map_or(opens, |retry| retry.max(opens)));
        }
    }

    #[must_use]
    fn due(&self) -> bool {
        self.window_closes.is_some_and(|at| Instant::now() >= at)
    }
}

struct Loop {
    queue: Vec<CardToWork>,
    discussions: VecDeque<QueuedDiscussion>,
    sessions: JoinSet<Finished>,
    running: HashMap<Id, Slot>,
    triage: Option<JoinHandle<Option<TriageResult>>>,
    /// Event ids handed to the running triage; finished from here whatever the task returns.
    in_flight: Vec<i64>,
    /// Every card started in this loop. One that ended without a recorded status (refused,
    /// held by another runner) keeps holding its dependents instead of looking external.
    started_cards: HashSet<String>,
    triaged: bool,
    sweep_failures: u32,
    next_sweep: Instant,
    batching: Batching,
    state_error: Option<String>,
    /// The end of the machine-wide pause last seen, to print one line per change.
    pause_seen: Option<DateTime<Utc>>,
    /// No session starts before this (`[sessions] start_stagger` after the previous one).
    next_start: Instant,
}

impl Loop {
    #[must_use]
    fn may_start(&self) -> bool {
        Instant::now() >= self.next_start
    }

    fn running_cards(&self) -> HashSet<String> {
        self.running
            .values()
            .filter_map(|slot| match slot {
                Slot::Card(card_ref) => Some(card_ref.clone()),
                Slot::Discussion(_) => None,
            })
            .collect()
    }
}

impl<S: Session> Runner<S> {
    pub async fn heartbeat(&self, interval: Duration, once: bool) {
        if !self.should_stop() {
            self.recover().await;
        }
        self.schedule(interval, once).await;
    }

    /// This runner's cards and cursors for a scheduling decision. On failure, prints one line
    /// (once per distinct error, since the loop retries every few seconds) and returns `None`
    /// so the caller skips.
    async fn load_for_scheduling(&self, last_error: &mut Option<String>) -> Option<State> {
        let runner = self.name().to_string();
        match self.store().call(move |s| s.load_state(&runner)).await {
            Ok(state) => {
                *last_error = None;
                Some(state)
            }
            Err(error) => {
                let message = one_line(&format!("store: {error}; scheduling skipped"));
                if last_error.as_deref() != Some(message.as_str()) {
                    self.emit("triage", "failed", &message);
                    *last_error = Some(message);
                }
                None
            }
        }
    }

    async fn pending_events(&self) -> bool {
        let runner = self.name().to_string();
        matches!(
            self.store().call(move |s| s.new_event_count(&runner)).await,
            Ok(count) if count > 0
        )
    }

    /// Triage replaces the queue with its latest list: cards it no longer lists are dropped,
    /// cards already queued keep their place (with fresh `blocked_by`), running,
    /// `in_progress`, `needs_human` and retry-pending cards are skipped, and at most
    /// `max_cards_per_tick` new cards join.
    fn merge_queue(
        &self,
        queue: &mut Vec<CardToWork>,
        listed: Vec<CardToWork>,
        running: &HashSet<String>,
        state: &State,
    ) {
        let limit = self.config.triage.max_cards_per_tick as usize;
        let now = self.now();
        let mut seen = HashSet::new();
        let mut added = 0;
        let mut next = Vec::new();
        for card in listed {
            let card_ref = card.card_ref.clone();
            if !seen.insert(card_ref.clone())
                || running.contains(&card_ref)
                || state.in_progress(&card_ref)
                || state.held_back(&card_ref, now)
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

    /// Cards whose retry is due join the queue with the `blocked_by` they last started with;
    /// they do not count against `max_cards_per_tick`.
    fn queue_due_retries(&self, work: &mut Loop, state: &State) {
        let now = self.now();
        let running = work.running_cards();
        for (card_ref, card) in &state.cards {
            let known =
                running.contains(card_ref) || work.queue.iter().any(|q| &q.card_ref == card_ref);
            if card.retry_due(now) && !known {
                work.queue.push(CardToWork {
                    card_ref: card_ref.clone(),
                    blocked_by: card.blocked_by.clone(),
                });
            }
        }
    }

    /// Discussions are one-off requests: appended (deduplicated by claim key), never replaced.
    fn merge_discussions(
        discussions: &mut VecDeque<QueuedDiscussion>,
        listed: Vec<DiscussionToRun>,
        running: &HashMap<Id, Slot>,
    ) {
        for discussion in listed {
            let key = discussion.claim_key();
            let known = discussions.iter().any(|d| d.discussion.claim_key() == key)
                || running
                    .values()
                    .any(|slot| matches!(slot, Slot::Discussion(k) if *k == key));
            if !known {
                discussions.push_back(QueuedDiscussion::new(discussion));
            }
        }
    }

    /// Discussions first (someone is waiting for an answer), then ready cards.
    fn start_ready(&self, work: &mut Loop, state: &State) {
        let max_parallel = self.config.card.max_parallel as usize;
        let now = self.now();
        while work.running.len() < max_parallel && work.may_start() {
            let Some(index) = work.discussions.iter().position(|d| d.ready(now)) else {
                break;
            };
            let Some(queued) = work.discussions.remove(index) else {
                break;
            };
            let key = queued.discussion.claim_key();
            let me = self.clone();
            let handle = work
                .sessions
                .spawn(async move { Finished::Discussion(me.attempt_discussion(queued).await.1) });
            work.running.insert(handle.id(), Slot::Discussion(key));
            self.stagger(work);
        }
        let batch: HashSet<String> = work
            .queue
            .iter()
            .map(|c| c.card_ref.clone())
            .chain(work.running_cards())
            .chain(work.started_cards.iter().cloned())
            .collect();
        let mut index = 0;
        while index < work.queue.len() && work.running.len() < max_parallel && work.may_start() {
            if !is_ready(&work.queue[index], state, &batch) {
                index += 1;
                continue;
            }
            let card = work.queue.remove(index);
            let workspace = self.config.workspace_for(&card.card_ref).cloned();
            let me = self.clone();
            let card_ref = card.card_ref.clone();
            work.started_cards.insert(card_ref.clone());
            let handle = work.sessions.spawn(async move {
                me.run_card_in(&card.card_ref, workspace, &card.blocked_by)
                    .await;
                Finished::Card
            });
            work.running.insert(handle.id(), Slot::Card(card_ref));
            self.stagger(work);
        }
    }

    /// A session just started: the next one waits `start_stagger` plus jitter.
    fn stagger(&self, work: &mut Loop) {
        work.next_start = Instant::now() + jittered(self.config.sessions.start_stagger, random());
    }

    /// Whether the machine-wide pause holds new sessions back now; one line when another
    /// runner's pause is first seen and when it lifts.
    async fn hold_for_pause(&self, work: &mut Loop) -> bool {
        let pause = self.paused().await;
        match (&pause, work.pause_seen) {
            (Some(pause), seen) if seen != Some(pause.until) => {
                if pause.runner != self.name() {
                    let detail = format!(
                        "until {} — {} (set by {})",
                        time(pause.until),
                        pause.reason,
                        pause.runner
                    );
                    self.emit("pause", "waiting", &detail);
                }
                work.pause_seen = Some(pause.until);
            }
            (None, Some(_)) => {
                self.emit("pause", "lifted", "");
                work.pause_seen = None;
                // Every runner sees the pause end at once; each waits its own jittered
                // stagger before starting again.
                self.stagger(work);
            }
            _ => {}
        }
        pause.is_some()
    }

    /// A new event that mentions a `needs_human` card is the external change it waited for:
    /// its attempt count starts over and triage may list it again.
    async fn reopen_mentioned(&self, events: &[Event]) {
        let texts: Vec<String> = events
            .iter()
            .map(|event| {
                format!(
                    "{} {}",
                    event.sender.as_deref().unwrap_or(""),
                    event.payload
                )
            })
            .collect();
        let (runner, now) = (self.name().to_string(), self.now());
        let reopened = self
            .store()
            .call(move |s| {
                let state = s.load_state(&runner)?;
                let mut reopened = Vec::new();
                for (card_ref, card) in &state.cards {
                    let mentioned = texts.iter().any(|text| mentions_ref(text, card_ref));
                    if card.status == CardStatus::NeedsHuman
                        && mentioned
                        && s.reset_card(&runner, card_ref, true, now)?
                    {
                        reopened.push(card_ref.clone());
                    }
                }
                Ok(reopened)
            })
            .await;
        match reopened {
            Ok(refs) => {
                for card_ref in refs {
                    self.emit(
                        "card",
                        "reopened",
                        &format!("{card_ref} — a new event mentions it"),
                    );
                }
            }
            Err(error) => self.emit("triage", "failed", &one_line(&format!("store: {error}"))),
        }
    }

    /// Start a triage if one is due: a closed batch window takes every `new` event; else the
    /// fallback sweep runs with none. `once`: the single triage takes pending events at once.
    async fn start_triage(&self, work: &mut Loop, once: bool) {
        let events_due = work.batching.due() || (once && self.pending_events().await);
        let sweep_due = Instant::now() >= work.next_sweep;
        if !events_due && !sweep_due {
            return;
        }
        let events = if events_due {
            work.batching.window_closes = None;
            let runner = self.name().to_string();
            match self.store().call(move |s| s.take_batch(&runner)).await {
                Ok(events) => events,
                Err(error) => {
                    // The window reopens on a later tick; a due sweep still runs now.
                    self.emit("triage", "failed", &one_line(&format!("store: {error}")));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        if events.is_empty() && !sweep_due {
            return;
        }
        if !events.is_empty() {
            self.reopen_mentioned(&events).await;
        }
        work.in_flight = events.iter().map(|e| e.id).collect();
        let me = self.clone();
        work.triage = Some(tokio::spawn(async move { me.triage(events).await }));
        self.stagger(work);
    }

    /// Batched events become `done` on success, else go back to `new`. A store failure leaves
    /// them `batched` until the next start (`recover`), with one line.
    async fn finish_events(&self, ids: Vec<i64>, succeeded: bool) {
        let runner = self.name().to_string();
        let finished = self
            .store()
            .call(move |s| s.finish_batch(&runner, &ids, succeeded))
            .await;
        if let Err(error) = finished {
            self.emit("triage", "failed", &one_line(&format!("store: {error}")));
        }
    }

    async fn finish_triage(
        &self,
        work: &mut Loop,
        result: Option<TriageResult>,
        interval: Duration,
    ) {
        work.triage = None;
        work.triaged = true;
        let events = std::mem::take(&mut work.in_flight);
        let succeeded = result.is_some();
        if events.is_empty() {
            work.sweep_failures = if succeeded {
                0
            } else {
                work.sweep_failures + 1
            };
            work.next_sweep = Instant::now() + backoff_delay(work.sweep_failures, interval);
        } else {
            // Every batched event is done once triage succeeds, whether or not `handled`
            // names it (a discussion it started is enough); on failure all go back to `new`.
            self.finish_events(events, succeeded).await;
            let batching = &mut work.batching;
            batching.failures = if succeeded { 0 } else { batching.failures + 1 };
            batching.retry_at =
                (!succeeded).then(|| Instant::now() + backoff_delay(batching.failures, interval));
        }
        if let Some(result) = result
            && let Some(state) = self.load_for_scheduling(&mut work.state_error).await
        {
            let busy = work.running_cards();
            self.merge_queue(&mut work.queue, result.cards_to_work, &busy, &state);
            Self::merge_discussions(
                &mut work.discussions,
                result.discussions_to_run,
                &work.running,
            );
        }
    }

    /// The coordinator loop. `once`: one triage (over pending events, else a sweep), then
    /// its cards and discussions, and any retry already due (respecting `blocked_by` and
    /// `max_parallel`), and return; cards whose blockers never finish, and retries not yet
    /// due, are left for the next run. Intake sources only run in the long-lived loop.
    pub async fn schedule(&self, interval: Duration, once: bool) {
        let mut work = Loop {
            queue: Vec::new(),
            discussions: VecDeque::new(),
            sessions: JoinSet::new(),
            running: HashMap::new(),
            triage: None,
            in_flight: Vec::new(),
            started_cards: HashSet::new(),
            triaged: false,
            sweep_failures: 0,
            next_sweep: Instant::now(),
            batching: Batching::default(),
            state_error: None,
            pause_seen: None,
            next_start: Instant::now(),
        };
        let mut sources = if once || self.should_stop() {
            JoinSet::new()
        } else {
            intake::spawn_sources(&self.intake_context())
        };
        let mut shutdown = self.subscribe_shutdown();
        let wake = self.wake.clone();
        let max_parallel = self.config.card.max_parallel as usize;

        loop {
            if self.should_stop() {
                self.begin_graceful_shutdown();
                break;
            }
            let triage_allowed = !(once && work.triaged);
            if !once {
                let pending = self.pending_events().await;
                work.batching.observe(pending, self.batch_window());
            }
            // After a usage limit nothing new starts until the pause ends; running sessions
            // carry on.
            let paused = self.hold_for_pause(&mut work).await;
            if !paused && work.may_start() && work.triage.is_none() && triage_allowed {
                self.start_triage(&mut work, once).await;
            }
            // An unreadable store skips this tick's scheduling; never an empty state.
            if !paused
                && work.running.len() < max_parallel
                && let Some(state) = self.load_for_scheduling(&mut work.state_error).await
            {
                self.queue_due_retries(&mut work, &state);
                self.start_ready(&mut work, &state);
            }
            // Past the stagger anything ready has just been started, so once nothing runs there
            // is nothing left to wait for; with nothing queued there is nothing to stagger.
            let idle = work.triage.is_none() && work.sessions.is_empty();
            let nothing_queued = work.queue.is_empty() && work.discussions.is_empty();
            if once && idle && (paused || (work.triaged && (work.may_start() || nothing_queued))) {
                if let (false, Some(until)) = (work.triaged, work.pause_seen) {
                    self.emit(
                        "triage",
                        "skipped",
                        &format!("paused until {}", time(until)),
                    );
                }
                break;
            }
            // Only when a triage could start: an overdue sweep or window would otherwise fire at
            // once on every pass while paused or staggered. The pause is polled every
            // `KILL_SWITCH_POLL`; the stagger has its own wake-up.
            let triage_idle =
                work.triage.is_none() && triage_allowed && !paused && work.may_start();
            let window = work.batching.window_closes;
            let event = tokio::select! {
                joined = wait_triage(&mut work.triage) => Wake::Triaged(joined.ok().flatten()),
                Some(done) = work.sessions.join_next_with_id(), if !work.sessions.is_empty() => {
                    match done {
                        Ok((id, finished)) => Wake::SessionDone(id, Some(finished)),
                        Err(error) => Wake::SessionDone(error.id(), None),
                    }
                }
                () = sleep_until(work.next_sweep), if triage_idle => Wake::Tick,
                () = sleep_until(window.unwrap_or_else(Instant::now)), if triage_idle && window.is_some() => Wake::Tick,
                () = sleep_until(work.next_start), if !work.may_start() => Wake::Tick,
                () = wake.notified() => Wake::Tick,
                () = sleep(KILL_SWITCH_POLL) => Wake::Tick,
                _ = shutdown.changed() => Wake::Tick,
            };
            match event {
                Wake::Triaged(result) => self.finish_triage(&mut work, result, interval).await,
                Wake::SessionDone(id, finished) => {
                    work.running.remove(&id);
                    if let Some(Finished::Discussion(Some(retry))) = finished {
                        work.discussions.push_back(retry);
                    }
                }
                Wake::Tick => {}
            }
        }

        sources.abort_all();
        if let Some(handle) = work.triage.take() {
            let succeeded = handle.await.is_ok_and(|result| result.is_some());
            let ids = std::mem::take(&mut work.in_flight);
            if !ids.is_empty() {
                self.finish_events(ids, succeeded).await;
            }
        }
        while work.sessions.join_next().await.is_some() {}
        if self.killed() {
            self.emit("triage", "stopped", "kill switch present");
        }
    }
}

#[cfg(test)]
mod tests;
