//! The coordinator loop: starts triage (on a batch of intake events, or as the fallback
//! sweep) and card / discussion sessions as separate tokio tasks, so triage keeps its
//! cadence while sessions run. Intake sources run beside it as their own tasks.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, sleep, sleep_until};

use crate::intake;
use crate::results::{CardToWork, DiscussionToRun};
use crate::runner::{Runner, Triaged, one_line};
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

async fn wait_triage<T>(handle: &mut Option<JoinHandle<T>>) -> Result<T, JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

enum Wake {
    Triaged(Option<Triaged>),
    SessionDone(Id),
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
    discussions: VecDeque<DiscussionToRun>,
    sessions: JoinSet<()>,
    running: HashMap<Id, Slot>,
    triage: Option<JoinHandle<Triaged>>,
    triaged: bool,
    sweep_failures: u32,
    next_sweep: Instant,
    batching: Batching,
    state_error: Option<String>,
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
    /// cards already queued keep their place (with fresh `blocked_by`), running or
    /// `in_progress` cards are skipped, and at most `max_cards_per_tick` new cards join.
    fn merge_queue(
        &self,
        queue: &mut Vec<CardToWork>,
        listed: Vec<CardToWork>,
        running: &HashSet<String>,
        state: &State,
    ) {
        let limit = self.config.triage.max_cards_per_tick as usize;
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

    /// Discussions are one-off requests: appended (deduplicated by claim key), never replaced.
    fn merge_discussions(
        discussions: &mut VecDeque<DiscussionToRun>,
        listed: Vec<DiscussionToRun>,
        running: &HashMap<Id, Slot>,
    ) {
        for discussion in listed {
            let key = discussion.claim_key();
            let known = discussions.iter().any(|d| d.claim_key() == key)
                || running
                    .values()
                    .any(|slot| matches!(slot, Slot::Discussion(k) if *k == key));
            if !known {
                discussions.push_back(discussion);
            }
        }
    }

    fn has_free_slot(&self, work: &Loop) -> bool {
        (!work.queue.is_empty() || !work.discussions.is_empty())
            && work.running.len() < self.config.card.max_parallel as usize
            && !self.should_stop()
    }

    /// Discussions first (someone is waiting for an answer), then ready cards.
    fn start_ready(&self, work: &mut Loop, state: &State) {
        let max_parallel = self.config.card.max_parallel as usize;
        while work.running.len() < max_parallel {
            let Some(discussion) = work.discussions.pop_front() else {
                break;
            };
            let key = discussion.claim_key();
            let me = self.clone();
            let handle = work.sessions.spawn(async move {
                me.run_discussion(discussion).await;
            });
            work.running.insert(handle.id(), Slot::Discussion(key));
        }
        let batch: HashSet<String> = work
            .queue
            .iter()
            .map(|c| c.card_ref.clone())
            .chain(work.running.values().filter_map(|slot| match slot {
                Slot::Card(card_ref) => Some(card_ref.clone()),
                Slot::Discussion(_) => None,
            }))
            .collect();
        let mut index = 0;
        while index < work.queue.len() && work.running.len() < max_parallel {
            if !is_ready(&work.queue[index], state, &batch) {
                index += 1;
                continue;
            }
            let card = work.queue.remove(index);
            let workspace = self.config.workspace_for(&card.card_ref).cloned();
            let me = self.clone();
            let card_ref = card.card_ref.clone();
            let handle = work.sessions.spawn(async move {
                me.run_card_in(&card.card_ref, workspace, &card.blocked_by)
                    .await;
            });
            work.running.insert(handle.id(), Slot::Card(card_ref));
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
                    self.emit("triage", "failed", &one_line(&format!("store: {error}")));
                    return;
                }
            }
        } else {
            Vec::new()
        };
        if events.is_empty() && !sweep_due {
            return;
        }
        let me = self.clone();
        work.triage = Some(tokio::spawn(async move { me.triage(events).await }));
    }

    async fn finish_triage(&self, work: &mut Loop, triaged: Option<Triaged>, interval: Duration) {
        work.triage = None;
        work.triaged = true;
        let Some(Triaged { result, events }) = triaged else {
            // The triage task panicked; treat it as a failed sweep.
            work.sweep_failures += 1;
            work.next_sweep = Instant::now() + backoff_delay(work.sweep_failures, interval);
            return;
        };
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
            let runner = self.name().to_string();
            let finished = self
                .store()
                .call(move |s| s.finish_batch(&runner, &events, succeeded))
                .await;
            if let Err(error) = finished {
                self.emit("triage", "failed", &one_line(&format!("store: {error}")));
            }
            let batching = &mut work.batching;
            batching.failures = if succeeded { 0 } else { batching.failures + 1 };
            batching.retry_at =
                (!succeeded).then(|| Instant::now() + backoff_delay(batching.failures, interval));
        }
        if let Some(result) = result
            && let Some(state) = self.load_for_scheduling(&mut work.state_error).await
        {
            let busy: HashSet<String> = work
                .running
                .values()
                .filter_map(|slot| match slot {
                    Slot::Card(card_ref) => Some(card_ref.clone()),
                    Slot::Discussion(_) => None,
                })
                .collect();
            self.merge_queue(&mut work.queue, result.cards_to_work, &busy, &state);
            Self::merge_discussions(
                &mut work.discussions,
                result.discussions_to_run,
                &work.running,
            );
        }
    }

    /// The coordinator loop. `once`: one triage (over pending events, else a sweep), then
    /// its cards and discussions (respecting `blocked_by` and `max_parallel`), and return;
    /// cards whose blockers never finish are left for the next run. Intake sources only run
    /// in the long-lived loop.
    pub async fn schedule(&self, interval: Duration, once: bool) {
        let mut work = Loop {
            queue: Vec::new(),
            discussions: VecDeque::new(),
            sessions: JoinSet::new(),
            running: HashMap::new(),
            triage: None,
            triaged: false,
            sweep_failures: 0,
            next_sweep: Instant::now(),
            batching: Batching::default(),
            state_error: None,
        };
        let mut sources = if once || self.should_stop() {
            JoinSet::new()
        } else {
            intake::spawn_sources(&self.intake_context())
        };
        let mut shutdown = self.subscribe_shutdown();
        let wake = self.wake.clone();

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
            if work.triage.is_none() && triage_allowed {
                self.start_triage(&mut work, once).await;
            }
            if self.has_free_slot(&work) {
                // An unreadable store skips this tick's scheduling; never an empty state.
                if let Some(state) = self.load_for_scheduling(&mut work.state_error).await {
                    self.start_ready(&mut work, &state);
                }
            }
            if once && work.triaged && work.triage.is_none() && work.sessions.is_empty() {
                break;
            }
            let triage_idle = work.triage.is_none() && triage_allowed;
            let window = work.batching.window_closes;
            let event = tokio::select! {
                joined = wait_triage(&mut work.triage) => Wake::Triaged(joined.ok()),
                Some(done) = work.sessions.join_next_with_id(), if !work.sessions.is_empty() => {
                    Wake::SessionDone(match done {
                        Ok((id, ())) => id,
                        Err(error) => error.id(),
                    })
                }
                () = sleep_until(work.next_sweep), if triage_idle => Wake::Tick,
                () = sleep_until(window.unwrap_or_else(Instant::now)), if triage_idle && window.is_some() => Wake::Tick,
                () = wake.notified() => Wake::Tick,
                () = sleep(crate::dispatch::KILL_SWITCH_POLL) => Wake::Tick,
                _ = shutdown.changed() => Wake::Tick,
            };
            match event {
                Wake::Triaged(triaged) => self.finish_triage(&mut work, triaged, interval).await,
                Wake::SessionDone(id) => {
                    work.running.remove(&id);
                }
                Wake::Tick => {}
            }
        }

        sources.abort_all();
        if let Some(handle) = work.triage.take()
            && let Ok(triaged) = handle.await
            && !triaged.events.is_empty()
        {
            let (runner, ids) = (self.name().to_string(), triaged.events);
            let succeeded = triaged.result.is_some();
            let _ = self
                .store()
                .call(move |s| s.finish_batch(&runner, &ids, succeeded))
                .await;
        }
        while work.sessions.join_next().await.is_some() {}
        if self.killed() {
            self.emit("triage", "stopped", "kill switch present");
        }
    }
}

#[cfg(test)]
mod tests;
