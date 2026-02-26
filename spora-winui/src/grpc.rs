use tonic::transport::Channel;

pub mod proto {
    tonic::include_proto!("spora");
}

use proto::spora_client::SporaClient;
use proto::*;

#[derive(Clone)]
pub struct GrpcClient {
    inner: SporaClient<Channel>,
}

impl GrpcClient {
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
            .share(ShareRequest { key })
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
}
