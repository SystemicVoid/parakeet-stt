mod client;
mod config;
mod injector;
mod protocol;
mod state;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use crate::client::WsClient;
use crate::config::{ClientConfig, DEFAULT_ENDPOINT};
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

    info!(
        endpoint = %config.endpoint,
        hotkey = %config.hotkey,
        "Hotkey loop not implemented yet; run with --demo to exercise the protocol"
    );
    Ok(())
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
