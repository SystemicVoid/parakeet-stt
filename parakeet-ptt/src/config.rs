use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use http::request::Request;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::HeaderValue;
use url::Url;

pub const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:8765/ws";

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub endpoint: Url,
    pub shared_secret: Option<String>,
    pub hotkey: String,
    pub wtype_path: Option<PathBuf>,
    pub wtype_delay_ms: u64,
    pub connect_timeout: Duration,
}

impl ClientConfig {
    pub fn new(
        endpoint: &str,
        shared_secret: Option<String>,
        hotkey: String,
        wtype_path: Option<PathBuf>,
        wtype_delay_ms: u64,
        connect_timeout: Duration,
    ) -> Result<Self> {
        let endpoint = Url::parse(endpoint)
            .with_context(|| format!("invalid WebSocket endpoint: {endpoint}"))?;
        Ok(Self {
            endpoint,
            shared_secret,
            hotkey,
            wtype_path,
            wtype_delay_ms,
            connect_timeout,
        })
    }

    pub fn build_request(&self) -> Result<Request<()>> {
        let mut request: Request<()> = self
            .endpoint
            .as_str()
            .into_client_request()
            .context("failed to build websocket request")?;

        if let Some(secret) = &self.shared_secret {
            let value =
                HeaderValue::from_str(secret).context("failed to encode shared secret header")?;
            request.headers_mut().insert("x-parakeet-secret", value);
        }

        Ok(request)
    }
}
