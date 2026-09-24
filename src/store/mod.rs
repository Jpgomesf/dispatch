//! `harness.db`: the machine-wide SQLite store shared by every runner (events, claims,
//! cards, cursors). One connection per process, used from the blocking pool.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

mod cards;
mod claims;
mod events;
mod schema;

pub use claims::{CLAIM_LEASE, CLAIM_RENEW, Claim};
pub use events::Enqueued;

pub const DB_ENV: &str = "HARNESS_DB";
pub const DEFAULT_DB: &str = "~/.local/state/claude-harness/harness.db";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Sqlite {
        path: String,
        source: rusqlite::Error,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("store task: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (creating it and its directory if needed) and apply pending migrations.
    pub fn open(path: &Path) -> Result<Store> {
        let fail = |source| StoreError::Sqlite {
            path: path.display().to_string(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| StoreError::Invalid(format!("create {}: {e}", dir.display())))?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut conn = Connection::open_with_flags(path, flags).map_err(fail)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(fail)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(fail)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(fail)?;
        migrate(&mut conn).map_err(fail)?;
        Ok(Store {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `work` on the blocking pool, so lock waits and fsyncs never stall async tasks.
    pub async fn call<R: Send + 'static>(
        &self,
        work: impl FnOnce(&Store) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || work(&store)).await?
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the connection leaves it usable: every write is a transaction.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn error(&self, source: rusqlite::Error) -> StoreError {
        StoreError::Sqlite {
            path: self.path.display().to_string(),
            source,
        }
    }

    /// One transaction; `immediate` takes the write lock up front (claims, batches).
    fn write<R>(
        &self,
        immediate: bool,
        work: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
    ) -> Result<R> {
        let mut conn = self.conn();
        let behavior = if immediate {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        };
        let run = || {
            let tx = conn.transaction_with_behavior(behavior)?;
            let result = work(&tx)?;
            tx.commit()?;
            Ok(result)
        };
        run().map_err(|e| self.error(e))
    }

    fn read<R>(&self, work: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> Result<R> {
        work(&self.conn()).map_err(|e| self.error(e))
    }
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
        [],
    )?;
    let applied: i64 = tx.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    for (version, migration) in (1i64..).zip(schema::MIGRATIONS) {
        if version <= applied {
            continue;
        }
        tx.execute_batch(migration)?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [version],
        )?;
    }
    tx.commit()
}

pub(crate) fn iso(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn parse_time(text: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("nested/harness.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn opens_with_wal_and_migrates_once() {
        let (dir, store) = temp_store();
        let mode: String = store
            .read(|c| c.query_row("PRAGMA journal_mode", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(store);
        let again = Store::open(&dir.path().join("nested/harness.db")).unwrap();
        let versions: Vec<i64> = again
            .read(|c| {
                let mut statement = c.prepare("SELECT version FROM schema_version")?;
                statement.query_map([], |r| r.get(0))?.collect()
            })
            .unwrap();
        assert_eq!(versions, [1]);
    }

    #[test]
    fn times_round_trip() {
        let now = crate::testing::now();
        assert_eq!(iso(now), "2026-01-15T09:30:00.000Z");
        assert_eq!(parse_time(&iso(now)).unwrap(), now);
        assert!(parse_time("yesterday").is_err());
    }
}
