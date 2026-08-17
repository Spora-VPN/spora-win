use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::route::RouteManager;

static NEXT_HANDLE: AtomicI32 = AtomicI32::new(1);

pub struct ShareSession {
    pub cancel: CancellationToken,
    pub task: JoinHandle<()>,
}

#[allow(dead_code)]
pub struct ConnectSession {
    pub cancel: CancellationToken,
    pub task: JoinHandle<()>,
    pub routes: RouteManager,
}

/// State behind a single lock so an insert and `shutdown_all` can never
/// interleave — otherwise a session created while the service is shutting
/// down could be registered after the store was drained and leak (its tunnel
/// and routes never torn down).
struct Inner {
    closed: bool,
    shares: HashMap<i32, ShareSession>,
    connects: HashMap<i32, ConnectSession>,
}

pub struct SessionStore {
    inner: Mutex<Inner>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                closed: false,
                shares: HashMap::new(),
                connects: HashMap::new(),
            }),
        }
    }

    /// Registers a share session, returning its handle. Returns `None` if the
    /// store is shutting down; the caller must then tear the session down
    /// itself (see the RPC handlers).
    pub fn insert_share(&self, session: ShareSession) -> Option<i32> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return None;
        }
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        inner.shares.insert(handle, session);
        Some(handle)
    }

    pub fn remove_share(&self, handle: i32) -> Option<ShareSession> {
        self.inner.lock().unwrap().shares.remove(&handle)
    }

    /// See [`insert_share`]. `None` means shutting down.
    pub fn insert_connect(&self, session: ConnectSession) -> Option<i32> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return None;
        }
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        inner.connects.insert(handle, session);
        Some(handle)
    }

    pub fn remove_connect(&self, handle: i32) -> Option<ConnectSession> {
        self.inner.lock().unwrap().connects.remove(&handle)
    }

    /// Marks the store closed (so future inserts refuse) and cancels every
    /// live session. Dropping a `ConnectSession` drops its `RouteManager`,
    /// which removes the routes it added. Sessions are dropped outside the
    /// lock because `RouteManager`'s drop touches the Win32 routing table.
    pub fn shutdown_all(&self) {
        let (shares, connects) = {
            let mut inner = self.inner.lock().unwrap();
            inner.closed = true;
            (
                inner.shares.drain().collect::<Vec<_>>(),
                inner.connects.drain().collect::<Vec<_>>(),
            )
        };
        for (_, s) in &shares {
            s.cancel.cancel();
            s.task.abort();
        }
        for (_, s) in &connects {
            s.cancel.cancel();
            s.task.abort();
        }
        // `shares`/`connects` drop here (outside the lock), running route cleanup.
    }
}
