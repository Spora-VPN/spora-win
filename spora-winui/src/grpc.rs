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

    /// A new share on the relays the service's settings name.
    pub async fn make_share(&self) -> Result<MakeShareResponse, String> {
        let resp = self
            .inner
            .clone()
            .make_share(MakeShareRequest {})
            .await
            .map_err(|e| format!("MakeShare RPC failed: {e}"))?;
        Ok(resp.into_inner())
    }

    pub async fn share(&self, share: Vec<u8>) -> Result<tonic::Streaming<ShareEvent>, String> {
        let resp = self
            .inner
            .clone()
            // Connection-log fields at their defaults: logging enabled in the
            // service's default directory with default retention.
            .share(ShareRequest {
                share,
                disable_conn_log: false,
                conn_log_dir: String::new(),
                conn_log_retention_days: 0,
                conn_log_sessions_only: false,
            })
            .await
            .map_err(|e| format!("Share RPC failed: {e}"))?;
        Ok(resp.into_inner())
    }

    pub async fn relay_settings(&self) -> Result<RelaySettings, String> {
        Ok(self
            .inner
            .clone()
            .get_relay_settings(GetRelaySettingsRequest {})
            .await
            .map_err(|e| format!("GetRelaySettings RPC failed: {e}"))?
            .into_inner())
    }

    pub async fn set_spora_relays(&self, use_spora: bool) -> Result<RelaySettings, String> {
        Ok(self
            .inner
            .clone()
            .set_spora_relays(SetSporaRelaysRequest { use_spora })
            .await
            .map_err(|e| format!("SetSporaRelays RPC failed: {e}"))?
            .into_inner())
    }

    /// Add (`original` empty) or change an own relay.
    pub async fn put_own_relay(
        &self,
        original: String,
        endpoint: String,
        token_action: TokenAction,
        token: String,
    ) -> Result<PutOwnRelayResponse, String> {
        Ok(self
            .inner
            .clone()
            .put_own_relay(PutOwnRelayRequest {
                original,
                endpoint,
                token_action: token_action as i32,
                token,
            })
            .await
            .map_err(|e| format!("PutOwnRelay RPC failed: {e}"))?
            .into_inner())
    }

    pub async fn remove_own_relay(&self, endpoint: String) -> Result<RelaySettings, String> {
        Ok(self
            .inner
            .clone()
            .remove_own_relay(RemoveOwnRelayRequest { endpoint })
            .await
            .map_err(|e| format!("RemoveOwnRelay RPC failed: {e}"))?
            .into_inner())
    }

    pub async fn refresh_relays(&self) -> Result<RelaySettings, String> {
        Ok(self
            .inner
            .clone()
            .refresh_relays(RefreshRelaysRequest {})
            .await
            .map_err(|e| format!("RefreshRelays RPC failed: {e}"))?
            .into_inner())
    }

    pub async fn telemetry(&self) -> Result<TelemetryStatus, String> {
        Ok(self
            .inner
            .clone()
            .get_telemetry(GetTelemetryRequest {})
            .await
            .map_err(|e| format!("GetTelemetry RPC failed: {e}"))?
            .into_inner())
    }

    pub async fn set_telemetry(&self, enabled: bool) -> Result<SetTelemetryResponse, String> {
        Ok(self
            .inner
            .clone()
            .set_telemetry(SetTelemetryRequest { enabled })
            .await
            .map_err(|e| format!("SetTelemetry RPC failed: {e}"))?
            .into_inner())
    }

    pub async fn acknowledge_telemetry_notice(&self) -> Result<TelemetryStatus, String> {
        Ok(self
            .inner
            .clone()
            .acknowledge_telemetry_notice(AcknowledgeTelemetryNoticeRequest {})
            .await
            .map_err(|e| format!("AcknowledgeTelemetryNotice RPC failed: {e}"))?
            .into_inner())
    }

    /// The bug-report zip, whole.
    pub async fn export_bug_report(&self, note: String) -> Result<Vec<u8>, String> {
        let mut stream = self
            .inner
            .clone()
            .export_bug_report(ExportBugReportRequest { note })
            .await
            .map_err(|e| e.message().to_string())?
            .into_inner();
        let mut zip = Vec::new();
        while let Some(chunk) = stream.message().await.map_err(|e| e.message().to_string())? {
            zip.extend_from_slice(&chunk.data);
        }
        if zip.is_empty() {
            return Err("the report came back empty".into());
        }
        Ok(zip)
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
