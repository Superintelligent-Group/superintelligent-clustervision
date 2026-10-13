//! One explicitly owned catalog connection for a read/discovery operation.
use super::{Publication, SessionRef};
use crate::Harness;

/// No transaction spans filesystem scans. Separate operations reopen the current catalog path;
/// this owner has no process-global connection or schema cache.
pub(crate) struct Reader {
    #[cfg(feature = "sqlite")]
    conn: rusqlite::Connection,
}

impl Reader {
    pub(crate) fn open() -> Option<Self> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::open_db().map(|conn| Self { conn })
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    pub(crate) fn last_full_sync(&self) -> Option<i64> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::last_full_sync_in(&self.conn)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    pub(crate) fn probe_stale(&self) -> Option<Vec<Harness>> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::probe_stale_in(&self.conn)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    pub(crate) fn all_sessions(&self) -> Option<Vec<SessionRef>> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::all_sessions_in(&self.conn)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    pub(crate) fn persist_discovery(&mut self, prune: bool) {
        #[cfg(feature = "sqlite")]
        {
            crate::discover_cache::persist_in(&mut self.conn, prune);
        }
        #[cfg(not(feature = "sqlite"))]
        {
            crate::discover_cache::persist(prune);
        }
    }

    pub(crate) fn publish_all(&mut self, rows: &[Publication<'_>]) {
        #[cfg(feature = "sqlite")]
        {
            super::imp::publish_all_in(&mut self.conn, rows);
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = rows;
        }
    }
}
