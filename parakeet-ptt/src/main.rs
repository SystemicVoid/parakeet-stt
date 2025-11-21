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
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use crate::client::WsClient;
use crate::config::{ClientConfig, DEFAULT_ENDPOINT};
use crate::hotkey::{spawn_hotkey_loop, HotkeyEvent};
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

    /// Run a single start/stop/demo sequence instead of the hotkey loop
    #[arg(long)]
    demo: bool,

    /// Override text to inject during demo (otherwise uses daemon final result)
    #[arg(long)]
    demo_text: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    let config = ClientConfig::new(
        &cli.endpoint,
        cli.shared_secret.clone(),
        cli.hotkey.clone(),
        cli.wtype.clone(),
        cli.wtype_delay_ms,
        Duration::from_secs(cli.timeout_seconds.max(1)),
    )?;

    if cli.demo {
        run_demo(config, cli.demo_text).await?;
        return Ok(());
    }

    run_hotkey_mode(config).await
}

async fn run_demo(config: ClientConfig, override_text: Option<String>) -> Result<()> {
    info!(endpoint = %config.endpoint, "Connecting to parakeet-stt-daemon");
    let mut client = WsClient::connect(&config).await?;
    let injector: Box<dyn TextInjector> = if let Some(path) = config.wtype_path.clone() {
        Box::new(WtypeInjector::new(path, config.wtype_delay_ms))
    } else {
        Box::new(NoopInjector)
    };

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
    let injector: Box<dyn TextInjector> = match config.wtype_path.clone() {
        Some(path) => {
            if path.exists() {
                info!(?path, "Using wtype injector");
                Box::new(WtypeInjector::new(path, config.wtype_delay_ms))
            } else {
                warn!(
                    ?path,
                    "Configured wtype path does not exist; using noop injector"
                );
                Box::new(NoopInjector)
            }
        }
        None => match which::which("wtype") {
            Ok(path) => {
                info!(?path, "Found wtype in PATH");
                Box::new(WtypeInjector::new(path, config.wtype_delay_ms))
            }
            Err(_) => {
                warn!("No injector configured and wtype not found; transcription will not be injected");
                Box::new(NoopInjector)
            }
        },
    };

    let mut state = PttState::new();
    let (hk_tx, mut hk_rx) = mpsc::unbounded_channel();
    let _hotkey_handle = spawn_hotkey_loop(hk_tx)?;

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
        .and_then(|r| r.json::<StatusInfo>())
        .await
    {
        Ok(status) => {
            info!(
                "Daemon status: state={:?}, sessions_active={:?}, device={:?}, streaming={:?}, chunk_secs={:?}",
                status.state, status.sessions_active, status.device, status.streaming_enabled, status.chunk_secs
            );
        }
        Err(err) => {
            warn!("Failed to fetch daemon status from {}: {}", url, err);
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
