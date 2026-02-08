mod client;
mod config;
mod hotkey;
mod injector;
mod protocol;
mod state;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::client::WsClient;
use crate::config::{ClientConfig, ClipboardOptions, InjectionConfig, DEFAULT_ENDPOINT};
use crate::hotkey::{ensure_input_access, spawn_hotkey_loop, HotkeyEvent};
use crate::injector::{NoopInjector, TextInjector, WtypeInjector};
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

    /// Path to wtype binary for text injection
    #[arg(long)]
    wtype: Option<PathBuf>,

    /// Delay between key events when using wtype
    #[arg(long, default_value_t = 6)]
    wtype_delay_ms: u64,

    /// Connection timeout in seconds
    #[arg(long, default_value_t = 5)]
    timeout_seconds: u64,

    /// Test injector only (types a fixed string then exits)
    #[arg(long)]
    test_injection: bool,

    /// Run a single start/stop/demo sequence instead of the hotkey loop
    #[arg(long)]
    demo: bool,

    /// Override text to inject during demo (otherwise uses daemon final result)
    #[arg(long)]
    demo_text: Option<String>,

    /// Injection mode: 'type' (default) or 'paste'
    #[arg(long, value_enum, default_value_t = CliInjectionMode::Type)]
    injection_mode: CliInjectionMode,

    /// Paste key chord to use in paste mode.
    /// Use `ctrl-shift-v` for terminals like Ghostty.
    #[arg(long, value_enum, default_value_t = CliPasteShortcut::CtrlShiftV)]
    paste_shortcut: CliPasteShortcut,

    /// Delay before restoring clipboard after paste key chord.
    #[arg(long, default_value_t = 250)]
    paste_restore_delay_ms: u64,

    /// Clipboard restore policy in paste mode.
    /// Use `never` to maximize paste reliability.
    #[arg(long, value_enum, default_value_t = CliPasteRestorePolicy::Never)]
    paste_restore_policy: CliPasteRestorePolicy,

    /// Keep wl-copy in foreground during paste choreography for deterministic ownership.
    #[arg(long, default_value_t = true)]
    paste_copy_foreground: bool,

    /// MIME type passed to wl-copy in paste mode.
    #[arg(long, default_value = "text/plain;charset=utf-8")]
    paste_mime_type: String,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliInjectionMode {
    Type,
    Paste,
}

impl From<CliInjectionMode> for crate::config::InjectionMode {
    fn from(mode: CliInjectionMode) -> Self {
        match mode {
            CliInjectionMode::Type => crate::config::InjectionMode::Type,
            CliInjectionMode::Paste => crate::config::InjectionMode::Paste,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliPasteShortcut {
    CtrlV,
    CtrlShiftV,
    ShiftInsert,
}

impl From<CliPasteShortcut> for crate::config::PasteShortcut {
    fn from(shortcut: CliPasteShortcut) -> Self {
        match shortcut {
            CliPasteShortcut::CtrlV => crate::config::PasteShortcut::CtrlV,
            CliPasteShortcut::CtrlShiftV => crate::config::PasteShortcut::CtrlShiftV,
            CliPasteShortcut::ShiftInsert => crate::config::PasteShortcut::ShiftInsert,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliPasteRestorePolicy {
    Never,
    Delayed,
}

impl From<CliPasteRestorePolicy> for crate::config::PasteRestorePolicy {
    fn from(policy: CliPasteRestorePolicy) -> Self {
        match policy {
            CliPasteRestorePolicy::Never => crate::config::PasteRestorePolicy::Never,
            CliPasteRestorePolicy::Delayed => crate::config::PasteRestorePolicy::Delayed,
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
            wtype_path: cli.wtype.clone(),
            wtype_delay_ms: cli.wtype_delay_ms,
            injection_mode: cli.injection_mode.into(),
            clipboard: ClipboardOptions {
                paste_shortcut: cli.paste_shortcut.into(),
                restore_policy: cli.paste_restore_policy.into(),
                restore_delay_ms: cli.paste_restore_delay_ms,
                copy_foreground: cli.paste_copy_foreground,
                mime_type: cli.paste_mime_type.clone(),
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
        run_demo(config, cli.demo_text).await?;
        return Ok(());
    }

    run_hotkey_mode(config).await
}

fn build_injector(config: &ClientConfig) -> Box<dyn TextInjector> {
    use crate::config::InjectionMode;
    use crate::injector::ClipboardInjector;

    // Helper to find wtype
    let find_wtype = || -> Option<PathBuf> {
        if let Some(path) = &config.wtype_path {
            if path.exists() {
                return Some(path.clone());
            } else {
                error!(?path, "Configured wtype path does not exist");
            }
        }
        match which::which("wtype") {
            Ok(path) => Some(path),
            Err(_) => {
                error!("wtype not found");
                None
            }
        }
    };

    let wtype_binary = match find_wtype() {
        Some(path) => path,
        None => {
            error!("wtype is required for injection but was not found; falling back to noop");
            return Box::new(NoopInjector);
        }
    };

    match config.injection_mode {
        InjectionMode::Type => {
            info!(
                ?wtype_binary,
                delay_ms = config.wtype_delay_ms,
                "Using wtype injector (type mode)"
            );
            Box::new(WtypeInjector::new(wtype_binary, config.wtype_delay_ms))
        }
        InjectionMode::Paste => {
            info!(
                ?wtype_binary,
                paste_shortcut = ?config.clipboard.paste_shortcut,
                restore_policy = ?config.clipboard.restore_policy,
                restore_delay_ms = config.clipboard.restore_delay_ms,
                copy_foreground = config.clipboard.copy_foreground,
                paste_mime_type = %config.clipboard.mime_type,
                "Using clipboard injector (paste mode)"
            );
            Box::new(ClipboardInjector::new(
                wtype_binary,
                config.clipboard.clone(),
            ))
        }
    }
}

async fn run_demo(config: ClientConfig, override_text: Option<String>) -> Result<()> {
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
                let to_inject = override_text.as_deref().unwrap_or(&text);
                info!(
                    session = %session_id,
                    latency_ms,
                    audio_ms,
                    "final result received"
                );
                injector
                    .inject(to_inject)
                    .context("failed to inject text into focused surface")?;
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

async fn run_hotkey_mode(config: ClientConfig) -> Result<()> {
    info!(
        endpoint = %config.endpoint,
        hotkey = %config.hotkey,
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
                                                    Ok(message) => handle_server_message(message, &mut state, injector.as_ref())?,
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

fn handle_server_message(
    message: ServerMessage,
    state: &mut PttState,
    injector: &dyn TextInjector,
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
            injector
                .inject(&text)
                .context("failed to inject text into focused surface")?;
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
