//! Append-only SQLite log, idempotent on event id. Used by the hub and by every host replica.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::event::{Body, Event};

pub struct Store {
    conn: Connection,
}

/// The cursor names an event this log does not have (e.g. the hub's log was replaced).
#[derive(Debug)]
pub struct UnknownCursor(pub String);

impl std::fmt::Display for UnknownCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown cursor {}", self.0)
    }
}

impl std::error::Error for UnknownCursor {}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                seq  INTEGER PRIMARY KEY AUTOINCREMENT,
                id   TEXT NOT NULL UNIQUE,
                type TEXT NOT NULL,
                json TEXT NOT NULL,
                at   INTEGER
            );
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        let has_at: bool = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'at'",
            [],
            |r| r.get(0),
        )?;
        if !has_at {
            conn.execute_batch("ALTER TABLE events ADD COLUMN at INTEGER")?;
        }
        Ok(Self { conn })
    }

    /// Appends `e` unless an event with its id is already in the log. Returns whether it was new.
    pub fn append(&self, e: &Event) -> Result<bool> {
        let ty = match &e.body {
            Body::Known(k) => k.type_name().to_string(),
            Body::Unknown(v) => v["type"].as_str().unwrap_or("?").to_string(),
        };
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO events (id, type, json, at) VALUES (?1, ?2, ?3, ?4)",
            params![
                e.id,
                ty,
                serde_json::to_string(e)?,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        Ok(n == 1)
    }

    /// Every event with the time it reached this store (unix ms; `None` for rows older than
    /// the column).
    pub fn all_with_arrival(&self) -> Result<Vec<(Event, Option<i64>)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT json, at FROM events ORDER BY seq")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
        })?;
        rows.map(|row| {
            let (json, at) = row?;
            Ok((serde_json::from_str(&json)?, at))
        })
        .collect()
    }

    pub fn bytes(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn contains(&self, id: &str) -> Result<bool> {
        Ok(self.seq_of(id)?.is_some())
    }

    /// Events in log order after the event `after` (from the start when `None`).
    /// The cursor is an id, but ordering is by arrival, so a late-replayed old ULID is not skipped.
    pub fn after(&self, after: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        let from = match after {
            None => 0,
            Some(id) => match self.seq_of(id)? {
                Some(seq) => seq,
                None => return Err(UnknownCursor(id.to_string()).into()),
            },
        };
        let mut stmt = self
            .conn
            .prepare_cached("SELECT json FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let rows = stmt.query_map(params![from, limit as i64], |r| r.get::<_, String>(0))?;
        rows.map(|json| Ok(serde_json::from_str(&json?)?)).collect()
    }

    pub fn all(&self) -> Result<Vec<Event>> {
        self.after(None, usize::MAX >> 1)
    }

    pub fn len(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    fn seq_of(&self, id: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT seq FROM events WHERE id = ?1", [id], |r| r.get(0))
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;

    fn ev(n: u32) -> Event {
        Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: "agent:h/s/a".into(),
            },
            "p",
            Known::TaskCreated(TaskCreated {
                title: format!("t{n}"),
                url: None,
            }),
        )
        .task(format!("task:p/{n}"))
        .into_event()
    }

    #[test]
    fn append_is_idempotent_on_id() {
        let s = Store::memory().unwrap();
        let e = ev(1);
        assert!(s.append(&e).unwrap());
        assert!(!s.append(&e).unwrap(), "replay of the same id is a no-op");
        assert!(!s.append(&e).unwrap());
        assert_eq!(s.len().unwrap(), 1);
        assert_eq!(s.all().unwrap(), vec![e]);
    }

    #[test]
    fn cursor_follows_arrival_not_ulid_order() {
        let s = Store::memory().unwrap();
        let old = ev(0); // older ULID, arrives last (a replayed spool)
        let (a, b) = (ev(1), ev(2));
        for e in [&a, &b, &old] {
            s.append(e).unwrap();
        }
        let ids = |v: Vec<Event>| v.into_iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(
            ids(s.after(Some(&a.id), 10).unwrap()),
            vec![b.id.clone(), old.id.clone()]
        );
        assert_eq!(ids(s.after(Some(&b.id), 10).unwrap()), vec![old.id.clone()]);
        assert!(s.after(Some(&old.id), 10).unwrap().is_empty());
        assert!(s.after(Some("01UNKNOWN"), 10).is_err());
        assert_eq!(s.after(None, 2).unwrap().len(), 2);
    }

    #[test]
    fn unknown_types_survive_the_store() {
        let s = Store::memory().unwrap();
        let mut v = serde_json::to_value(ev(1)).unwrap();
        v["type"] = "res.lease".into();
        let e: Event = serde_json::from_value(v.clone()).unwrap();
        s.append(&e).unwrap();
        assert_eq!(serde_json::to_value(&s.all().unwrap()[0]).unwrap(), v);
    }

    #[test]
    fn a_log_from_before_arrival_times_still_opens() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("hub.db");
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
                 type TEXT NOT NULL, json TEXT NOT NULL);",
            )
            .unwrap();
        let s = Store::open(&db).unwrap();
        s.append(&ev(1)).unwrap();
        let rows = s.all_with_arrival().unwrap();
        assert!(rows[0].1.is_some());
        assert!(Store::open(&db).is_ok(), "the column is added once");
    }

    #[test]
    fn meta_roundtrip() {
        let s = Store::memory().unwrap();
        assert_eq!(s.meta("cursor").unwrap(), None);
        s.set_meta("cursor", "a").unwrap();
        s.set_meta("cursor", "b").unwrap();
        assert_eq!(s.meta("cursor").unwrap().as_deref(), Some("b"));
    }
}
