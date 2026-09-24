//! Event intake: independent sources (macOS notifications, Linear, Jira) that store events in
//! `harness.db`, where the dispatcher batches them into triage sessions. Each source is its
//! own tokio task with its own backoff; one failing never stops the others.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Notify;
use tokio::task::JoinSet;

pub mod event;
pub mod http;
pub mod jira;
pub mod linear;
pub mod notifications;
pub mod secrets;

pub use event::{Event, EventKind, Fetched, IncomingEvent};

use crate::config::{Config, IntakeConfig};
use crate::paths::Paths;
use crate::runner::{Clock, Output};
use crate::store::{Enqueued, Store};
use secrets::{EnvLookup, Secrets};

const MAX_BACKOFF: Duration = Duration::from_secs(10 * 60);
const MAX_DOUBLINGS: u32 = 16;

/// Everything a source task needs; cheap to clone.
#[derive(Clone)]
pub struct IntakeContext {
    pub config: Arc<Config>,
    pub paths: Arc<Paths>,
    pub store: Store,
    pub env: EnvLookup,
    pub clock: Clock,
    pub out: Output,
    /// Wakes the dispatcher when new events are stored.
    pub wake: Arc<Notify>,
}

impl IntakeContext {
    fn emit(&self, detail: &str) {
        let now = (self.clock)().format("%Y-%m-%dT%H:%M:%SZ");
        (self.out)(format!("{now} intake {detail}"));
    }

    fn secrets(&self) -> Secrets {
        Secrets::load(&self.paths.secrets, self.env.clone())
    }

    fn runner(&self) -> &str {
        &self.config.name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Notifications,
    Linear,
    Jira,
}

impl SourceKind {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SourceKind::Notifications => notifications::SOURCE,
            SourceKind::Linear => linear::SOURCE,
            SourceKind::Jira => jira::SOURCE,
        }
    }

    fn poll_interval(self, intake: &IntakeConfig) -> Duration {
        match self {
            SourceKind::Notifications => intake.notifications.poll,
            SourceKind::Linear => intake.linear.poll,
            SourceKind::Jira => intake.jira.poll,
        }
    }

    /// Sources enabled in config.
    #[must_use]
    pub fn enabled(intake: &IntakeConfig) -> Vec<SourceKind> {
        [
            (SourceKind::Notifications, intake.notifications.enabled),
            (SourceKind::Linear, intake.linear.enabled),
            (SourceKind::Jira, intake.jira.enabled),
        ]
        .into_iter()
        .filter_map(|(kind, enabled)| enabled.then_some(kind))
        .collect()
    }
}

/// Names of the secrets a source needs.
#[must_use]
pub fn required_keys(kind: SourceKind, intake: &IntakeConfig) -> Vec<&str> {
    match kind {
        SourceKind::Notifications => vec![],
        SourceKind::Linear => vec![intake.linear.api_key_env.as_str()],
        SourceKind::Jira => vec![
            intake.jira.email_env.as_str(),
            intake.jira.token_env.as_str(),
        ],
    }
}

/// Why a poll produced nothing: waiting for a key (not a failure) or a real failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollError {
    Idle(String),
    Failed(String),
}

impl From<http::HttpError> for PollError {
    fn from(error: http::HttpError) -> Self {
        PollError::Failed(error.to_string())
    }
}

/// Mentions of me and work on my own tickets always pass; otherwise a non-empty
/// `allow_senders` must contain (case-insensitively) part of the sender.
#[must_use]
pub fn admitted(intake: &IntakeConfig, event: &IncomingEvent) -> bool {
    if event.kind == EventKind::Work || event.mentions_me || intake.allow_senders.is_empty() {
        return true;
    }
    let Some(sender) = event.sender.as_deref().map(str::to_lowercase) else {
        return false;
    };
    intake
        .allow_senders
        .iter()
        .any(|allowed| !allowed.is_empty() && sender.contains(&allowed.to_lowercase()))
}

fn cursor_key(kind: SourceKind, stream: &str) -> String {
    format!("intake:{}:{stream}", kind.name())
}

async fn load_cursor(ctx: &IntakeContext, key: String) -> Result<Option<String>, PollError> {
    let runner = ctx.runner().to_string();
    ctx.store
        .call(move |s| s.cursor(&runner, &key))
        .await
        .map_err(|e| PollError::Failed(format!("store: {e}")))
}

async fn time_cursor(ctx: &IntakeContext, key: &str) -> Result<Option<DateTime<Utc>>, PollError> {
    Ok(load_cursor(ctx, key.to_string())
        .await?
        .and_then(|text| crate::store::parse_time(&text).ok()))
}

/// One stream of events: fetched, admitted, stored, then the cursor moves.
struct Batch {
    events: Vec<IncomingEvent>,
    cursors: Vec<(String, String)>,
}

impl Batch {
    fn empty() -> Batch {
        Batch {
            events: Vec::new(),
            cursors: Vec::new(),
        }
    }

