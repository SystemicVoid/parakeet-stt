mod audio_feedback;
mod client;
mod config;
mod hotkey;
mod injector;
mod protocol;
mod routing;
mod state;
mod surface_focus;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::audio_feedback::AudioFeedback;
use crate::client::WsClient;
use crate::config::{ClientConfig, ClipboardOptions, InjectionConfig, DEFAULT_ENDPOINT};
use crate::hotkey::{ensure_input_access, spawn_hotkey_loop, HotkeyEvent};
use crate::injector::TextInjector;
use crate::protocol::{start_message, stop_message, ServerMessage};
use crate::state::PttState;

#[derive(Parser, Debug)]
#[command(
    name = "parakeet-ptt",
    version,
    about = "Push-to-talk client for the Parakeet daemon"
)]
struct Cli {
    /// WebSocket endpoint exposed by parakeet-stt-daemon
    #[arg(long, default_value = DEFAULT_ENDPOINT)]
    endpoint: String,

    /// Optional shared secret to send as x-parakeet-secret
    #[arg(long)]
    shared_secret: Option<String>,

    /// Hotkey to bind (currently unused placeholder)
    #[arg(long, default_value = "KEY_RIGHTCTRL")]
    hotkey: String,

    /// Path to ydotool binary (used when paste key backend is ydotool/auto)
    #[arg(long)]
    ydotool: Option<PathBuf>,

    /// Key dwell time in milliseconds for direct uinput paste chords
    #[arg(long, default_value_t = 18)]
    uinput_dwell_ms: u64,

    /// Connection timeout in seconds
    #[arg(long, default_value_t = 5)]
    timeout_seconds: u64,

    /// Test injector only (injects a fixed string then exits)
    #[arg(long)]
    test_injection: bool,

    /// Run a single start/stop/demo sequence instead of the hotkey loop
    #[arg(long)]
    demo: bool,

    /// Override text to inject during demo (otherwise uses daemon final result)
    #[arg(long)]
    demo_text: Option<String>,

    /// Injection mode: 'paste' (default) or 'copy-only'
    #[arg(long, value_enum, default_value_t = CliInjectionMode::Paste)]
    injection_mode: CliInjectionMode,

    /// Keyboard injection backend for paste shortcut(s).
    #[arg(long, value_enum, default_value_t = CliPasteKeyBackend::Auto)]
    paste_key_backend: CliPasteKeyBackend,

    /// Behavior when selected paste backend cannot be initialized or used.
    #[arg(
        long,
        value_enum,
        default_value_t = CliPasteBackendFailurePolicy::CopyOnly
    )]
    paste_backend_failure_policy: CliPasteBackendFailurePolicy,

    /// Optional Wayland seat for wl-copy/wl-paste operations.
    #[arg(long)]
    paste_seat: Option<String>,

    /// Mirror transcript into PRIMARY selection in addition to clipboard.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false)]
    paste_write_primary: bool,

    /// Enable or disable completion sound feedback.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    completion_sound: bool,

    /// Path to a custom completion sound file (WAV, OGG, etc.).
    #[arg(long)]
    completion_sound_path: Option<PathBuf>,

    /// Volume for completion sound (0-100).
    #[arg(long, default_value_t = 100)]
    completion_sound_volume: u8,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliInjectionMode {
    Paste,
    CopyOnly,
}

