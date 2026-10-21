//! Thread state: resume cursor, turn lock and persistence behind one seam.
//!
//! A **thread** used to be spread over three places in `main.rs`: an in-memory
//! cache map, a per-thread turn-lock map, and SQLite through `store`. Every
//! caller re-implemented the cache-then-store fallback, and the rules about
//! which value wins were stated in three comments that could drift.
//!
//! This module is the single owner. `Persist` is the seam; `SqlitePersist` is
//! the production adapter and `MemoryPersist` the test one. Two adapters make
//! the seam real rather than hypothetical.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::store::{self, Store};

/// Resume state for one conversation thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadState {
    pub session_id: String,
    pub parent_message_id: Option<i64>,
    /// User/tool messages exchanged so far on this thread. Drives the periodic
    /// tool-format reminder; kept for resume state and diagnostics.
    pub exchanges: usize,
}

/// Durable side of the seam: where resume state survives a restart.
pub trait Persist: Send + Sync {
    fn load(&self, thread_id: &str) -> Option<ThreadState>;
    fn save(&self, thread_id: &str, state: &ThreadState);
}

/// Production adapter: SQLite through [`store::Store`].
pub struct SqlitePersist {
    store: Store,
}

impl SqlitePersist {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

impl Persist for SqlitePersist {
    fn load(&self, thread_id: &str) -> Option<ThreadState> {
        let row = self.store.get_thread(thread_id).ok().flatten()?;
        Some(ThreadState {
            session_id: row.session_id,
            parent_message_id: row.parent_message_id.and_then(|p| p.parse().ok()),
            // Exchange counters restart with a resumed process; they only feed
            // diagnostics and the periodic reminder.
            exchanges: 0,
        })
    }

    fn save(&self, thread_id: &str, state: &ThreadState) {
        let Some(parent) = state.parent_message_id else {
            return;
        };
        let _ = self.store.save_thread(
            thread_id,
            &store::ThreadResume {
                session_id: state.session_id.clone(),
                parent_message_id: Some(parent.to_string()),
            },
        );
    }
}

/// Test adapter: nothing outlives the process. Two adapters behind [`Persist`]
/// are what make the seam real rather than hypothetical.
#[derive(Default)]
#[allow(dead_code)] // constructed by tests only
pub struct MemoryPersist {
    rows: Mutex<HashMap<String, ThreadState>>,
}

#[allow(dead_code)] // constructed by tests only
impl MemoryPersist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything written so far, for assertions.
    pub fn rows(&self) -> HashMap<String, ThreadState> {
        self.rows.lock().expect("persist lock poisoned").clone()
    }
}

impl Persist for MemoryPersist {
    fn load(&self, thread_id: &str) -> Option<ThreadState> {
        self.rows
            .lock()
            .expect("persist lock poisoned")
            .get(thread_id)
            .cloned()
    }

    fn save(&self, thread_id: &str, state: &ThreadState) {
        self.rows
            .lock()
            .expect("persist lock poisoned")
            .insert(thread_id.to_string(), state.clone());
    }
}

type Locks = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// The thread module: cache, lock and persistence owned together.
pub struct Threads {
    cache: Mutex<HashMap<String, ThreadState>>,
    locks: Locks,
    persist: Arc<dyn Persist>,
}