    /// Add a fetched time-cursored stream; a first run only sets its cursor to `now`.
    fn add(
        &mut self,
        key: String,
        since: Option<DateTime<Utc>>,
        fetched: Option<Fetched>,
        now: DateTime<Utc>,
    ) {
        match (since, fetched) {
            (Some(since), Some(fetched)) => {
                let next = fetched.latest.map_or(since, |latest| latest.max(since));
                self.events.extend(fetched.events);
                self.cursors.push((key, crate::store::iso(next)));
            }
            _ => self.cursors.push((key, crate::store::iso(now))),
        }
    }
}

async fn poll_linear(ctx: &IntakeContext, secrets: &Secrets) -> Result<Batch, PollError> {
    let config = &ctx.config.intake.linear;
    let key = secrets
        .get(&config.api_key_env)
        .ok_or_else(|| PollError::Idle(secrets.missing_reason(&config.api_key_env)))?;
    let client = linear::LinearClient::new(&config.api_url, key)?;
    let now = (ctx.clock)();
    let mut batch = Batch::empty();
    let work_key = cursor_key(SourceKind::Linear, "work");
    let since = time_cursor(ctx, &work_key).await?;
    let fetched = match since {
        Some(since) => Some(client.work_since(config, since).await?),
        None => None,
    };
    batch.add(work_key, since, fetched, now);
    let talk_key = cursor_key(SourceKind::Linear, "discussion");
    let since = time_cursor(ctx, &talk_key).await?;
    let fetched = match since {
        Some(since) => Some(client.discussion_since(config, since).await?),
        None => None,
    };
    batch.add(talk_key, since, fetched, now);
    Ok(batch)
}

fn jira_client(ctx: &IntakeContext, secrets: &Secrets) -> Result<jira::JiraClient, PollError> {
    let config = &ctx.config.intake.jira;
    let email = secrets
        .get(&config.email_env)
        .ok_or_else(|| PollError::Idle(secrets.missing_reason(&config.email_env)))?;
    let token = secrets
        .get(&config.token_env)
        .ok_or_else(|| PollError::Idle(secrets.missing_reason(&config.token_env)))?;
    Ok(jira::JiraClient::new(&config.base_url, email, token)?)
}

async fn poll_jira(ctx: &IntakeContext, secrets: &Secrets) -> Result<Batch, PollError> {
    let client = jira_client(ctx, secrets)?;
    let jql = &ctx.config.intake.jira.jql;
    let now = (ctx.clock)();
    let mut batch = Batch::empty();
    let work_key = cursor_key(SourceKind::Jira, "work");
    let since = time_cursor(ctx, &work_key).await?;
    let fetched = match since {
        Some(since) => Some(client.work_since(jql, since, now).await?),
        None => None,
    };
    batch.add(work_key, since, fetched, now);
    let talk_key = cursor_key(SourceKind::Jira, "discussion");
    let since = time_cursor(ctx, &talk_key).await?;
    let fetched = match since {
        Some(since) => {
            let me = client.myself().await?;
            Some(client.discussion_since(jql, &me, since, now).await?)
        }
        None => None,
    };
    batch.add(talk_key, since, fetched, now);
    Ok(batch)
}

async fn poll_notifications(ctx: &IntakeContext) -> Result<Batch, PollError> {
    let key = cursor_key(SourceKind::Notifications, "rec_id");
    let cursor = load_cursor(ctx, key.clone())
        .await?
        .and_then(|text| text.parse::<i64>().ok());
    let db = ctx.paths.notifications_db.clone();
    let config = ctx.config.intake.notifications.clone();
    let names = ctx.config.intake.mention_names.clone();
    let now = (ctx.clock)();
    let polled =
        tokio::task::spawn_blocking(move || notifications::poll(&db, &config, &names, cursor, now))
            .await
            .map_err(|e| PollError::Failed(e.to_string()))?
            .map_err(|e| PollError::Failed(format!("notification DB (Full Disk Access?): {e}")))?;
    for skipped in &polled.skipped {
        ctx.emit(&format!("notifications skipped {skipped}"));
    }
    Ok(Batch {
        events: polled.events,
        cursors: vec![(key, polled.cursor.to_string())],
    })
}

/// Poll once: fetch, filter, store, advance cursors. Returns how many events were stored.
pub async fn poll_once(ctx: &IntakeContext, kind: SourceKind) -> Result<usize, PollError> {
    let batch = match kind {
        SourceKind::Notifications => poll_notifications(ctx).await?,
        SourceKind::Linear => poll_linear(ctx, &ctx.secrets()).await?,
        SourceKind::Jira => poll_jira(ctx, &ctx.secrets()).await?,
    };
    let admitted: Vec<IncomingEvent> = batch
        .events
        .into_iter()
        .filter(|event| admitted(&ctx.config.intake, event))
        .collect();
    let runner = ctx.runner().to_string();
    let now = (ctx.clock)();
    let cursors = batch.cursors.into_iter().collect();
    let stored = ctx
        .store
        .call(move |store| {
            let mut stored = 0;
            for event in &admitted {
                if let Enqueued::Inserted(_) = store.enqueue(&runner, event, now)? {
                    stored += 1;
                }
            }
            // Only after the events are stored: a crash in between re-fetches, dedup absorbs it.
            store.set_cursors(&runner, &cursors)?;
            Ok(stored)
        })
        .await
        .map_err(|e| PollError::Failed(format!("store: {e}")))?;
    if stored > 0 {
        ctx.wake.notify_one();
    }
    Ok(stored)
}

