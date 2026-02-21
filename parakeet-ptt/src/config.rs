use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use http::request::Request;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::HeaderValue;
use url::Url;

pub const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:8765/ws";

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum InjectionMode {
    Paste,
    CopyOnly,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum PasteShortcut {
    CtrlV,
    CtrlShiftV,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum PasteKeyBackend {
    Ydotool,
    Uinput,
    Auto,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum PasteBackendFailurePolicy {
    CopyOnly,
    Error,
}

#[derive(Clone, Debug)]
pub struct ClipboardOptions {
    pub key_backend: PasteKeyBackend,
    pub backend_failure_policy: PasteBackendFailurePolicy,
    pub post_chord_hold_ms: u64,
    pub seat: Option<String>,
    pub write_primary: bool,
}

#[derive(Clone, Debug)]
pub struct InjectionConfig {
    pub ydotool_path: Option<PathBuf>,
    pub uinput_dwell_ms: u64,
    pub injection_mode: InjectionMode,
    pub clipboard: ClipboardOptions,
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub endpoint: Url,
    pub shared_secret: Option<String>,
    pub hotkey: String,
    pub ydotool_path: Option<PathBuf>,
    pub uinput_dwell_ms: u64,
    pub injection_mode: InjectionMode,
    pub clipboard: ClipboardOptions,
    pub connect_timeout: Duration,
}

impl ClientConfig {
    pub fn new(
        endpoint: &str,
        shared_secret: Option<String>,
        hotkey: String,
        injection: InjectionConfig,
        connect_timeout: Duration,
    ) -> Result<Self> {
        let endpoint = Url::parse(endpoint)
            .with_context(|| format!("invalid WebSocket endpoint: {endpoint}"))?;
        Ok(Self {
            endpoint,
            shared_secret,
            hotkey,
            ydotool_path: injection.ydotool_path,
            uinput_dwell_ms: injection.uinput_dwell_ms,
            injection_mode: injection.injection_mode,
            clipboard: injection.clipboard,
            connect_timeout,
        })
    }

    pub fn status_url(&self) -> Option<Url> {
        let mut url = self.endpoint.clone();
        match url.scheme() {
            "ws" => {
                let _ = url.set_scheme("http");
            }
            "wss" => {
                let _ = url.set_scheme("https");
            }
            "http" | "https" => {}
            _ => return None,
        }
        // Replace path with /status
        url.set_path("/status");
        Some(url)
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
