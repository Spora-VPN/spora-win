//! Named-pipe transport for the gRPC control API.
//!
//! The control channel is a named pipe rather than a TCP socket so the OS
//! decides who may talk to the service: the pipe DACL admits SYSTEM,
//! Administrators, and interactively logged-on users (the desktop UI runs
//! unelevated), and remote clients are rejected outright. A localhost TCP
//! port would be reachable by every local security context. Each accepted
//! connection is tagged with the Windows user on the other end, which is
//! what the service serves (see `users.rs`).

use std::io;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;

use futures_util::stream::Stream;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use crate::users::{Conn, UserId, Users};

/// Pipe DACL. `D:P` protects the DACL from inheriting anything wider.
///
/// - SYSTEM and Administrators: full control (`GA`). The service runs as
///   LocalSystem and needs full control to create every pipe instance.
/// - Interactively logged-on users: exactly `0x12008b` =
///   `FILE_GENERIC_READ | FILE_WRITE_DATA`. This is what the desktop UI
///   (unelevated) needs to talk to the service. It deliberately excludes
///   `FILE_APPEND_DATA` (0x4) — which on a named pipe is the same bit as
///   `FILE_CREATE_PIPE_INSTANCE` — so a non-admin process cannot create a
///   competing instance of this pipe to hijack or MITM the control channel.
///   `spora-winui` opens the client end requesting exactly this mask (see
///   its grpc.rs); a plain `GENERIC_WRITE` open would ask for 0x4 and be
///   denied here.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12008b;;;IU)";

/// The Windows user on the other end of `pipe`: the user of the client's
/// process token, as a SID string. Asked right after the connection is
/// accepted, while the client that opened the pipe is there: a process ID
/// is reused only once its process is gone, and a client's connection can
/// only be handed to a process its own user controls (as Tailscale's
/// Windows service tells its users apart).
fn client_user(pipe: &NamedPipeServer) -> io::Result<UserId> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Closes a handle on every way out.
    struct Owned(HANDLE);
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: a handle this function opened and closes once.
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: plain Win32 calls on handles opened here and buffers that
    // outlive them; the SID string is LocalAlloc'd by the conversion and
    // freed below.
    unsafe {
        let mut pid = 0u32;
        if GetNamedPipeClientProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = Owned(process);
        let mut token: HANDLE = 0;
        if OpenProcessToken(process.0, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Owned(token);
        let mut len = 0u32;
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut len);
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64s: TOKEN_USER holds a pointer and wants its alignment.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut wide: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut wide) == 0 {
            return Err(io::Error::last_os_error());
        }
        let n = (0..).take_while(|&i| *wide.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(wide, n));
        LocalFree(wide as _);
        Ok(UserId::from_sid(sid))
    }
}

/// Owns the security descriptor applied to every pipe instance.
struct PipeSecurity {
    attrs: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
    descriptor: *mut core::ffi::c_void,
}

// The descriptor is LocalAlloc'd heap memory only this struct touches.
unsafe impl Send for PipeSecurity {}

impl PipeSecurity {
    fn new(sddl: &str) -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            attrs: windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()
                    as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
            descriptor,
        })
    }

    fn create_instance(&mut self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    &mut self.attrs as *mut _ as *mut core::ffi::c_void,
                )
        }
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.descriptor) };
    }
}

/// Stream of accepted pipe connections for
/// `Server::serve_with_incoming_shutdown`, each tagged with its user.
///
/// The first instance is created eagerly with `FIRST_PIPE_INSTANCE`, so if
/// another process is squatting on the name we fail at startup instead of
/// silently sharing it. After each accept the replacement instance is created
/// before the connection is yielded, so there is no window with nothing
/// listening.
pub fn incoming(
    name: &'static str,
    users: Arc<Users>,
) -> io::Result<impl Stream<Item = io::Result<Conn<NamedPipeServer>>>> {
    let mut security = PipeSecurity::new(PIPE_SDDL)?;
    let first = security.create_instance(name, true)?;

    Ok(futures_util::stream::unfold(
        (security, first, users),
        move |(mut security, server, users)| async move {
            loop {
                match server.connect().await {
                    Ok(()) => {
                        // At once, while the client is surely there. One
                        // that cannot be told is served nothing.
                        let user = client_user(&server)
                            .inspect_err(|e| log::warn!("cannot tell the pipe client's user: {e}"))
                            .ok();
                        // Never yield Err: tonic treats accept errors as
                        // fatal, and a transient instance-creation failure
                        // must not take the whole control server down.
                        let next = loop {
                            match security.create_instance(name, false) {
                                Ok(n) => break n,
                                Err(e) => {
                                    log::error!(
                                        "failed to create pipe instance: {e}; retrying in 1s"
                                    );
                                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                }
                            }
                        };
                        let conn = users.accept(server, user);
                        return Some((Ok(conn), (security, next, users)));
                    }
                    Err(e) => {
                        log::warn!("pipe accept failed: {e}");
                        let _ = server.disconnect();
                    }
                }
            }
        },
    ))
}
