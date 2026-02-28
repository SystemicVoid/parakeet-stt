use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use http::request::Request;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::HeaderValue;
use url::Url;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

pub const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:8765/ws";

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum OverlayMode {
    LayerShell,
    FallbackWindow,
    Disabled,
}

impl OverlayMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LayerShell => "layer_shell",
            Self::FallbackWindow => "fallback_window",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayCapability {
    pub mode: OverlayMode,
    pub reason: String,
}

impl OverlayCapability {
    fn layer_shell() -> Self {
        Self {
            mode: OverlayMode::LayerShell,
            reason: "zwlr_layer_shell_v1_available".to_string(),
        }
    }

    fn fallback_window(reason: &str) -> Self {
        Self {
            mode: OverlayMode::FallbackWindow,
            reason: reason.to_string(),
        }
    }

    fn disabled(reason: impl Into<String>) -> Self {
        Self {
            mode: OverlayMode::Disabled,
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct OverlayProbeSignals {
    has_layer_shell: bool,
    has_wl_compositor: bool,
    has_xdg_wm_base: bool,
}

pub fn probe_overlay_capability() -> OverlayCapability {
    if let Ok(raw_override) = std::env::var("PARAKEET_OVERLAY_MODE") {
        if let Some(capability) = parse_overlay_mode_override(raw_override.trim()) {
            return capability;
        }
        return OverlayCapability::disabled(format!(
            "overlay_mode_override_invalid:{raw_override}"
        ));
    }

    match probe_wayland_overlay_signals() {
        Ok(signals) => classify_overlay_capability(signals),
        Err(err) => OverlayCapability::disabled(format!("wayland_probe_failed:{err}")),
    }
}

fn parse_overlay_mode_override(raw: &str) -> Option<OverlayCapability> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "auto" => None,
        "layer-shell" | "layer_shell" => Some(OverlayCapability {
            mode: OverlayMode::LayerShell,
            reason: "overlay_mode_override:layer_shell".to_string(),
        }),
        "fallback-window" | "fallback_window" => Some(OverlayCapability {
            mode: OverlayMode::FallbackWindow,
            reason: "overlay_mode_override:fallback_window".to_string(),
        }),
        "disabled" | "off" | "none" => Some(OverlayCapability {
            mode: OverlayMode::Disabled,
            reason: "overlay_mode_override:disabled".to_string(),
        }),
        _ => None,
    }
}

fn classify_overlay_capability(signals: OverlayProbeSignals) -> OverlayCapability {
    if signals.has_layer_shell {
        return OverlayCapability::layer_shell();
    }

    if signals.has_wl_compositor && signals.has_xdg_wm_base {
        return OverlayCapability::fallback_window(
            "zwlr_layer_shell_v1_unavailable_using_xdg_toplevel_fallback",
        );
    }

    let mut missing = Vec::new();
    if !signals.has_wl_compositor {
        missing.push("wl_compositor");
    }
    if !signals.has_xdg_wm_base {
        missing.push("xdg_wm_base");
    }

    OverlayCapability::disabled(format!("missing_window_protocols:{}", missing.join(",")))
}

fn probe_wayland_overlay_signals() -> Result<OverlayProbeSignals> {
    let connection = Connection::connect_to_env().context("connect_to_env")?;
    let display = connection.display();
    let mut event_queue = connection.new_event_queue();
    let queue_handle = event_queue.handle();
    let _registry = display.get_registry(&queue_handle, ());
    let mut state = OverlayProbeSignals::default();

    event_queue
        .roundtrip(&mut state)
        .context("registry_roundtrip_initial")?;
    event_queue
        .roundtrip(&mut state)
        .context("registry_roundtrip_secondary")?;

    Ok(state)
}

impl Dispatch<wl_registry::WlRegistry, ()> for OverlayProbeSignals {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, .. } = event {
            match interface.as_str() {
                "zwlr_layer_shell_v1" => state.has_layer_shell = true,
                "wl_compositor" => state.has_wl_compositor = true,
                "xdg_wm_base" => state.has_xdg_wm_base = true,
                _ => {}
            }
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::{
        classify_overlay_capability, parse_overlay_mode_override, OverlayMode, OverlayProbeSignals,
    };

    #[test]
    fn classify_overlay_prefers_layer_shell_when_available() {
        let capability = classify_overlay_capability(OverlayProbeSignals {
            has_layer_shell: true,
            has_wl_compositor: true,
            has_xdg_wm_base: true,
        });

        assert_eq!(capability.mode, OverlayMode::LayerShell);
        assert_eq!(capability.reason, "zwlr_layer_shell_v1_available");
    }

    #[test]
    fn classify_overlay_uses_fallback_when_layer_shell_missing() {
        let capability = classify_overlay_capability(OverlayProbeSignals {
            has_layer_shell: false,
            has_wl_compositor: true,
            has_xdg_wm_base: true,
        });

        assert_eq!(capability.mode, OverlayMode::FallbackWindow);
        assert_eq!(
            capability.reason,
            "zwlr_layer_shell_v1_unavailable_using_xdg_toplevel_fallback"
        );
    }

    #[test]
    fn classify_overlay_disables_when_required_protocols_missing() {
        let capability = classify_overlay_capability(OverlayProbeSignals {
            has_layer_shell: false,
            has_wl_compositor: true,
            has_xdg_wm_base: false,
        });

        assert_eq!(capability.mode, OverlayMode::Disabled);
        assert_eq!(capability.reason, "missing_window_protocols:xdg_wm_base");
    }

    #[test]
    fn parse_overlay_mode_override_forces_fallback_window() {
        let capability = parse_overlay_mode_override("fallback-window")
            .expect("fallback-window override should parse");

        assert_eq!(capability.mode, OverlayMode::FallbackWindow);
        assert_eq!(capability.reason, "overlay_mode_override:fallback_window");
    }

    #[test]
    fn parse_overlay_mode_override_rejects_invalid_values() {
        assert!(parse_overlay_mode_override("bad-value").is_none());
    }
}
