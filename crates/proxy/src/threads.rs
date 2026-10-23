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

/// How many previous cursors a thread keeps for a rollback.
///
/// The turn walks the ring newest-first on a rejected parent id, so this is
/// both the capacity and the maximum number of cheap retries before the
/// expensive rebuild path runs.
pub const REWIND_DEPTH: usize = 5;

/// Resume state for one conversation thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadState {
    pub session_id: String,
    pub parent_message_id: Option<i64>,
    /// Previously committed cursors, oldest first. In memory only: the point is
    /// to survive a parent id the backend discards during this process, and a
    /// restart already falls back to the durable row (which keeps one id).
    pub prev_parents: Vec<i64>,
    /// User/tool messages exchanged so far on this thread. Drives the periodic
    /// tool-format reminder; kept for resume state and diagnostics.
    pub exchanges: usize,
}

impl ThreadState {
    /// The only place that knows the field list.
    ///
    /// Every construction site used to spell the four fields out by hand, so
    /// adding one was a compile-error sweep across four files. A new remote
    /// session always starts with no parent and an empty ring.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            parent_message_id: None,
            prev_parents: Vec::new(),
            exchanges: 0,
        }
    }
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
            // Nothing to walk back to across a restart: only the newest id is
            // durable, so a stale parent goes straight to the rebuild path.
            prev_parents: Vec::new(),
            // Exchange counters restart with a resumed process; they only feed
            // diagnostics and the periodic reminder.
            exchanges: 0,
        })
    }

    fn save(&self, thread_id: &str, state: &ThreadState) {
        // A fresh or rebuilt session has no parent yet, but its session id is
        // the only thing that survives a restart. Dropping the row here made
        // the next process open a *second* remote session for a thread that
        // already had one.
        let _ = self.store.save_thread(
            thread_id,
            &store::ThreadResume {
                session_id: state.session_id.clone(),
                parent_message_id: state.parent_message_id.map(|p| p.to_string()),
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
    /// Seed state without touching durable storage.
    ///
    /// Production reaches this state machine through
    /// [`crate::session::open_session`], which installs a new session with
    /// `replace`. Only tests need a cache entry with no remote session behind
    /// it.
    #[cfg(test)]
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
            let entry = cache
                .entry(key.to_string())
                .or_insert_with(|| ThreadState::new(session_id));
            // A rebuilt session has its own message ids: walking back into the
            // abandoned one would fail exactly the same way.
            if entry.session_id != session_id {
                entry.prev_parents.clear();
            } else if let Some(old) = entry.parent_message_id {
                if old != mid {
                    entry.prev_parents.push(old);
                    let excess = entry.prev_parents.len().saturating_sub(REWIND_DEPTH);
                    if excess > 0 {
                        entry.prev_parents.drain(..excess);
                    }
                }
            }
            entry.parent_message_id = Some(mid);
            entry.session_id = session_id.to_string();
            entry.clone()
        };
        self.persist.save(key, &merged);
    }

    /// Walk the cursor back one commit.
    ///
    /// Returns the parent id the thread had before its newest commit, or `None`
    /// when there is nothing left to fall back to (a fresh process, the first
    /// turn, or a session that was already rebuilt). The caller resends on top
    /// of the returned id; the durable row follows so a restart does not
    /// resurrect the id that just failed.
    pub fn rewind(&self, key: &str) -> Option<i64> {
        let merged = {
            let mut cache = self.cache.lock().expect("threads lock poisoned");
            let entry = cache.get_mut(key)?;
            let prev = entry.prev_parents.pop()?;
            entry.parent_message_id = Some(prev);
            entry.clone()
        };
        self.persist.save(key, &merged);
        merged.parent_message_id
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
            prev_parents: Vec::new(),
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
    fn commit_keeps_a_bounded_history_of_cursors() {
        let persist = Arc::new(MemoryPersist::new());
        let threads = threads_with(persist);
        threads.remember("t1", state("s1", Some(1)));
        for mid in 2..=6 {
            threads.commit("t1", "s1", Some(mid));
        }
        let t = threads.resume("t1").unwrap();
        assert_eq!(t.parent_message_id, Some(6));
        // The oldest entries fall off; the newest REWIND_DEPTH stay.
        assert_eq!(t.prev_parents, vec![1, 2, 3, 4, 5]);
        assert_eq!(t.prev_parents.len(), REWIND_DEPTH);
    }

    #[test]
    fn rewind_walks_the_whole_ring_newest_first() {
        let persist = Arc::new(MemoryPersist::new());
        let threads = threads_with(persist.clone());
        threads.remember("t1", state("s1", Some(1)));
        for mid in 2..=6 {
            threads.commit("t1", "s1", Some(mid));
        }
        // Newest-first: 5, 4, 3, 2, 1 - the whole ring, not just one step.
        for expected in [5, 4, 3, 2, 1] {
            assert_eq!(threads.rewind("t1"), Some(expected));
            assert_eq!(threads.resume("t1").unwrap().parent_message_id, Some(expected));
            assert_eq!(persist.rows()["t1"].parent_message_id, Some(expected));
        }
        // Exhausted: the caller must rebuild instead.
        assert_eq!(threads.rewind("t1"), None);
    }

    #[test]
    fn rewind_stops_at_the_ring_capacity() {
        let threads = threads_with(Arc::new(MemoryPersist::new()));
        threads.remember("t1", state("s1", Some(1)));
        for mid in 2..=20 {
            threads.commit("t1", "s1", Some(mid));
        }
        // Only REWIND_DEPTH commits are recoverable, no matter how long the
        // thread is: 19, 18, 17, 16, 15.
        let walked: Vec<i64> = (0..REWIND_DEPTH)
            .filter_map(|_| threads.rewind("t1"))
            .collect();
        assert_eq!(walked, vec![19, 18, 17, 16, 15]);
        assert_eq!(threads.rewind("t1"), None);
    }

    #[test]
    fn rewind_walks_back_one_commit_and_follows_the_persisted_row() {
        let persist = Arc::new(MemoryPersist::new());
        let threads = threads_with(persist.clone());
        threads.remember("t1", state("s1", Some(10)));
        threads.commit("t1", "s1", Some(11));
        assert_eq!(threads.rewind("t1"), Some(10));
        assert_eq!(threads.resume("t1").unwrap().parent_message_id, Some(10));
        assert_eq!(persist.rows()["t1"].parent_message_id, Some(10));
        // Nothing left to fall back to: the caller must rebuild instead.
        assert_eq!(threads.rewind("t1"), None);
    }

    #[test]
    fn a_rebuilt_session_forgets_the_old_cursors() {
        let threads = threads_with(Arc::new(MemoryPersist::new()));
        threads.remember("t1", state("old", Some(10)));
        threads.commit("t1", "old", Some(11));
        // Message ids are scoped to a session, so the fresh one starts clean.
        threads.commit("t1", "fresh", Some(1));
        assert!(threads.resume("t1").unwrap().prev_parents.is_empty());
        assert_eq!(threads.rewind("t1"), None);
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

    /// A rebuilt session has no parent yet, but its id is the only thing that
    /// survives a restart. The durable adapter used to drop that row, so the
    /// next process opened a *second* remote session for the same thread.
    #[test]
    fn sqlite_persist_keeps_a_session_that_has_no_parent_yet() {
        let store = crate::store::Store::open_memory().expect("in-memory store");
        let persist = SqlitePersist::new(store);

        persist.save("t1", &state("fresh", None));

        let resumed = persist.load("t1").expect("rebuilt session must survive");
        assert_eq!(resumed.session_id, "fresh");
        assert_eq!(resumed.parent_message_id, None);

        // And a later commit still upgrades the same row rather than adding one.
        persist.save("t1", &state("fresh", Some(9)));
        let resumed = persist.load("t1").expect("row");
        assert_eq!(resumed.parent_message_id, Some(9));
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
