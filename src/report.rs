//! Read-only views of `dispatch.db` for `dispatch status` (what this runner is doing now) and
//! `dispatch history` (what its sessions did, with what `claude --resume` needs).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::state::CardStatus;
use crate::store::{Attempt, Result, Store};

/// Attempts `dispatch history` shows when no ref is given.
pub const HISTORY_LIMIT: i64 = 20;

fn time(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Single-quoted for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `card EX-1 #2`, or `triage #7`.
fn attempt_label(attempt: &Attempt) -> String {
    if attempt.reference.is_empty() {
        format!("{} #{}", attempt.mode, attempt.attempt)
    } else {
        format!(
            "{} {} #{}",
            attempt.mode, attempt.reference, attempt.attempt
        )
    }
}

pub fn status_lines(
    store: &Store,
    runner: &str,
    kill_switch: bool,
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    let mut lines = vec![
        format!("runner:       {runner}"),
        format!("kill switch:  {}", if kill_switch { "SET" } else { "off" }),
    ];

    let running = store.open_attempts(runner)?;
    lines.push(format!("sessions:     {} running", running.len()));
    for attempt in &running {
        let session = attempt.session_id.as_deref().unwrap_or("not yet known");
        lines.push(format!(
            "  {} since {} session={session} in {}",
            attempt_label(attempt),
            time(attempt.started_at),
            attempt.cwd
        ));
    }

    lines.push(format!(
        "events:       {} new, {} batched",
        store.event_count(runner, "new")?,
        store.event_count(runner, "batched")?
    ));

    let state = store.load_state(runner)?;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for card in state.cards.values() {
        *counts.entry(card.status.as_str()).or_default() += 1;
    }
    let counted: Vec<String> = counts
        .iter()
        .map(|(status, count)| format!("{count} {status}"))
        .collect();
    let summary = if counted.is_empty() {
        "none".to_string()
    } else {
        counted.join(", ")
    };
    lines.push(format!("cards:        {summary}"));
    for (card_ref, card) in &state.cards {
        if card.status == CardStatus::Done {
            continue;
        }
        let mut line = format!(
            "  {card_ref} {} since {}",
            card.status.as_str(),
            time(card.updated_at)
        );
        if card.attempts > 0 {
            line.push_str(&format!(", {} attempt(s) in this run", card.attempts));
        }
        if let Some(at) = card.retry_at {
            line.push_str(&format!(", retry at {}", time(at)));
        }
        if let Some(reason) = &card.reason {
            line.push_str(&format!(", reason {reason}"));
        }
        lines.push(line);
    }

    let claims = store.claims_held(runner, now)?;
    lines.push(format!("claims:       {}", claims.len()));
    for (key, until) in &claims {
        lines.push(format!("  {key} until {}", time(*until)));
    }
    Ok(lines)
}

/// Oldest first: every attempt of `reference`, else the last `HISTORY_LIMIT`.
pub fn history_lines(store: &Store, runner: &str, reference: Option<&str>) -> Result<Vec<String>> {
    let limit = if reference.is_some() {
        -1
    } else {
        HISTORY_LIMIT
    };
    let mut attempts = store.attempts(runner, reference, limit)?;
    attempts.reverse();
    let mut lines = Vec::new();
    for attempt in &attempts {
        let outcome = attempt.outcome.as_deref().unwrap_or("running");
        let ended = attempt.ended_at.map_or_else(|| "…".to_string(), time);
        let cost = attempt
            .cost_usd
            .map(|c| format!(" cost=${c:.2}"))
            .unwrap_or_default();
        let commits = attempt
            .new_commits
            .map(|n| format!(" new_commits={n}"))
            .unwrap_or_default();
        lines.push(format!(
            "{} {outcome} {} → {ended}{cost}{commits}",
            attempt_label(attempt),
            time(attempt.started_at)
        ));
        if let Some(summary) = attempt.summary.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("  {summary}"));
        }
        if let Some(blocked_on) = &attempt.blocked_on {
            lines.push(format!("  blocked on: {blocked_on}"));
        }
        if let Some(session_id) = &attempt.session_id {
            lines.push(format!(
                "  resume: cd {} && claude --resume {session_id}",
                shell_quote(&attempt.cwd)
            ));
        }
    }
    if lines.is_empty() {
        lines.push(match reference {
            Some(reference) => format!("no attempts for {reference}"),
            None => "no attempts yet".to_string(),
        });
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::state::{CardState, CardStatus};
    use crate::store::{AttemptEnd, CLAIM_LEASE};
    use crate::testing::now;

    fn seeded() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("dispatch.db")).unwrap();
        let cwd = Path::new("/tmp/example app/worktree");
        let (done, _) = store
            .begin_attempt("example-app", "card", "EX-1", cwd, now())
            .unwrap();
        store
            .end_attempt(
                done,
                &AttemptEnd {
                    outcome: "blocked".into(),
                    summary: "waiting for an answer".into(),
                    blocked_on: Some("a product decision".into()),
                    session_id: Some("session-1".into()),
                    cost_usd: Some(1.5),
                    new_commits: Some(0),
                    ended_at: now(),
                },
            )
            .unwrap();
        store
            .begin_attempt("example-app", "card", "EX-1", cwd, now())
            .unwrap();
        store
            .begin_attempt("other-app", "card", "EX-9", cwd, now())
            .unwrap();
        let card = |status| CardState::new(status, now());
        store
            .set_card("example-app", "EX-1", &card(CardStatus::InProgress))
            .unwrap();
        store
            .set_card("example-app", "EX-0", &card(CardStatus::Done))
            .unwrap();
        let mut waiting = card(CardStatus::Failed);
        waiting.attempts = 2;
        waiting.retry_at = Some(now() + chrono::TimeDelta::minutes(2));
        store.set_card("example-app", "EX-2", &waiting).unwrap();
        let mut escalated = card(CardStatus::NeedsHuman);
        escalated.attempts = 3;
        escalated.reason = Some("max_attempts".into());
        store.set_card("example-app", "EX-3", &escalated).unwrap();
        store
            .claim("card:EX-1", "example-app", now(), CLAIM_LEASE)
            .unwrap();
        (dir, store)
    }

    #[test]
    fn status_shows_sessions_events_cards_and_claims() {
        let (_dir, store) = seeded();
        let lines = status_lines(&store, "example-app", false, now()).unwrap();
        let text = lines.join("\n");
        assert!(text.contains("sessions:     1 running"), "{text}");
        assert!(
            text.contains("  card EX-1 #2 since 2026-01-15T09:30:00Z session=not yet known"),
            "{text}"
        );
        assert!(text.contains("events:       0 new, 0 batched"), "{text}");
        assert!(
            text.contains("cards:        1 done, 1 failed, 1 in_progress, 1 needs_human"),
            "{text}"
        );
        assert!(text.contains("  EX-1 in_progress since"), "{text}");
        assert!(
            text.contains("  EX-2 failed since 2026-01-15T09:30:00Z, 2 attempt(s) in this run, retry at 2026-01-15T09:32:00Z"),
            "{text}"
        );
        assert!(
            text.contains("  EX-3 needs_human since 2026-01-15T09:30:00Z, 3 attempt(s) in this run, reason max_attempts"),
            "{text}"
        );
        assert!(
            !text.contains("  EX-0"),
            "done cards are only counted: {text}"
        );
        assert!(
            text.contains("  card:EX-1 until 2026-01-15T09:40:00Z"),
            "{text}"
        );
        assert!(
            !text.contains("EX-9"),
            "other runners are not shown: {text}"
        );
    }

    #[test]
    fn history_prints_how_to_resume_each_session() {
        let (_dir, store) = seeded();
        let lines = history_lines(&store, "example-app", Some("EX-1")).unwrap();
        assert_eq!(
            lines,
            [
                "card EX-1 #1 blocked 2026-01-15T09:30:00Z → 2026-01-15T09:30:00Z cost=$1.50 new_commits=0",
                "  waiting for an answer",
                "  blocked on: a product decision",
                "  resume: cd '/tmp/example app/worktree' && claude --resume session-1",
                "card EX-1 #2 running 2026-01-15T09:30:00Z → …",
            ]
        );
        let empty = history_lines(&store, "example-app", Some("EX-404")).unwrap();
        assert_eq!(empty, ["no attempts for EX-404"]);
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