/// Delay after `failures` consecutive failures: the poll interval doubling, capped at
/// 10 minutes (or the interval, if longer).
#[must_use]
pub fn source_backoff(poll: Duration, failures: u32) -> Duration {
    let cap = MAX_BACKOFF.max(poll);
    cap.min(poll.saturating_mul(1u32 << failures.min(MAX_DOUBLINGS)))
}

/// A source's loop: poll, print one line per change of state (never a value), sleep.
pub async fn run_source(ctx: IntakeContext, kind: SourceKind) {
    let poll = kind.poll_interval(&ctx.config.intake);
    let mut failures = 0u32;
    let mut last_state = String::new();
    loop {
        let (state, delay) = match poll_once(&ctx, kind).await {
            Ok(stored) => {
                failures = 0;
                if stored > 0 {
                    ctx.emit(&format!("{} queued {stored} event(s)", kind.name()));
                }
                ("ready".to_string(), poll)
            }
            Err(PollError::Idle(reason)) => {
                failures = 0;
                (format!("idle — {reason}"), poll)
            }
            Err(PollError::Failed(error)) => {
                failures += 1;
                let delay = source_backoff(poll, failures);
                (format!("failed — {error}"), delay)
            }
        };
        if state != last_state {
            ctx.emit(&format!("{} {state}", kind.name()));
            last_state = state;
        }
        tokio::time::sleep(delay).await;
    }
}

/// One task per enabled source; dropping or aborting the set stops them.
pub fn spawn_sources(ctx: &IntakeContext) -> JoinSet<()> {
    let mut tasks = JoinSet::new();
    for kind in SourceKind::enabled(&ctx.config.intake) {
        tasks.spawn(run_source(ctx.clone(), kind));
    }
    tasks
}

/// Personal scope for doing work: may this runner claim `card_ref`?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardScope {
    /// Assigned to the key's owner in an enabled tracker.
    Mine,
    /// No tracker intake is enabled, so the runner has no API to ask; the skill checks.
    Unchecked,
    Refused(String),
}

/// Ask every enabled tracker whether `card_ref` is assigned to me. Fails closed: a missing
/// key, an API error or an unknown ref refuses the card.
pub async fn card_scope(config: &Config, secrets: &Secrets, card_ref: &str) -> CardScope {
    let intake = &config.intake;
    if !intake.linear.enabled && !intake.jira.enabled {
        return CardScope::Unchecked;
    }
    let mut reasons = Vec::new();
    if intake.linear.enabled {
        match linear_assigned(config, secrets, card_ref).await {
            Ok(Some(true)) => return CardScope::Mine,
            Ok(Some(false)) => reasons.push("not assigned to me in linear".to_string()),
            Ok(None) => reasons.push("not found in linear".to_string()),
            Err(reason) => reasons.push(format!("linear: {reason}")),
        }
    }
    if intake.jira.enabled {
        match jira_assigned(config, secrets, card_ref).await {
            Ok(Some(true)) => return CardScope::Mine,
            Ok(Some(false)) => reasons.push("not assigned to me in jira".to_string()),
            Ok(None) => reasons.push("not found in jira".to_string()),
            Err(reason) => reasons.push(format!("jira: {reason}")),
        }
    }
    CardScope::Refused(reasons.join("; "))
}

async fn linear_assigned(
    config: &Config,
    secrets: &Secrets,
    card_ref: &str,
) -> Result<Option<bool>, String> {
    let linear = &config.intake.linear;
    let key = secrets
        .get(&linear.api_key_env)
        .ok_or_else(|| secrets.missing_reason(&linear.api_key_env))?;
    let client = linear::LinearClient::new(&linear.api_url, key).map_err(|e| e.to_string())?;
    client
        .assigned_to_me(card_ref)
        .await
        .map_err(|e| e.to_string())
}

async fn jira_assigned(
    config: &Config,
    secrets: &Secrets,
    card_ref: &str,
) -> Result<Option<bool>, String> {
    let jira = &config.intake.jira;
    let email = secrets
        .get(&jira.email_env)
        .ok_or_else(|| secrets.missing_reason(&jira.email_env))?;
    let token = secrets
        .get(&jira.token_env)
        .ok_or_else(|| secrets.missing_reason(&jira.token_env))?;
    let client = jira::JiraClient::new(&jira.base_url, email, token).map_err(|e| e.to_string())?;
    let me = client.myself().await.map_err(|e| e.to_string())?;
    client
        .assigned_to_me(card_ref, &me)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
