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

pub struct SessionStore {
    shares: Mutex<HashMap<i32, ShareSession>>,
    connects: Mutex<HashMap<i32, ConnectSession>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            shares: Mutex::new(HashMap::new()),
            connects: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert_share(&self, session: ShareSession) -> i32 {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        self.shares.lock().unwrap().insert(handle, session);
        handle
    }

    pub fn remove_share(&self, handle: i32) -> Option<ShareSession> {
        self.shares.lock().unwrap().remove(&handle)
    }

    pub fn insert_connect(&self, session: ConnectSession) -> i32 {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        self.connects.lock().unwrap().insert(handle, session);
        handle
    }

    pub fn remove_connect(&self, handle: i32) -> Option<ConnectSession> {
        self.connects.lock().unwrap().remove(&handle)
    }
}
