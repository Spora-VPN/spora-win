//! One Windows user at a time. Every caller is a Windows user (on Windows,
//! the user of the process on the pipe's other end). The service serves
//! the first user to call, with everything that user has under
//! `users\<SID>\` (see `paths.rs`): their device identity and relay
//! settings, telemetry switch and uploader, records, diagnostic log and
//! crash files. It does so until that user's last connection closes; calls
//! from any other user meanwhile are refused (`PERMISSION_DENIED`, with a
//! sentence the UI shows). Then the next user to call is served.
//!
//! So only one user's state is ever open, and the diagnostic log file moves
//! with the user served ([`spora_service::diag::switch_file`]): one user's
//! uploads and bug reports never carry another's activity. While nobody is
//! served there is no diagnostic log file (the human-readable service log
//! still gets every line). Sessions are tied to their event streams, so the
//! user's sessions end with their connections; the hand-over waits for them
//! to finish going, so their last lines land in their own log.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use log::{info, warn};
use spora_service::diag::{self, DiagConfig};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::Status;
use tonic::transport::server::Connected;

use crate::backend::{self, Backend};
use crate::paths::UserDirs;
use crate::session::SessionStore;

/// What a refused caller is told; the UI shows it as it is.
pub const IN_USE: &str = "Spora is in use by another Windows user on this PC. \
                          It is available again once they quit Spora or sign out.";

/// How long a hand-over waits for the leaving user's sessions to end.
const SESSION_DRAIN: Duration = Duration::from_secs(5);
/// What is queued for a log file is written out before it is let go.
const LOG_DRAIN: Duration = Duration::from_secs(1);

/// A Windows user, by SID (`S-1-5-21-…`); on Linux dev builds whoever
/// reaches the TCP port is the one user `local`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserId(String);

impl UserId {
    /// A SID string as `ConvertSidToStringSidW` writes it.
    #[cfg(windows)]
    pub fn from_sid(sid: String) -> UserId {
        UserId(sid)
    }

    #[cfg(not(windows))]
    pub fn local() -> UserId {
        UserId("local".into())
    }

    /// Also its directory's name: a SID is `S`, digits and dashes.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who is on the other end of a connection (every request on it carries
/// this); `None` when that could not be told, and every call is refused.
#[derive(Clone, Debug)]
pub struct Caller(pub Option<UserId>);

pub struct Users {
    sessions: Arc<SessionStore>,
    /// Open connections per user.
    conns: Mutex<HashMap<UserId, usize>>,
    /// The user served. Async: binding and the hand-over do file I/O and
    /// wait for sessions while holding it.
    current: tokio::sync::Mutex<Option<Current>>,
    me: Weak<Users>,
}

struct Current {
    user: UserId,
    backend: Arc<Backend>,
}

impl Users {
    pub fn new(sessions: Arc<SessionStore>) -> Arc<Users> {
        Arc::new_cyclic(|me| Users {
            sessions,
            conns: Mutex::new(HashMap::new()),
            current: tokio::sync::Mutex::new(None),
            me: me.clone(),
        })
    }

    /// An accepted connection from `user` (`None`: unknown), counted until
    /// it is dropped.
    pub fn accept<S>(&self, io: S, user: Option<UserId>) -> Conn<S> {
        let ticket = user.clone().map(|user| {
            *self.conns.lock().unwrap().entry(user.clone()).or_default() += 1;
            Ticket {
                users: self.me.clone(),
                user,
            }
        });
        Conn {
            io,
            caller: Caller(user),
            _ticket: ticket,
        }
    }

    fn connections(&self, user: &UserId) -> usize {
        self.conns.lock().unwrap().get(user).copied().unwrap_or(0)
    }

    /// The caller's state, serving them if nobody is served (or the user
    /// served has gone and the hand-over is still to come). Every call goes
    /// through here first.
    pub async fn enter<T>(&self, request: &tonic::Request<T>) -> Result<Arc<Backend>, Status> {
        let Some(user) = request
            .extensions()
            .get::<Caller>()
            .and_then(|c| c.0.clone())
        else {
            return Err(Status::unauthenticated(
                "Spora could not tell which Windows user is asking.",
            ));
        };
        let mut current = self.current.lock().await;
        if let Some(c) = current.as_ref() {
            if c.user == user {
                return Ok(c.backend.clone());
            }
            if self.connections(&c.user) > 0 {
                return Err(Status::permission_denied(IN_USE));
            }
            self.release(&mut current, SESSION_DRAIN).await;
        }
        let backend = tokio::task::spawn_blocking({
            let user = user.clone();
            move || serve(&user)
        })
        .await
        .map_err(|e| Status::internal(format!("opening the user's state: {e}")))?;
        *current = Some(Current {
            user,
            backend: backend.clone(),
        });
        Ok(backend)
    }

    /// `user`'s last connection closed: if they are the one served, the
    /// next caller may be.
    async fn left(self: Arc<Self>, user: UserId) {
        let mut current = self.current.lock().await;
        if current.as_ref().is_some_and(|c| c.user == user) && self.connections(&user) == 0 {
            self.release(&mut current, SESSION_DRAIN).await;
        }
    }

    /// The service stops: let the user served go (one last upload pass).
    /// Sessions are already stopped.
    pub async fn shutdown(&self) {
        let mut current = self.current.lock().await;
        self.release(&mut current, Duration::from_secs(1)).await;
    }

