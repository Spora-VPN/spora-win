//! Named-pipe transport for the gRPC control API.
//!
//! The control channel is a named pipe rather than a TCP socket so the OS
//! decides who may talk to the service: the pipe DACL admits SYSTEM,
//! Administrators, and interactively logged-on users (the desktop UI runs
//! unelevated), and remote clients are rejected outright. A localhost TCP
//! port would be reachable by every local security context.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::stream::Stream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tonic::transport::server::Connected;

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

/// A connected pipe instance, wrapped so tonic can treat it as an accepted
/// connection.
pub struct PipeConn(NamedPipeServer);

impl Connected for PipeConn {
    type ConnectInfo = ();
    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for PipeConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for PipeConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
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
/// `Server::serve_with_incoming_shutdown`.
///
/// The first instance is created eagerly with `FIRST_PIPE_INSTANCE`, so if
/// another process is squatting on the name we fail at startup instead of
/// silently sharing it. After each accept the replacement instance is created
/// before the connection is yielded, so there is no window with nothing
/// listening.
pub fn incoming(name: &'static str) -> io::Result<impl Stream<Item = io::Result<PipeConn>>> {
    let mut security = PipeSecurity::new(PIPE_SDDL)?;
    let first = security.create_instance(name, true)?;

    Ok(futures_util::stream::unfold(
        (security, first),
        move |(mut security, server)| async move {
            loop {
                match server.connect().await {
                    Ok(()) => {
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
                        return Some((Ok(PipeConn(server)), (security, next)));
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