impl Threads {
    pub fn new(persist: Arc<dyn Persist>) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            persist,
        }
    }

    /// Resume state for `key`: memory first, then the durable adapter. This is
    /// the cache-then-store fallback that used to be open-coded at the call
    /// site, now stated once.
    pub fn resume(&self, key: &str) -> Option<ThreadState> {
        if let Some(t) = self
            .cache
            .lock()
            .expect("threads lock poisoned")
            .get(key)
            .cloned()
        {
            return Some(t);
        }
        let loaded = self.persist.load(key)?;
        self.cache
            .lock()
            .expect("threads lock poisoned")
            .insert(key.to_string(), loaded.clone());
        Some(loaded)
    }

    /// Take the turn lock for `key`, creating it on first use.
    ///
    /// Held from before the parent id is read until the whole turn (including
    /// every correction turn) is finished.
    pub async fn lock(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.locks.lock().expect("threads lock poisoned");
            map.entry(key.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    /// Cache only. Used for a brand-new remote session, which has no cursor to
    /// persist yet.
    pub fn remember(&self, key: &str, state: ThreadState) {
        self.cache
            .lock()
            .expect("threads lock poisoned")
            .insert(key.to_string(), state);
    }

    /// Commit the newest upstream message. The cursor is never written on a
    /// failed attempt: a throttled turn still carries a `response_message_id`
    /// for a message the backend discarded, and parenting to it makes DeepSeek
    /// answer `{"code":26}`.
    pub fn commit(&self, key: &str, session_id: &str, cursor: Option<i64>) {
        let Some(mid) = cursor else { return };
        let merged = {
            let mut cache = self.cache.lock().expect("threads lock poisoned");
            let entry = cache.entry(key.to_string()).or_insert_with(|| ThreadState {
                session_id: session_id.to_string(),
                parent_message_id: None,
                exchanges: 0,
            });
            entry.parent_message_id = Some(mid);
            entry.session_id = session_id.to_string();
            entry.clone()
        };
        self.persist.save(key, &merged);
    }

    /// Replace the cached session (after a context-limit rebuild) and reset the
    /// durable resume state to match.
    pub fn replace(&self, key: &str, fresh: ThreadState) {
        self.cache
            .lock()
            .expect("threads lock poisoned")
            .insert(key.to_string(), fresh.clone());
        self.persist.save(key, &fresh);
    }

    /// Threads currently cached in memory (`/stats`).
    pub fn cached_len(&self) -> usize {
        self.cache.lock().expect("threads lock poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn threads_with(persist: Arc<dyn Persist>) -> Threads {
        Threads::new(persist)
    }

    fn state(session: &str, parent: Option<i64>) -> ThreadState {
        ThreadState {
            session_id: session.into(),
            parent_message_id: parent,
            exchanges: 0,
        }
    }

    #[test]
    fn resume_prefers_memory_then_durable() {
        let persist = Arc::new(MemoryPersist::new());
        persist.save("t1", &state("s", Some(7)));
        let threads = threads_with(persist);
        let resumed = threads.resume("t1").expect("durable row");
        assert_eq!(resumed.parent_message_id, Some(7));
        // Second read comes from the cache, not the adapter.
        assert_eq!(threads.resume("t1"), Some(resumed));
    }

    #[test]
    fn commit_advances_to_the_newest_message() {
        // The regression this guards: after a correction turn the client's next
        // message must be a child of the CORRECTED answer.
        let persist = Arc::new(MemoryPersist::new());
        let threads = threads_with(persist.clone());
        threads.remember("t1", state("s1", Some(100)));
        threads.commit("t1", "s1", Some(101));
        assert_eq!(threads.resume("t1").unwrap().parent_message_id, Some(101));
        threads.commit("t1", "s1", Some(102));
        assert_eq!(threads.resume("t1").unwrap().parent_message_id, Some(102));
        assert_eq!(persist.rows()["t1"].parent_message_id, Some(102));
    }

    #[test]
    fn commit_without_cursor_leaves_state_untouched() {
        let persist = Arc::new(MemoryPersist::new());
        let threads = threads_with(persist.clone());
        threads.remember("t1", state("s1", Some(7)));
        threads.commit("t1", "s1", None);
        assert_eq!(threads.resume("t1").unwrap().parent_message_id, Some(7));
        assert!(persist.rows().is_empty());
    }

    #[test]
    fn replace_follows_a_rebuilt_session() {
        let persist = Arc::new(MemoryPersist::new());
        let threads = threads_with(persist.clone());
        threads.remember("t1", state("old", Some(50)));
        threads.replace("t1", state("fresh", None));
        let t = threads.resume("t1").unwrap();
        assert_eq!(t.session_id, "fresh");
        assert_eq!(t.parent_message_id, None);
    }

    #[tokio::test]
    async fn lock_serialises_two_turns_on_one_thread() {
        let threads = Arc::new(threads_with(Arc::new(MemoryPersist::new())));
        let first = threads.lock("t1").await;
        let second = threads.clone();
        let waiter = tokio::spawn(async move {
            let _guard = second.lock("t1").await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "second turn must wait for the first");
        drop(first);
        waiter.await.unwrap();
    }
}
