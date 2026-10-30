use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spora_core::identity::{Credentials, ShareId};
use spora_service::Telemetry;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

static NEXT_HANDLE: AtomicI32 = AtomicI32::new(1);

pub struct ShareSession {
    pub cancel: CancellationToken,
    pub task: JoinHandle<()>,
    pub share_id: ShareId,
    /// What the share's registrars present: renewed relay tokens go here.
    pub credentials: Credentials,
    /// Some of its relays are Spora's (they want the device's relay token).
    pub uses_spora: bool,
    /// The user's service layer it was registered with (renewed tokens).
    pub telemetry: Option<Arc<Telemetry>>,
}

#[allow(dead_code)]
pub struct ConnectSession {
    pub cancel: CancellationToken,
    pub task: JoinHandle<()>,
    /// The host integration (adapter, routes, resolver); `None` when the
    /// session runs without a tunnel (Linux dev builds). Dropping it undoes
    /// every host change.
    pub vpn: Option<Arc<spora_vpn::Session>>,
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
    /// Share and connect handler tasks still running ([`SessionStore::track`]).
    live: tokio::sync::watch::Sender<usize>,
}

/// Held by a share or connect handler task for its whole life.
pub struct LiveTask(Arc<SessionStore>);

impl Drop for LiveTask {
    fn drop(&mut self) {
        self.0.live.send_modify(|n| *n -= 1);
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                closed: false,
                shares: HashMap::new(),
                connects: HashMap::new(),
            }),
            live: tokio::sync::watch::Sender::new(0),
        }
    }

    /// Count a handler task as live until the guard drops: its session is
    /// gone only once the task has ended.
    pub fn track(self: &Arc<Self>) -> LiveTask {
        self.live.send_modify(|n| *n += 1);
        LiveTask(self.clone())
    }

    /// Wait (up to `timeout`) until no handler task is live; whether none is.
    pub async fn idle(&self, timeout: Duration) -> bool {
        let mut rx = self.live.subscribe();
        tokio::time::timeout(timeout, rx.wait_for(|n| *n == 0))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Registers a share session, returning its handle. Returns `None` if the
    /// store is shutting down; the caller must then tear the session down
    /// itself (see the RPC handlers). From here on the share gets every
    /// relay token the service renews.
    pub fn insert_share(&self, session: ShareSession) -> Option<i32> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return None;
        }
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        if let Some(t) = &session.telemetry {
            t.register_share(session.share_id, session.credentials.clone());
        }
        inner.shares.insert(handle, session);
        Some(handle)
    }

    pub fn remove_share(&self, handle: i32) -> Option<ShareSession> {
        let mut inner = self.inner.lock().unwrap();
        let session = inner.shares.remove(&handle)?;
        unregister(&inner, &session);
        Some(session)
    }

    /// Some running share is on Spora's relays.
    pub fn any_share_uses_spora(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .shares
            .values()
            .any(|s| s.uses_spora)
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
    /// live session. Dropping a `ConnectSession` drops its spora-vpn session,
    /// which undoes the routes and resolver it installed and closes the
    /// adapter. Sessions are dropped outside the lock because that touches
    /// the host's routing table and resolver settings.
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
            if let Some(t) = &s.telemetry {
                t.unregister_share(&s.share_id);
            }
        }
        for (_, s) in &connects {
            s.cancel.cancel();
            s.task.abort();
        }
        // `shares`/`connects` drop here (outside the lock), running route cleanup.
    }
}

/// A share session left the store: it no longer gets renewed tokens. The
/// service keys shares by share id, so if another instance of the same share
/// is still in the store (a quick off→on, the old one still going away), it
/// is registered again rather than left without renewals.
fn unregister(inner: &Inner, gone: &ShareSession) {
    let Some(t) = &gone.telemetry else {
        return;
    };
    t.unregister_share(&gone.share_id);
    if let Some(other) = inner.shares.values().find(|s| s.share_id == gone.share_id) {
        t.register_share(other.share_id, other.credentials.clone());
    }
}
