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
    Type,
    Paste,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum PasteShortcut {
    CtrlV,
    CtrlShiftV,
    ShiftInsert,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum PasteRestorePolicy {
    Never,
    Delayed,
}

#[derive(Clone, Debug)]
pub struct ClipboardOptions {
    pub paste_shortcut: PasteShortcut,
    pub shortcut_fallback: Option<PasteShortcut>,
    pub restore_policy: PasteRestorePolicy,
    pub restore_delay_ms: u64,
    pub copy_foreground: bool,
    pub mime_type: String,
}

#[derive(Clone, Debug)]
pub struct InjectionConfig {
    pub wtype_path: Option<PathBuf>,
    pub wtype_delay_ms: u64,
    pub injection_mode: InjectionMode,
    pub clipboard: ClipboardOptions,
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub endpoint: Url,
    pub shared_secret: Option<String>,
    pub hotkey: String,
    pub wtype_path: Option<PathBuf>,
    pub wtype_delay_ms: u64,
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
            wtype_path: injection.wtype_path,
            wtype_delay_ms: injection.wtype_delay_ms,
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
