use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardStatus {
    InProgress,
    Done,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardState {
    pub status: CardStatus,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub cursors: BTreeMap<String, String>,
    pub cards: BTreeMap<String, CardState>,
}

impl State {
    pub fn status(&self, card_ref: &str) -> Option<CardStatus> {
        self.cards.get(card_ref).map(|card| card.status)
    }

    pub fn in_progress(&self, card_ref: &str) -> bool {
        self.status(card_ref) == Some(CardStatus::InProgress)
    }

    pub fn set_card(
        &mut self,
        card_ref: &str,
        status: CardStatus,
        now: DateTime<Utc>,
        pr_url: Option<String>,
    ) {
        let card = CardState {
            status,
            updated_at: now,
            pr_url,
        };
        self.cards.insert(card_ref.to_string(), card);
    }
}

/// `state.json` in the state dir. Reads are lock-free (writes are atomic renames);
/// read-modify-write goes through `update`, under an exclusive lock on `state.lock`,
/// so parallel card tasks and other harness processes never lose each other's writes.
#[derive(Debug, Clone)]
pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    pub fn new(state_dir: &Path) -> StateStore {
        StateStore {
            dir: state_dir.to_path_buf(),
        }
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn load(&self) -> Result<State> {
        load_state(&self.file())
    }

    pub fn update<R>(&self, change: impl FnOnce(&mut State) -> R) -> Result<R> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create state dir {}", self.dir.display()))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("state.lock"))
            .context("open state lock")?;
        lock.lock().context("lock state")?;
        let mut state = self.load()?;
        let result = change(&mut state);
        save_state(&self.file(), &state)?;
        Ok(result)
        // `lock` drops here, releasing the lock.
    }
}

pub fn load_state(path: &Path) -> Result<State> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Atomic write: temp file in the same dir, fsync, rename over the target.
pub fn save_state(path: &Path, state: &State) -> Result<()> {
    let dir = path.parent().context("state path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".state-")
        .suffix(".json")
        .tempfile_in(dir)?;
    serde_json::to_writer_pretty(&mut tmp, state)?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::now;

    #[test]
    fn missing_state_file_is_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_state(&dir.path().join("state.json")).unwrap(),
            State::default()
        );
    }

    #[test]
    fn round_trip_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state.json");
        let mut state = State::default();
        state
            .cursors
            .insert("slack:C0000000001".into(), "1700000000.000100".into());
        state.set_card(
            "EX-1",
            CardStatus::Done,
            now(),
            Some("https://example.com/pr/1".into()),
        );
        save_state(&path, &state).unwrap();
        assert_eq!(load_state(&path).unwrap(), state);
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["state.json"]);
    }

    #[test]
    fn reads_state_written_by_the_python_runner() {
        let text = r#"{"cursors": {"tracker:linear": "c1"}, "sends": [],
            "cards": {"EX-1": {"status": "in_progress", "updated_at": "2026-01-15T09:30:00Z", "pr_url": null}}}"#;
        let state: State = serde_json::from_str(text).unwrap();
        assert!(state.in_progress("EX-1"));
        assert_eq!(state.cursors["tracker:linear"], "c1");
    }

    #[test]
    fn in_progress() {
        let mut state = State::default();
        state.set_card("EX-1", CardStatus::InProgress, now(), None);
        state.set_card("EX-2", CardStatus::Done, now(), None);
        assert!(state.in_progress("EX-1"));
        assert!(!state.in_progress("EX-2"));
        assert!(!state.in_progress("EX-3"));
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path());
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        store
                            .update(|s| {
                                s.cursors.insert(format!("t{i}-{j}"), "x".into());
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(store.load().unwrap().cursors.len(), 80);
    }
}
