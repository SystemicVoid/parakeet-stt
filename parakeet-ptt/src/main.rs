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

    /// Path to ydotool binary (used when paste key backend is ydotool/auto)
    #[arg(long)]
    ydotool: Option<PathBuf>,

    /// Delay between key events when using wtype
    #[arg(long, default_value_t = 6)]
    wtype_delay_ms: u64,

    /// Key dwell time in milliseconds for direct uinput paste chords
    #[arg(long, default_value_t = 18)]
    uinput_dwell_ms: u64,

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

    /// Optional fallback chord when the primary paste shortcut fails.
    #[arg(long, value_enum, default_value_t = CliPasteShortcutFallback::None)]
    paste_shortcut_fallback: CliPasteShortcutFallback,

    /// Paste shortcut strategy.
    #[arg(long, value_enum, default_value_t = CliPasteStrategy::AlwaysChain)]
    paste_strategy: CliPasteStrategy,

    /// Delay between chained paste shortcuts.
    #[arg(long, default_value_t = 45)]
    paste_chain_delay_ms: u64,

    /// Delay before restoring clipboard after paste key chord.
    #[arg(long, default_value_t = 250)]
    paste_restore_delay_ms: u64,

    /// Time to keep the foreground wl-copy source alive after paste chord(s).
    #[arg(long, default_value_t = 700)]
    paste_post_chord_hold_ms: u64,

    /// Clipboard restore policy in paste mode.
    /// Use `never` to maximize paste reliability.
    #[arg(long, value_enum, default_value_t = CliPasteRestorePolicy::Never)]
    paste_restore_policy: CliPasteRestorePolicy,

    /// Keep wl-copy in foreground during paste choreography for deterministic ownership.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    paste_copy_foreground: bool,

    /// MIME type passed to wl-copy in paste mode.
    #[arg(long, default_value = "text/plain;charset=utf-8")]
    paste_mime_type: String,

    /// Keyboard injection backend for paste shortcut(s).
    #[arg(long, value_enum, default_value_t = CliPasteKeyBackend::Wtype)]
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
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliInjectionMode {
    Type,
    Paste,
    CopyOnly,
}

