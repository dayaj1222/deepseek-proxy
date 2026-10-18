//! SQLite persistence: thread resume state and API keys.
//!
//! Schema mirrors the Python proxy's `dispatch.db` closely enough that the two
//! can share a database during the parity phase:
//!
//! ```sql
//! CREATE TABLE threads (thread_id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
//!                      parent_message_id TEXT, updated_at INTEGER);
//! CREATE TABLE api_keys (id TEXT PRIMARY KEY, key_hash TEXT UNIQUE NOT NULL,
//!                        key_prefix TEXT NOT NULL, name TEXT NOT NULL,
//!                        created_at INTEGER);
//! ```
//!
//! `threads.parent_message_id` is stored as TEXT because the values arrive as
//! strings in some responses.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Resume state for one conversation thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadResume {
    pub session_id: String,
    pub parent_message_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Thread-safe handle to the proxy database.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    #[allow(dead_code)] // test helper
    pub fn open_memory() -> Result<Self, StoreError> {
        Self::open_path(":memory:")
    }

    /// Open (and migrate) a database at `path`.
    pub fn open_path(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS threads (
                 thread_id         TEXT PRIMARY KEY,
                 session_id        TEXT NOT NULL,
                 parent_message_id TEXT,
                 updated_at        INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS api_keys (
                 id         TEXT PRIMARY KEY,
                 key_hash   TEXT UNIQUE NOT NULL,
                 key_prefix TEXT NOT NULL,
                 name       TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS thread_bindings (
                 thread_id  TEXT PRIMARY KEY,
                 email      TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )?;
        Ok(())
    }

    /// Account email a thread is bound to (survives restarts).
    pub fn get_binding(&self, thread_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let row = conn
            .query_row(
                "SELECT email FROM thread_bindings WHERE thread_id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row)
    }

    pub fn bind(&self, thread_id: &str, email: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute(
            "INSERT INTO thread_bindings (thread_id, email, updated_at)
             VALUES (?1, ?2, unixepoch())
             ON CONFLICT(thread_id) DO UPDATE SET
                 email = excluded.email, updated_at = excluded.updated_at",
            params![thread_id, email],
        )?;
        Ok(())
    }

    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadResume>, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let row = conn
            .query_row(
                "SELECT session_id, parent_message_id FROM threads WHERE thread_id = ?1",
                params![thread_id],
                |r| {
                    Ok(ThreadResume {
                        session_id: r.get(0)?,
                        parent_message_id: r.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn save_thread(
        &self,
        thread_id: &str,
        resume: &ThreadResume,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute(
            "INSERT INTO threads (thread_id, session_id, parent_message_id, updated_at)
             VALUES (?1, ?2, ?3, unixepoch())
             ON CONFLICT(thread_id) DO UPDATE SET
                 session_id = excluded.session_id,
                 parent_message_id = excluded.parent_message_id,
                 updated_at = excluded.updated_at",
            params![thread_id, resume.session_id, resume.parent_message_id],
        )?;
        Ok(())
    }

    #[allow(dead_code)] // used by tests and future admin UI
    pub fn delete_thread(&self, thread_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute("DELETE FROM threads WHERE thread_id = ?1", params![thread_id])?;
        Ok(())
    }

    /// Register an API key. Only the hash is stored.
    pub fn insert_api_key(&self, name: &str, key: &str) -> Result<(), StoreError> {
        let hash = hash_key(key);
        let prefix = key.chars().take(8).collect::<String>();
        let id = format!("key-{hash:.12}");
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute(
            "INSERT INTO api_keys (id, key_hash, key_prefix, name, created_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![id, hash, prefix, name],
        )?;
        Ok(())
    }

    /// True when `key` matches a stored hash. Always true when no keys exist,
    /// so a fresh install is not locked out.
    pub fn verify_api_key(&self, key: &str) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))?;
        if count == 0 {
            return Ok(true);
        }
        let hash = hash_key(key);
        let hit: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM api_keys WHERE key_hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .optional()?;
        Ok(hit.is_some())
    }
}

pub fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_roundtrip() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.get_thread("t1").unwrap(), None);

        s.save_thread(
            "t1",
            &ThreadResume {
                session_id: "sess".into(),
                parent_message_id: Some("42".into()),
            },
        )
        .unwrap();

        let got = s.get_thread("t1").unwrap().unwrap();
        assert_eq!(got.session_id, "sess");
        assert_eq!(got.parent_message_id.as_deref(), Some("42"));

        s.save_thread(
            "t1",
            &ThreadResume {
                session_id: "sess".into(),
                parent_message_id: Some("43".into()),
            },
        )
        .unwrap();
        assert_eq!(
            s.get_thread("t1").unwrap().unwrap().parent_message_id.as_deref(),
            Some("43"),
            "upsert must overwrite"
        );
    }

    #[test]
    fn api_keys_open_when_none_registered() {
        let s = Store::open_memory().unwrap();
        assert!(s.verify_api_key("anything").unwrap());
    }

    #[test]
    fn api_key_accepts_registered_and_rejects_others() {
        let s = Store::open_memory().unwrap();
        s.insert_api_key("test", "sk-secret-123").unwrap();
        assert!(s.verify_api_key("sk-secret-123").unwrap());
        assert!(!s.verify_api_key("sk-wrong").unwrap());
    }

    #[test]
    fn hash_is_stable_and_hides_input() {
        let h = hash_key("sk-abc");
        assert_eq!(h, hash_key("sk-abc"));
        assert_ne!(h, hash_key("sk-abd"));
        assert!(!h.contains("sk-abc"));
    }
}