impl From<CliInjectionMode> for crate::config::InjectionMode {
    fn from(mode: CliInjectionMode) -> Self {
        match mode {
            CliInjectionMode::Paste => crate::config::InjectionMode::Paste,
            CliInjectionMode::CopyOnly => crate::config::InjectionMode::CopyOnly,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliPasteKeyBackend {
    Ydotool,
    Uinput,
    Auto,
}

impl From<CliPasteKeyBackend> for crate::config::PasteKeyBackend {
    fn from(backend: CliPasteKeyBackend) -> Self {
        match backend {
            CliPasteKeyBackend::Ydotool => crate::config::PasteKeyBackend::Ydotool,
            CliPasteKeyBackend::Uinput => crate::config::PasteKeyBackend::Uinput,
            CliPasteKeyBackend::Auto => crate::config::PasteKeyBackend::Auto,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliPasteBackendFailurePolicy {
    CopyOnly,
    Error,
}

impl From<CliPasteBackendFailurePolicy> for crate::config::PasteBackendFailurePolicy {
    fn from(policy: CliPasteBackendFailurePolicy) -> Self {
        match policy {
            CliPasteBackendFailurePolicy::CopyOnly => {
                crate::config::PasteBackendFailurePolicy::CopyOnly
            }
            CliPasteBackendFailurePolicy::Error => crate::config::PasteBackendFailurePolicy::Error,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    let config = ClientConfig::new(
        &cli.endpoint,
        cli.shared_secret.clone(),
        cli.hotkey.clone(),
        InjectionConfig {
            ydotool_path: cli.ydotool.clone(),
            uinput_dwell_ms: cli.uinput_dwell_ms,
            injection_mode: cli.injection_mode.into(),
            clipboard: ClipboardOptions {
                key_backend: cli.paste_key_backend.into(),
                backend_failure_policy: cli.paste_backend_failure_policy.into(),
                post_chord_hold_ms: 700,
                seat: cli.paste_seat.clone(),
                write_primary: cli.paste_write_primary,
            },
        },
        Duration::from_secs(cli.timeout_seconds.max(1)),
    )?;

    if cli.test_injection {
        let injector = build_injector(&config);
        injector
            .inject("Parakeet Test")
            .context("injector test failed")?;
        info!("Injector test sent 'Parakeet Test'");
        return Ok(());
    }

    if cli.demo {
        let audio_feedback = AudioFeedback::new(
            cli.completion_sound,
            cli.completion_sound_path.clone(),
            cli.completion_sound_volume,
        );
        run_demo(config, cli.demo_text, audio_feedback).await?;
        return Ok(());
    }

    let audio_feedback = AudioFeedback::new(
        cli.completion_sound,
        cli.completion_sound_path,
        cli.completion_sound_volume,
    );
    run_hotkey_mode(config, audio_feedback).await
}

fn build_injector(config: &ClientConfig) -> Arc<dyn TextInjector> {
    use crate::config::{InjectionMode, PasteBackendFailurePolicy, PasteKeyBackend};
    use crate::injector::{ClipboardInjector, FailInjector, PasteKeySender, UinputChordSender};

    let resolve_binary = |configured: Option<&PathBuf>, binary: &str| -> Option<PathBuf> {
        if let Some(path) = configured {
            if path.exists() {
                return Some(path.clone());
            }
            error!(?path, binary, "Configured binary path does not exist");
            return None;
        }
        which::which(binary).ok()
    };

    let ydotool_binary = resolve_binary(config.ydotool_path.as_ref(), "ydotool");

    let backend_failure_fallback = |reason: String| -> Arc<dyn TextInjector> {
        match config.clipboard.backend_failure_policy {
            PasteBackendFailurePolicy::CopyOnly => {
                warn!(
                    reason = %reason,
                    "paste backend unavailable; falling back to copy-only injection"
                );
                Arc::new(ClipboardInjector::new(
                    PasteKeySender::Disabled,
                    config.clipboard.clone(),
                    true,
                ))
            }
            PasteBackendFailurePolicy::Error => {
                error!(
                    reason = %reason,
                    "paste backend unavailable and policy=error; returning explicit injector error"
                );
                Arc::new(FailInjector::new(reason))
            }
        }
    };

    let sender = if matches!(config.injection_mode, InjectionMode::CopyOnly) {
        PasteKeySender::Disabled
    } else {
        match config.clipboard.key_backend {
            PasteKeyBackend::Ydotool => {
                let Some(path) = ydotool_binary.clone() else {
                    return backend_failure_fallback(
                        "paste_key_backend=ydotool but ydotool was not found".to_string(),
                    );
                };
                PasteKeySender::Ydotool(path)
            }
            PasteKeyBackend::Uinput => match UinputChordSender::new(config.uinput_dwell_ms) {
                Ok(sender) => PasteKeySender::Uinput(std::sync::Arc::new(sender)),
                Err(err) => {
                    return backend_failure_fallback(format!(
                        "paste_key_backend=uinput could not initialize /dev/uinput: {}",
                        err
                    ));
                }
            },
            PasteKeyBackend::Auto => match UinputChordSender::new(config.uinput_dwell_ms) {
                Ok(sender) => {
                    let mut senders = Vec::new();
                    senders.push(PasteKeySender::Uinput(std::sync::Arc::new(sender)));
                    if let Some(path) = ydotool_binary.clone() {
                        senders.push(PasteKeySender::Ydotool(path));
                    }
                    PasteKeySender::Chain(senders)
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        dwell_ms = config.uinput_dwell_ms,
                        "paste_key_backend=auto could not initialize uinput; trying ydotool backend"
                    );
                    let Some(path) = ydotool_binary.clone() else {
                        return backend_failure_fallback(
                            "paste_key_backend=auto could not initialize uinput and could not find ydotool".to_string(),
                        );
                    };
                    PasteKeySender::Ydotool(path)
                }
            },
        }
    };

    info!(
        mode = if matches!(config.injection_mode, InjectionMode::CopyOnly) {
            "copy-only"
        } else {
            "paste"
        },
        paste_key_backend = ?config.clipboard.key_backend,
        paste_backend_failure_policy = ?config.clipboard.backend_failure_policy,
        post_chord_hold_ms = config.clipboard.post_chord_hold_ms,
        uinput_dwell_ms = config.uinput_dwell_ms,
        paste_seat = ?config.clipboard.seat,
        paste_write_primary = config.clipboard.write_primary,
        "Using clipboard injector"
    );

    Arc::new(ClipboardInjector::new(
        sender,
        config.clipboard.clone(),
        matches!(config.injection_mode, InjectionMode::CopyOnly),
    ))
}

async fn inject_text_async(injector: Arc<dyn TextInjector>, text: String) -> Result<()> {
    tokio::task::spawn_blocking(move || injector.inject(&text))
        .await
        .context("injector worker task failed")?
        .context("failed to inject text into focused surface")
}

async fn run_demo(
    config: ClientConfig,
    override_text: Option<String>,
    audio_feedback: AudioFeedback,
) -> Result<()> {
    info!(endpoint = %config.endpoint, "Connecting to parakeet-stt-daemon");
    let mut client = WsClient::connect(&config).await?;
    let injector = build_injector(&config);

    let mut state = PttState::new();
    let Some(session_id) = state.begin_listening() else {
        return Err(anyhow!("failed to start session state"));
    };

    client
        .send(&start_message(session_id, Some("auto".to_string())))
        .await?;
    info!(session = %session_id, "start_session sent");

    // For demo purposes we immediately stop after starting.
    client.send(&stop_message(session_id)).await?;
    state.stop_listening();

    while let Some(message) = client.next_message().await? {
        match message {
            ServerMessage::SessionStarted { session_id, .. } => {
                info!(session = %session_id, "session started ack");
            }
            ServerMessage::FinalResult {
                session_id,
                text,
                latency_ms,
                audio_ms,
                ..
            } => {
                let to_inject = override_text.as_deref().unwrap_or(&text).to_string();
                info!(
                    session = %session_id,
                    latency_ms,
                    audio_ms,
                    "final result received"
                );
                audio_feedback.play_completion();
                inject_text_async(Arc::clone(&injector), to_inject).await?;
                state.reset();
                break;
            }
            ServerMessage::Error {
                session_id,
                message,
                ..
            } => {
                warn!(session = ?session_id, "daemon error: {}", message);
                break;
            }
            other => {
                debug!(?other, "ignoring server message");
            }
        }
    }

    Ok(())
}

async fn run_hotkey_mode(config: ClientConfig, audio_feedback: AudioFeedback) -> Result<()> {
    info!(
        endpoint = %config.endpoint,
        hotkey = %config.hotkey,
        completion_sound = audio_feedback.is_enabled(),
        "Starting hotkey loop; press Right Ctrl to talk"
    );
    ensure_input_access()?;
    let injector = build_injector(&config);

    let mut state = PttState::new();
    let (hk_tx, mut hk_rx) = mpsc::unbounded_channel();
    let hotkey_tasks = spawn_hotkey_loop(hk_tx)?;
    info!(
        devices = hotkey_tasks.len(),
        "Hotkey listeners started for KEY_RIGHTCTRL"
    );

    fetch_status_once(&config).await;

    let mut backoff = TokioDuration::from_millis(500);
    loop {
        match WsClient::connect(&config).await {
            Ok(ws_client) => {
                info!("Connected to daemon");
                backoff = TokioDuration::from_millis(500);
                let (mut ws_write, mut ws_read) = ws_client.into_split();

                let run_loop = async {
                    loop {
                        tokio::select! {
                            Some(evt) = hk_rx.recv() => {
                                match evt {
                                    HotkeyEvent::Down => {
                                        if let Some(session_id) = state.begin_listening() {
                                            let message = start_message(session_id, Some("auto".to_string()));
                                            send_message(&mut ws_write, &message).await?;
                                            info!(session = %session_id, "start_session sent (hotkey down)");
                                        }
                                    }
                                    HotkeyEvent::Up => {
                                        if let Some(session_id) = state.stop_listening() {
                                            let message = stop_message(session_id);
                                            send_message(&mut ws_write, &message).await?;
                                            info!(session = %session_id, "stop_session sent (hotkey up)");
                                        }
                                    }
                                }
                            }
                            next = ws_read.next() => {
                                match next {
                                    Some(Ok(msg)) => {
                                        match msg {
                                            tokio_tungstenite::tungstenite::protocol::Message::Text(txt) => {
                                                match serde_json::from_str::<ServerMessage>(&txt) {
                                                    Ok(message) => handle_server_message(message, &mut state, Arc::clone(&injector), &audio_feedback).await?,
                                                    Err(err) => warn!("failed to decode server message: {}", err),
                                                }
                                            }
                                            tokio_tungstenite::tungstenite::protocol::Message::Ping(payload) => {
                                                ws_write.send(tokio_tungstenite::tungstenite::protocol::Message::Pong(payload)).await?;
                                            }
                                            tokio_tungstenite::tungstenite::protocol::Message::Close(_) => {
                                                warn!("daemon closed the connection");
                                                break;
                                            }
                                            _ => {}
                                        }
                                    }
                                    Some(Err(err)) => {
                                        warn!("websocket error: {}", err);
                                        break;
                                    }
                                    None => {
                                        warn!("websocket stream ended");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Result::<()>::Ok(())
                }.await;

                if let Err(err) = run_loop {
                    warn!("session loop ended with error: {err}");
                }
                state.reset();
                warn!("Reconnecting to daemon after drop");
            }
            Err(err) => {
                warn!(
                    "Connection to daemon failed: {} (retrying in {:.1?})",
                    err, backoff
                );
                sleep(backoff).await;
                backoff = (backoff * 2).min(TokioDuration::from_secs(10));
            }
        }
    }
}

async fn send_message(
    ws_write: &mut crate::client::WsWrite,
    message: &crate::protocol::ClientMessage,
) -> Result<()> {
    let payload = serde_json::to_string(message).context("failed to serialize message")?;
    ws_write
        .send(tokio_tungstenite::tungstenite::protocol::Message::Text(
            payload,
        ))
        .await
        .context("failed to send message")
}

async fn handle_server_message(
    message: ServerMessage,
    state: &mut PttState,
    injector: Arc<dyn TextInjector>,
    audio_feedback: &AudioFeedback,
) -> Result<()> {
    match message {
        ServerMessage::SessionStarted { session_id, .. } => {
            info!(session = %session_id, "session started ack");
        }
        ServerMessage::FinalResult {
            session_id,
            text,
            latency_ms,
            audio_ms,
            ..
        } => {
            info!(
                session = %session_id,
                latency_ms,
                audio_ms,
                "final result received"
            );
            audio_feedback.play_completion();
            inject_text_async(injector, text).await?;
            state.reset();
        }
        ServerMessage::Error {
            session_id,
            message,
            ..
        } => {
            warn!(session = ?session_id, "daemon error: {}", message);
            state.reset();
        }
        other => {
            debug!(?other, "ignoring server message");
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct StatusInfo {
    state: Option<String>,
    sessions_active: Option<u32>,
    device: Option<String>,
    streaming_enabled: Option<bool>,
    chunk_secs: Option<f64>,
}

async fn fetch_status_once(config: &ClientConfig) {
    let Some(url) = config.status_url() else {
        return;
    };
    let client = reqwest::Client::new();
    match client
        .get(url.clone())
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(response) => match response.json::<StatusInfo>().await {
            Ok(status) => {
                info!(
                    "Daemon status: state={:?}, sessions_active={:?}, device={:?}, streaming={:?}, chunk_secs={:?}",
                    status.state, status.sessions_active, status.device, status.streaming_enabled, status.chunk_secs
                );
            }
            Err(err) => {
                warn!("Failed to decode daemon status from {}: {}", url, err);
            }
        },
        Err(err) => {
            warn!("Failed to fetch daemon status from {}: {}", url, err);
        }
    };
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use clap::Parser;

    use crate::config::{
        ClientConfig, ClipboardOptions, InjectionConfig, InjectionMode, PasteBackendFailurePolicy,
        PasteKeyBackend,
    };

    use super::build_injector;

    #[test]
    fn backend_failure_policy_error_returns_injector_error() {
        let config = ClientConfig::new(
            "ws://127.0.0.1:8765/ws",
            None,
            "KEY_RIGHTCTRL".to_string(),
            InjectionConfig {
                ydotool_path: Some(PathBuf::from("/definitely/missing/ydotool")),
                uinput_dwell_ms: 18,
                injection_mode: InjectionMode::Paste,
                clipboard: ClipboardOptions {
                    key_backend: PasteKeyBackend::Ydotool,
                    backend_failure_policy: PasteBackendFailurePolicy::Error,
                    post_chord_hold_ms: 700,
                    seat: None,
                    write_primary: false,
                },
            },
            Duration::from_secs(5),
        )
        .expect("config should parse");

        let injector = build_injector(&config);
        let err = injector
            .inject("test")
            .expect_err("policy=error should fail injection");
        let message = format!("{err:#}");
        assert!(message.contains("ydotool"));
        assert!(message.contains("not found"));
    }

    #[test]
    fn cli_default_paste_key_backend_is_auto() {
        let cli = super::Cli::parse_from(["parakeet-ptt"]);
        assert!(matches!(
            cli.paste_key_backend,
            super::CliPasteKeyBackend::Auto
        ));
    }
}