impl From<CliInjectionMode> for crate::config::InjectionMode {
    fn from(mode: CliInjectionMode) -> Self {
        match mode {
            CliInjectionMode::Type => crate::config::InjectionMode::Type,
            CliInjectionMode::Paste => crate::config::InjectionMode::Paste,
            CliInjectionMode::CopyOnly => crate::config::InjectionMode::CopyOnly,
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
enum CliPasteShortcutFallback {
    None,
    CtrlV,
    CtrlShiftV,
    ShiftInsert,
}

impl From<CliPasteShortcutFallback> for Option<crate::config::PasteShortcut> {
    fn from(fallback: CliPasteShortcutFallback) -> Self {
        match fallback {
            CliPasteShortcutFallback::None => None,
            CliPasteShortcutFallback::CtrlV => Some(crate::config::PasteShortcut::CtrlV),
            CliPasteShortcutFallback::CtrlShiftV => Some(crate::config::PasteShortcut::CtrlShiftV),
            CliPasteShortcutFallback::ShiftInsert => {
                Some(crate::config::PasteShortcut::ShiftInsert)
            }
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliPasteStrategy {
    Single,
    OnError,
    AlwaysChain,
}

impl From<CliPasteStrategy> for crate::config::PasteStrategy {
    fn from(strategy: CliPasteStrategy) -> Self {
        match strategy {
            CliPasteStrategy::Single => crate::config::PasteStrategy::Single,
            CliPasteStrategy::OnError => crate::config::PasteStrategy::OnError,
            CliPasteStrategy::AlwaysChain => crate::config::PasteStrategy::AlwaysChain,
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

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliPasteKeyBackend {
    Wtype,
    Ydotool,
    Uinput,
    Auto,
}

impl From<CliPasteKeyBackend> for crate::config::PasteKeyBackend {
    fn from(backend: CliPasteKeyBackend) -> Self {
        match backend {
            CliPasteKeyBackend::Wtype => crate::config::PasteKeyBackend::Wtype,
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
            wtype_path: cli.wtype.clone(),
            ydotool_path: cli.ydotool.clone(),
            wtype_delay_ms: cli.wtype_delay_ms,
            uinput_dwell_ms: cli.uinput_dwell_ms,
            injection_mode: cli.injection_mode.into(),
            clipboard: ClipboardOptions {
                paste_shortcut: cli.paste_shortcut.into(),
                shortcut_fallback: cli.paste_shortcut_fallback.into(),
                paste_strategy: cli.paste_strategy.into(),
                chain_delay_ms: cli.paste_chain_delay_ms,
                restore_policy: cli.paste_restore_policy.into(),
                restore_delay_ms: cli.paste_restore_delay_ms,
                post_chord_hold_ms: cli.paste_post_chord_hold_ms,
                copy_foreground: cli.paste_copy_foreground,
                mime_type: cli.paste_mime_type.clone(),
                key_backend: cli.paste_key_backend.into(),
                backend_failure_policy: cli.paste_backend_failure_policy.into(),
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
        run_demo(config, cli.demo_text).await?;
        return Ok(());
    }

    run_hotkey_mode(config).await
}

fn build_injector(config: &ClientConfig) -> Box<dyn TextInjector> {
    use crate::config::{InjectionMode, PasteKeyBackend};
    use crate::injector::{ClipboardInjector, PasteKeySender, UinputChordSender};

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

    let wtype_binary = resolve_binary(config.wtype_path.as_ref(), "wtype");
    let ydotool_binary = resolve_binary(config.ydotool_path.as_ref(), "ydotool");

    match config.injection_mode {
        InjectionMode::Type => {
            let Some(wtype_binary) = wtype_binary else {
                error!(
                    "wtype is required for type injection but was not found; falling back to noop"
                );
                return Box::new(NoopInjector);
            };

            info!(
                ?wtype_binary,
                delay_ms = config.wtype_delay_ms,
                "Using wtype injector (type mode)"
            );
            Box::new(WtypeInjector::new(wtype_binary, config.wtype_delay_ms))
        }
        InjectionMode::Paste | InjectionMode::CopyOnly => {
            let sender = if matches!(config.injection_mode, InjectionMode::CopyOnly) {
                PasteKeySender::Disabled
            } else {
                match config.clipboard.key_backend {
                    PasteKeyBackend::Wtype => {
                        let Some(path) = wtype_binary.clone() else {
                            error!("paste_key_backend=wtype but wtype was not found; falling back to noop");
                            return Box::new(NoopInjector);
                        };
                        PasteKeySender::Wtype(path)
                    }
                    PasteKeyBackend::Ydotool => {
                        let Some(path) = ydotool_binary.clone() else {
                            error!("paste_key_backend=ydotool but ydotool was not found; falling back to noop");
                            return Box::new(NoopInjector);
                        };
                        PasteKeySender::Ydotool(path)
                    }
                    PasteKeyBackend::Uinput => match UinputChordSender::new(config.uinput_dwell_ms)
                    {
                        Ok(sender) => PasteKeySender::Uinput(std::sync::Arc::new(sender)),
                        Err(err) => {
                            error!(
                                error = %err,
                                dwell_ms = config.uinput_dwell_ms,
                                "paste_key_backend=uinput could not initialize /dev/uinput; falling back to noop"
                            );
                            return Box::new(NoopInjector);
                        }
                    },
                    PasteKeyBackend::Auto => match UinputChordSender::new(config.uinput_dwell_ms) {
                        Ok(sender) => PasteKeySender::Uinput(std::sync::Arc::new(sender)),
                        Err(err) => {
                            warn!(
                                error = %err,
                                dwell_ms = config.uinput_dwell_ms,
                                "paste_key_backend=auto could not initialize uinput; trying ydotool/wtype backends"
                            );
                            if let Some(path) = ydotool_binary.clone() {
                                PasteKeySender::Ydotool(path)
                            } else if let Some(path) = wtype_binary.clone() {
                                PasteKeySender::Wtype(path)
                            } else {
                                error!("paste_key_backend=auto could not initialize uinput and could not find ydotool or wtype; falling back to noop");
                                return Box::new(NoopInjector);
                            }
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
                paste_shortcut = ?config.clipboard.paste_shortcut,
                paste_shortcut_fallback = ?config.clipboard.shortcut_fallback,
                paste_strategy = ?config.clipboard.paste_strategy,
                chain_delay_ms = config.clipboard.chain_delay_ms,
                restore_policy = ?config.clipboard.restore_policy,
                restore_delay_ms = config.clipboard.restore_delay_ms,
                post_chord_hold_ms = config.clipboard.post_chord_hold_ms,
                copy_foreground = config.clipboard.copy_foreground,
                paste_mime_type = %config.clipboard.mime_type,
                paste_key_backend = ?config.clipboard.key_backend,
                paste_backend_failure_policy = ?config.clipboard.backend_failure_policy,
                uinput_dwell_ms = config.uinput_dwell_ms,
                paste_seat = ?config.clipboard.seat,
                paste_write_primary = config.clipboard.write_primary,
                "Using clipboard injector"
            );

            Box::new(ClipboardInjector::new(
                sender,
                config.clipboard.clone(),
                matches!(config.injection_mode, InjectionMode::CopyOnly),
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
