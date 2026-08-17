use tonic::transport::Channel;

pub mod proto {
    tonic::include_proto!("spora");
}

use proto::spora_client::SporaClient;
use proto::*;

/// Control-channel endpoint of the wincore service on Windows. Must match
/// the server constant in spora-wincore/src/main.rs.
#[cfg(windows)]
const PIPE_NAME: &str = r"\\.\pipe\spora-wincore";

/// Opens the wincore control pipe requesting exactly
/// `FILE_GENERIC_READ | FILE_WRITE_DATA` (0x12008b). Tokio's
/// `ClientOptions` would ask for full `GENERIC_WRITE`, which includes
/// `FILE_APPEND_DATA` (== `FILE_CREATE_PIPE_INSTANCE`); the service's pipe
/// DACL withholds that bit from interactive users so a non-admin cannot
/// stand up a rival instance, so we must request the narrow mask here to be
/// admitted at all. The handle is opened overlapped with
/// `SECURITY_IDENTIFICATION`, matching what tokio requires and requests.
#[cfg(windows)]
fn open_pipe_client(
    name: &str,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_WRITE_DATA, OPEN_EXISTING,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ | FILE_WRITE_DATA,
            0, // no sharing
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            0, // no template
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid, overlapped pipe handle we just opened and
    // no longer touch; NamedPipeClient takes ownership and closes it on drop
    // (and registers it with tokio's IOCP, hence the fallible result).
    unsafe { tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle(handle as _) }
}

#[derive(Clone)]
pub struct GrpcClient {
    inner: SporaClient<Channel>,
}

impl GrpcClient {
    /// Connect to the named pipe of the wincore Windows service. Fails until
    /// the service is up (or if all pipe instances are momentarily busy);
    /// callers retry.
    #[cfg(windows)]
    pub async fn connect() -> Result<Self, String> {
        use hyper_util::rt::TokioIo;

        // The URI is required by tonic but never resolved — the connector
        // ignores it and opens the pipe.
        let channel = tonic::transport::Endpoint::from_static("http://spora-wincore")
            .connect_with_connector(tower::service_fn(|_: tonic::transport::Uri| async {
                open_pipe_client(PIPE_NAME).map(TokioIo::new)
            }))
            .await
            .map_err(|e| format!("gRPC connect failed: {e}"))?;
        Ok(Self {
            inner: SporaClient::new(channel),
        })
    }

    /// Linux dev builds: wincore runs as a plain child process serving TCP.
    #[cfg(not(windows))]
    pub async fn connect() -> Result<Self, String> {
        let channel = Channel::from_static("http://[::1]:50051")
            .connect()
            .await
            .map_err(|e| format!("gRPC connect failed: {e}"))?;
        Ok(Self {
            inner: SporaClient::new(channel),
        })
    }

    pub async fn make_secret_key(&self) -> Result<String, String> {
        let resp = self
            .inner
            .clone()
            .make_secret_key(MakeSecretKeyRequest {})
            .await
            .map_err(|e| format!("MakeSecretKey RPC failed: {e}"))?;
        Ok(resp.into_inner().key)
    }

    pub async fn share(
        &self,
        key: String,
    ) -> Result<tonic::Streaming<ShareEvent>, String> {
        let resp = self
            .inner
            .clone()
            // Connection-log fields at their defaults: logging enabled in the
            // service's default directory with default retention.
            .share(ShareRequest {
                key,
                disable_conn_log: false,
                conn_log_dir: String::new(),
                conn_log_retention_days: 0,
                conn_log_sessions_only: false,
            })
            .await
            .map_err(|e| format!("Share RPC failed: {e}"))?;
        Ok(resp.into_inner())
    }

    pub async fn connect_vpn(
        &self,
        url: String,
    ) -> Result<tonic::Streaming<ConnectEvent>, String> {
        let resp = self
            .inner
            .clone()
            .connect(ConnectRequest { url })
            .await
            .map_err(|e| format!("Connect RPC failed: {e}"))?;
        Ok(resp.into_inner())
    }

    pub async fn stop_share(&self, handle: i32) -> Result<(), String> {
        self.inner
            .clone()
            .stop_share(StopShareRequest { handle })
            .await
            .map_err(|e| format!("StopShare RPC failed: {e}"))?;
        Ok(())
    }

    pub async fn disconnect(&self, handle: i32) -> Result<(), String> {
        self.inner
            .clone()
            .disconnect(DisconnectRequest { handle })
            .await
            .map_err(|e| format!("Disconnect RPC failed: {e}"))?;
        Ok(())
    }

    /// Stops the wincore process. Only used on Linux dev builds, where the
    /// UI spawned wincore itself; on Windows the service outlives the UI.
    #[cfg(not(windows))]
    pub async fn shutdown(&self) -> Result<(), String> {
        self.inner
            .clone()
            .shutdown(ShutdownRequest {})
            .await
            .map_err(|e| format!("Shutdown RPC failed: {e}"))?;
        Ok(())
    }
}