    async fn release(&self, current: &mut Option<Current>, drain: Duration) {
        let Some(c) = current.take() else { return };
        if !self.sessions.idle(drain).await {
            warn!("sessions still ending after {drain:?}; letting the user go anyway");
        }
        let user = c.user;
        let backend = c.backend;
        let _ = tokio::task::spawn_blocking(move || {
            backend.close();
            diag::switch_file(None, LOG_DRAIN);
        })
        .await;
        // The human-readable log only: no diagnostic log file now.
        info!("no longer serving Windows user {user}");
    }
}

/// Open `user`'s state and move the diagnostic log to their directory
/// (file I/O: a blocking context).
fn serve(user: &UserId) -> Arc<Backend> {
    let dirs = UserDirs::of(user);
    if let Err(e) = dirs.ensure() {
        log::error!("directories of Windows user {user}: {e}");
    }
    if !diag::switch_file(
        Some(DiagConfig::in_dir(dirs.diag()).with_platform(backend::platform())),
        LOG_DRAIN,
    ) {
        warn!("no diagnostic log for Windows user {user}");
    }
    info!("serving Windows user {user}");
    let backend = Arc::new(Backend::new(dirs));
    // The device identity, the relay access and the uploader: opened now
    // so the relay token is renewed and uploads run. Local files only; a
    // failure is retried when something needs it.
    backend.telemetry();
    backend
}

/// Counts its connection; the last one of a user to go lets them go.
struct Ticket {
    users: Weak<Users>,
    user: UserId,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let Some(users) = self.users.upgrade() else {
            return;
        };
        let last = {
            let mut conns = users.conns.lock().unwrap();
            match conns.get_mut(&self.user) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                _ => {
                    conns.remove(&self.user);
                    true
                }
            }
        };
        // Connections drop on the runtime (hyper's connection task); with
        // none at hand, the next caller's `enter` does the hand-over.
        if last && let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(users.left(self.user.clone()));
        }
    }
}

/// An accepted connection: the transport (named pipe, or TCP on dev
/// builds), who is on the other end, and its place in the count.
pub struct Conn<S> {
    io: S,
    caller: Caller,
    _ticket: Option<Ticket>,
}

impl<S> Connected for Conn<S> {
    type ConnectInfo = Caller;
    fn connect_info(&self) -> Caller {
        self.caller.clone()
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Conn<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Conn<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// Dev builds: the control port's connections, all from the one user.
#[cfg(not(windows))]
pub fn tcp_incoming(
    listener: tokio::net::TcpListener,
    users: Arc<Users>,
) -> impl futures_util::Stream<Item = io::Result<Conn<tokio::net::TcpStream>>> {
    futures_util::stream::unfold((listener, users), |(listener, users)| async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    let conn = users.accept(stream, Some(UserId::local()));
                    return Some((Ok(conn), (listener, users)));
                }
                // Never yield Err: tonic treats accept errors as fatal.
                Err(e) => {
                    warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str) -> Option<UserId> {
        Some(UserId(name.into()))
    }

    fn refusal(r: Result<Arc<Backend>, Status>) -> Status {
        match r {
            Ok(_) => panic!("served"),
            Err(s) => s,
        }
    }

    /// A request as it arrives on `conn`.
    fn on<S>(conn: &Conn<S>) -> tonic::Request<()> {
        let mut r = tonic::Request::new(());
        r.extensions_mut().insert(conn.connect_info());
        r
    }

    #[tokio::test]
    async fn one_user_at_a_time() {
        let data = std::env::temp_dir().join(format!("wincore-users-{}", std::process::id()));
        // SAFETY: the only test in this crate that reads the environment
        // after start (the data directory) is this one.
        unsafe { std::env::set_var("SPORA_WINCORE_DATA", &data) };
        let users = Users::new(Arc::new(SessionStore::new()));

        let a = users.accept((), user("alice"));
        let alice = users.enter(&on(&a)).await.unwrap();
        assert!(Arc::ptr_eq(&alice, &users.enter(&on(&a)).await.unwrap()));
        assert!(alice.dirs.service().starts_with(data.join("users/alice")));
        assert_eq!(diag::log_dir(), Some(data.join("users/alice/diag")));

        // Another user, or one nobody could tell: refused while she is served.
        let b = users.accept((), user("bob"));
        let refused = refusal(users.enter(&on(&b)).await);
        assert_eq!(refused.code(), tonic::Code::PermissionDenied);
        assert_eq!(refused.message(), IN_USE);
        let unknown = users.accept((), None);
        let refused = refusal(users.enter(&on(&unknown)).await);
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);

        // A second connection of hers keeps her served when the first goes.
        let a2 = users.accept((), user("alice"));
        drop(a);
        tokio::task::yield_now().await;
        assert!(Arc::ptr_eq(&alice, &users.enter(&on(&a2)).await.unwrap()));
        assert!(users.enter(&on(&b)).await.is_err());

        // Her last one gone: the next caller is served, with his own state
        // and log, and she is refused now.
        drop(a2);
        let bob = users.enter(&on(&b)).await.unwrap();
        assert!(bob.dirs.service().starts_with(data.join("users/bob")));
        assert_eq!(diag::log_dir(), Some(data.join("users/bob/diag")));
        assert!(alice.telemetry().is_none(), "her state is closed");
        let a3 = users.accept((), user("alice"));
        assert_eq!(
            refusal(users.enter(&on(&a3)).await).code(),
            tonic::Code::PermissionDenied
        );

        users.shutdown().await;
        assert_eq!(diag::log_dir(), None);
        let _ = std::fs::remove_dir_all(&data);
    }
}
