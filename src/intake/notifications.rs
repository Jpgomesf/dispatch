//! macOS notification watcher: reads the notification center DB read-only (needs Full Disk
//! Access) and turns delivered notifications of the configured apps into `message` events.
//!
//! Schema (macOS 15+): `record(rec_id, app_id, uuid, data, request_date, request_last_date,
//! delivered_date, presented, style, snooze_fire_date)` ⋈ `app(app_id, identifier, badge)`.
//! `data` is a binary plist: `{app, date, req: {titl, subt, body, ...}, ...}`; dates are
//! seconds since 2001-01-01 (Core Data reference date).
//!
//! `rec_id` is a plain `INTEGER PRIMARY KEY` (no AUTOINCREMENT): when the newest rows are
//! deleted (read notifications are cleared) their ids are reused. So the cursor is
//! `delivered_date` (sub-microsecond, so `>` loses nothing in practice) and the dedup key is the
//! record's `uuid`.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::json;

use super::event::{EventKind, IncomingEvent, preview};
use crate::config::{NotificationsConfig, expand_user};

pub const SOURCE: &str = "notifications";
pub const DEFAULT_DB: &str = "~/Library/Group Containers/group.com.apple.usernoted/db2/db";
const MAX_ROWS_PER_POLL: i64 = 500;
/// 2001-01-01T00:00:00Z as a Unix timestamp.
const APPLE_EPOCH: i64 = 978_307_200;

#[must_use]
pub fn default_db() -> PathBuf {
    expand_user(Path::new(DEFAULT_DB))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub date: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("not a plist: {0}")]
    NotPlist(String),
    #[error("unexpected shape: {0}")]
    Shape(&'static str),
}

/// Decode one record's `data` plist. An unexpected shape is an error for that record only.
pub fn parse_record(data: &[u8]) -> Result<Notification, ParseError> {
    let value = plist::Value::from_reader(Cursor::new(data))
        .map_err(|e| ParseError::NotPlist(e.to_string()))?;
    let root = value
        .as_dictionary()
        .ok_or(ParseError::Shape("root is not a dictionary"))?;
    let request = root
        .get("req")
        .and_then(plist::Value::as_dictionary)
        .ok_or(ParseError::Shape("no req dictionary"))?;
    let text = |key: &str| -> Result<String, ParseError> {
        match request.get(key) {
            None => Ok(String::new()),
            Some(value) => value
                .as_string()
                .map(str::to_string)
                .ok_or(ParseError::Shape("title, subtitle or body is not a string")),
        }
    };
    let notification = Notification {
        title: text("titl")?,
        subtitle: text("subt")?,
        body: text("body")?,
        date: root
            .get("date")
            .and_then(plist::Value::as_real)
            .and_then(apple_time),
    };
    if notification.title.is_empty() && notification.body.is_empty() {
        return Err(ParseError::Shape("neither title nor body"));
    }
    Ok(notification)
}

fn apple_time(seconds: f64) -> Option<DateTime<Utc>> {
    if !seconds.is_finite() {
        return None;
    }
    // Millisecond precision is plenty; `as` saturates for out-of-range values.
    let millis = (seconds * 1000.0).round() as i64;
    Utc.timestamp_millis_opt(APPLE_EPOCH * 1000 + millis)
        .single()
}

/// One row of `record ⋈ app` newer than the cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct RawRecord {
    pub rec_id: i64,
    pub uuid: Vec<u8>,
    pub app: String,
    pub data: Vec<u8>,
    /// `delivered_date` (Core Data seconds); 0 when missing.
    pub delivered: f64,
}

impl RawRecord {
    /// Stable per notification, unlike `rec_id`.
    #[must_use]
    pub fn external_id(&self) -> String {
        if self.uuid.is_empty() {
            return format!("rec-{}", self.rec_id);
        }
        self.uuid.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// SQLite URI for a read-only open; `?` / `#` / `%` in the path must be escaped.
fn read_only_uri(path: &Path) -> String {
    let escaped = path
        .display()
        .to_string()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    format!("file:{escaped}?mode=ro")
}

fn open_read_only(db: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(read_only_uri(db), flags)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(conn)
}

/// The newest `delivered_date`: a first run starts from here instead of replaying history.
pub fn latest_delivered(db: &Path) -> rusqlite::Result<f64> {
    let conn = open_read_only(db)?;
    conn.query_row(
        "SELECT COALESCE(MAX(delivered_date), 0.0) FROM record",
        [],
        |r| r.get(0),
    )
}

/// Records delivered after `since`, oldest first, every app (the caller filters, so
/// the cursor also moves past other apps' records).
pub fn records_since(db: &Path, since: f64) -> rusqlite::Result<Vec<RawRecord>> {
    let conn = open_read_only(db)?;
    let mut statement = conn.prepare(
        "SELECT r.rec_id, r.uuid, a.identifier, r.data, COALESCE(r.delivered_date, 0.0)
         FROM record r JOIN app a ON a.app_id = r.app_id
         WHERE COALESCE(r.delivered_date, 0.0) > ?1
         ORDER BY r.delivered_date, r.rec_id LIMIT ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![since, MAX_ROWS_PER_POLL], |row| {
        Ok(RawRecord {
            rec_id: row.get(0)?,
            uuid: row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
            app: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            data: row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
            delivered: row.get(4)?,
        })
    })?;
    rows.collect()
}

/// `true` when the notification belongs to this runner: an empty `match` takes all.
#[must_use]
pub fn routes_here(config: &NotificationsConfig, notification: &Notification) -> bool {
    config.match_.is_empty()
        || config.match_.iter().any(|pattern| {
            notification.title.contains(pattern.as_str())
                || notification.subtitle.contains(pattern.as_str())
        })
}

#[must_use]
pub fn mentions_me(mention_names: &[String], notification: &Notification) -> bool {
    let text = format!(
        "{}\n{}\n{}",
        notification.title, notification.subtitle, notification.body
    )
    .to_lowercase();
    mention_names
        .iter()
        .filter(|name| !name.is_empty())
        .any(|name| text.contains(&name.to_lowercase()))
}

pub fn to_event(
    record: &RawRecord,
    notification: &Notification,
    mention_names: &[String],
    now: DateTime<Utc>,
) -> IncomingEvent {
    let delivered = Some(record.delivered)
        .filter(|seconds| *seconds > 0.0)
        .and_then(apple_time)
        .or(notification.date)
        .unwrap_or(now);
    IncomingEvent {
        source: SOURCE.into(),
        external_id: record.external_id(),
        kind: EventKind::Message,
        mentions_me: mentions_me(mention_names, notification),
        sender: (!notification.title.is_empty()).then(|| notification.title.clone()),
        occurred_at: delivered,
        payload: json!({
            "app": record.app,
            "title": notification.title,
            "subtitle": notification.subtitle,
            "body": preview(&notification.body),
            "delivered_at": crate::store::iso(delivered),
        }),
    }
}

/// Outcome of one poll: events for this runner, records skipped loudly, the new cursor.
#[derive(Debug, Default)]
pub struct Poll {
    pub events: Vec<IncomingEvent>,
    pub skipped: Vec<String>,
    /// Newest `delivered_date` seen.
    pub cursor: f64,
}

pub fn poll(
    db: &Path,
    config: &NotificationsConfig,
    mention_names: &[String],
    cursor: Option<f64>,
    now: DateTime<Utc>,
) -> rusqlite::Result<Poll> {
    let Some(since) = cursor else {
        return Ok(Poll {
            cursor: latest_delivered(db)?,
            ..Poll::default()
        });
    };
    let mut result = Poll {
        cursor: since,
        ..Poll::default()
    };
    for record in records_since(db, since)? {
        result.cursor = result.cursor.max(record.delivered);
        if !config.apps.contains(&record.app) {
            continue;
        }
        match parse_record(&record.data) {
            Ok(notification) if routes_here(config, &notification) => {
                result
                    .events
                    .push(to_event(&record, &notification, mention_names, now));
            }
            Ok(_) => {}
            Err(error) => result.skipped.push(format!(
                "record {} of {}: {error}",
                record.rec_id, record.app
            )),
        }
    }
    Ok(result)
}

/// Whether the DB is readable at all (Full Disk Access), for `harness check`.
pub fn probe(db: &Path) -> Result<(), String> {
    open_read_only(db)
        .and_then(|conn| {
            conn.query_row("SELECT rec_id FROM record LIMIT 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::SLACK_APP_ID;

    /// A synthetic notification plist in the shape macOS stores; fictional content only.
    pub(crate) fn fixture_plist(title: &str, subtitle: Option<&str>, body: &str) -> Vec<u8> {
        let mut request = plist::Dictionary::new();
        request.insert("titl".into(), title.into());
        if let Some(subtitle) = subtitle {
            request.insert("subt".into(), subtitle.into());
        }
        request.insert("body".into(), body.into());
        request.insert("iden".into(), "example-identifier".into());
        request.insert("thre".into(), "example-thread".into());
        let mut root = plist::Dictionary::new();
        root.insert("app".into(), SLACK_APP_ID.into());
        root.insert("date".into(), plist::Value::Real(790_000_000.5));
        root.insert("req".into(), plist::Value::Dictionary(request));
        let mut bytes = Vec::new();
        plist::Value::Dictionary(root)
            .to_writer_binary(&mut bytes)
            .unwrap();
        bytes
    }

    /// Fixture uuid: 16 bytes, the tag in the last one.
    pub(crate) fn uuid(tag: u8) -> Vec<u8> {
        let mut bytes = vec![0xab; 15];
        bytes.push(tag);
        bytes
    }

    /// Insert one record delivered at `790000000 + delivered` seconds.
    pub(crate) fn insert_record(
        conn: &Connection,
        rec_id: i64,
        tag: u8,
        app: &str,
        data: &[u8],
        delivered: f64,
    ) {
        conn.execute("INSERT OR IGNORE INTO app (identifier) VALUES (?1)", [app])
            .unwrap();
        let app_id: i64 = conn
            .query_row("SELECT app_id FROM app WHERE identifier = ?1", [app], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO record (rec_id, app_id, uuid, data, delivered_date) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![rec_id, app_id, uuid(tag), data, 790_000_000.0 + delivered],
        )
        .unwrap();
    }

    /// A notification DB with the real schema (column names verified on macOS). Record `n`
    /// has uuid tag `n` and is delivered `n` seconds after the fixture epoch.
    pub(crate) fn fixture_db(dir: &Path, records: &[(i64, &str, Vec<u8>)]) -> PathBuf {
        let path = dir.join("db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app (app_id INTEGER PRIMARY KEY, identifier VARCHAR, badge INTEGER NULL);
             CREATE TABLE record (rec_id INTEGER PRIMARY KEY, app_id INTEGER, uuid BLOB, data BLOB,
                 request_date REAL, request_last_date REAL, delivered_date REAL, presented Bool,
                 style INTEGER, snooze_fire_date REAL);",
        )
        .unwrap();
        for (rec_id, app, data) in records {
            let tag = u8::try_from(*rec_id).unwrap();
            insert_record(&conn, *rec_id, tag, app, data, *rec_id as f64);
        }
        path
    }

    fn config(match_: &[&str]) -> NotificationsConfig {
        NotificationsConfig {
            enabled: true,
            match_: match_.iter().map(|s| s.to_string()).collect(),
            ..NotificationsConfig::default()
        }
    }

    fn external_id(tag: u8) -> String {
        format!("{}{tag:02x}", "ab".repeat(15))
    }

    #[test]
    fn parses_a_synthetic_record() {
        let data = fixture_plist(
            "Example Person",
            Some("#example-channel"),
            "Can you look at EX-1?",
        );
        let parsed = parse_record(&data).unwrap();
        assert_eq!(parsed.title, "Example Person");
        assert_eq!(parsed.subtitle, "#example-channel");
        assert_eq!(parsed.body, "Can you look at EX-1?");
        assert_eq!(
            parsed.date.unwrap().to_rfc3339(),
            "2026-01-13T12:26:40.500+00:00"
        );
        let no_subtitle = parse_record(&fixture_plist("Example Co", None, "hi")).unwrap();
        assert_eq!(no_subtitle.subtitle, "");
    }

    #[test]
    fn unexpected_shapes_are_errors() {
        assert!(matches!(
            parse_record(b"not a plist"),
            Err(ParseError::NotPlist(_))
        ));
        let mut array = Vec::new();
        plist::Value::Array(vec![])
            .to_writer_binary(&mut array)
            .unwrap();
        assert_eq!(
            parse_record(&array),
            Err(ParseError::Shape("root is not a dictionary"))
        );
        let mut no_req = Vec::new();
        plist::Value::Dictionary(plist::Dictionary::new())
            .to_writer_binary(&mut no_req)
            .unwrap();
        assert_eq!(
            parse_record(&no_req),
            Err(ParseError::Shape("no req dictionary"))
        );
        let mut request = plist::Dictionary::new();
        request.insert("titl".into(), plist::Value::Integer(7.into()));
        let mut root = plist::Dictionary::new();
        root.insert("req".into(), plist::Value::Dictionary(request));
        let mut bad_title = Vec::new();
        plist::Value::Dictionary(root)
            .to_writer_binary(&mut bad_title)
            .unwrap();
        assert!(matches!(
            parse_record(&bad_title),
            Err(ParseError::Shape(_))
        ));
    }

    #[test]
    fn first_poll_starts_at_the_newest_record() {
        let dir = tempfile::tempdir().unwrap();
        let data = fixture_plist("Example Person", None, "old news");
        let db = fixture_db(dir.path(), &[(7, SLACK_APP_ID, data)]);
        let first = poll(&db, &config(&[]), &[], None, crate::testing::now()).unwrap();
        assert!(first.events.is_empty());
        assert_eq!(first.cursor, 790_000_007.0);
    }

    #[test]
    fn polls_routes_and_skips_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![
            (
                1,
                SLACK_APP_ID,
                fixture_plist(
                    "Example Person",
                    Some("#example-channel"),
                    "ping @Example User",
                ),
            ),
            (
                2,
                SLACK_APP_ID,
                fixture_plist("Other Person", Some("#other-channel"), "unrelated"),
            ),
            (
                3,
                "com.example.other",
                fixture_plist("Example Person", Some("#example-channel"), "other app"),
            ),
            (4, SLACK_APP_ID, b"garbage".to_vec()),
            (
                5,
                SLACK_APP_ID,
                fixture_plist("Example Person", Some("#example-channel"), "second"),
            ),
        ];
        let db = fixture_db(dir.path(), &records);
        let names = vec!["@example user".to_string()];
        let result = poll(
            &db,
            &config(&["#example-channel"]),
            &names,
            Some(0.0),
            crate::testing::now(),
        )
        .unwrap();
        assert_eq!(
            result.cursor, 790_000_005.0,
            "the cursor passes every record"
        );
        let ids: Vec<&str> = result
            .events
            .iter()
            .map(|e| e.external_id.as_str())
            .collect();
        assert_eq!(ids, [external_id(1), external_id(5)]);
        assert!(result.events[0].mentions_me);
        assert!(!result.events[1].mentions_me);
        assert_eq!(result.events[0].sender.as_deref(), Some("Example Person"));
        assert_eq!(result.events[0].payload["subtitle"], "#example-channel");
        assert_eq!(result.events[0].payload["app"], SLACK_APP_ID);
        assert_eq!(result.skipped.len(), 1);
        assert!(
            result.skipped[0].starts_with("record 4 of com.tinyspeck.slackmacgap: not a plist")
        );

        let again = poll(
            &db,
            &config(&[]),
            &[],
            Some(result.cursor),
            crate::testing::now(),
        )
        .unwrap();
        assert!(again.events.is_empty() && again.skipped.is_empty());
    }

    #[test]
    fn reused_rec_ids_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let first = fixture_plist("Example Person", None, "first");
        let db = fixture_db(
            dir.path(),
            &[(1, SLACK_APP_ID, first.clone()), (2, SLACK_APP_ID, first)],
        );
        let seen = poll(&db, &config(&[]), &[], Some(0.0), crate::testing::now()).unwrap();
        assert_eq!(seen.events.len(), 2);
        // The newest record is cleared and SQLite hands its rec_id to the next notification.
        let conn = Connection::open(&db).unwrap();
        conn.execute("DELETE FROM record WHERE rec_id = 2", [])
            .unwrap();
        let next = fixture_plist("Example Person", None, "next");
        insert_record(&conn, 2, 99, SLACK_APP_ID, &next, 9.0);
        let later = poll(
            &db,
            &config(&[]),
            &[],
            Some(seen.cursor),
            crate::testing::now(),
        )
        .unwrap();
        let ids: Vec<&str> = later
            .events
            .iter()
            .map(|e| e.external_id.as_str())
            .collect();
        assert_eq!(ids, [external_id(99)], "a new uuid, so a new event");
        assert_eq!(later.cursor, 790_000_009.0);
    }

    #[test]
    fn database_is_opened_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path(), &[]);
        let conn = open_read_only(&db).unwrap();
        assert!(conn.execute("DELETE FROM record", []).is_err());
        assert!(probe(&db).is_ok());
        assert!(probe(&dir.path().join("missing")).is_err());
        assert!(read_only_uri(Path::new("/a b/c?d#e%")).ends_with("/a b/c%3fd%23e%25?mode=ro"));
    }

    #[test]
    fn long_bodies_are_previewed() {
        let record = RawRecord {
            rec_id: 9,
            uuid: vec![],
            app: SLACK_APP_ID.into(),
            data: vec![],
            delivered: 0.0,
        };
        let notification = Notification {
            title: String::new(),
            subtitle: String::new(),
            body: "x".repeat(2000),
            date: None,
        };
        let event = to_event(&record, &notification, &[], crate::testing::now());
        assert_eq!(event.payload["body"].as_str().unwrap().len(), 500);
        assert_eq!(event.sender, None);
        assert_eq!(event.external_id, "rec-9");
        assert_eq!(event.occurred_at, crate::testing::now());
    }
}
