mod audio_feedback;
mod client;
mod config;
mod hotkey;
mod injector;
mod protocol;
mod routing;
mod state;
mod surface_focus;

use std::collections::BTreeSet;
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

use crate::audio_feedback::AudioFeedback;
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
    #[arg(long, value_enum, default_value_t = CliPasteStrategy::Single)]
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
    #[arg(long, value_enum, default_value_t = CliPasteKeyBackend::Auto)]
    paste_key_backend: CliPasteKeyBackend,

    /// Routing mode for choosing paste shortcut(s).
    #[arg(long, value_enum, default_value_t = CliPasteRoutingMode::Adaptive)]
    paste_routing_mode: CliPasteRoutingMode,

    /// Preferred shortcut when focused surface is classified as terminal-like.
    #[arg(long, value_enum, default_value_t = CliPasteShortcut::CtrlShiftV)]
    adaptive_terminal_shortcut: CliPasteShortcut,

    /// Preferred shortcut when focused surface is classified as editor/browser-like.
    #[arg(long, value_enum, default_value_t = CliPasteShortcut::CtrlV)]
    adaptive_general_shortcut: CliPasteShortcut,

    /// Preferred shortcut when focused surface cannot be classified.
    #[arg(long, value_enum, default_value_t = CliPasteShortcut::CtrlShiftV)]
    adaptive_unknown_shortcut: CliPasteShortcut,

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
enum CliPasteRoutingMode {
    Static,
    Adaptive,
}

impl From<CliPasteRoutingMode> for crate::config::PasteRoutingMode {
    fn from(mode: CliPasteRoutingMode) -> Self {
        match mode {
            CliPasteRoutingMode::Static => crate::config::PasteRoutingMode::Static,
            CliPasteRoutingMode::Adaptive => crate::config::PasteRoutingMode::Adaptive,
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

const DEPRECATED_COMPAT_FLAGS: &[&str] = &[
    "--paste-shortcut",
    "--paste-shortcut-fallback",
    "--paste-strategy",
    "--paste-chain-delay-ms",
    "--paste-restore-policy",
    "--paste-restore-delay-ms",
    "--paste-post-chord-hold-ms",
    "--paste-copy-foreground",
    "--paste-mime-type",
    "--paste-routing-mode",
    "--adaptive-terminal-shortcut",
    "--adaptive-general-shortcut",
    "--adaptive-unknown-shortcut",
];

fn collect_deprecated_cli_flags(args: &[String]) -> Vec<&'static str> {
    let mut used = BTreeSet::new();
    for arg in args {
        for flag in DEPRECATED_COMPAT_FLAGS {
            if arg == *flag || arg.starts_with(&format!("{flag}=")) {
                used.insert(*flag);
            }
        }
    }
    used.into_iter().collect()
}

fn warn_deprecated_cli_flags(flags: &[&'static str]) {
    if flags.is_empty() {
        return;
    }

    warn!(
        deprecated_flags = %flags.join(", "),
        "deprecated compatibility flags are ignored and will be removed in a future release"
    );
}

fn apply_robust_profile_over_deprecated_flags(cli: &mut Cli) {
    cli.paste_shortcut = CliPasteShortcut::CtrlShiftV;
    cli.paste_shortcut_fallback = CliPasteShortcutFallback::None;
    cli.paste_strategy = CliPasteStrategy::Single;
    cli.paste_chain_delay_ms = 45;
    cli.paste_restore_delay_ms = 250;
    cli.paste_post_chord_hold_ms = 700;
    cli.paste_restore_policy = CliPasteRestorePolicy::Never;
    cli.paste_copy_foreground = true;
    cli.paste_mime_type = "text/plain;charset=utf-8".to_string();
    cli.paste_routing_mode = CliPasteRoutingMode::Adaptive;
    cli.adaptive_terminal_shortcut = CliPasteShortcut::CtrlShiftV;
    cli.adaptive_general_shortcut = CliPasteShortcut::CtrlV;
    cli.adaptive_unknown_shortcut = CliPasteShortcut::CtrlShiftV;
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().collect();
    let deprecated_cli_flags = collect_deprecated_cli_flags(raw_args.get(1..).unwrap_or(&[]));
    let mut cli = Cli::parse_from(raw_args);
    init_tracing();
    warn_deprecated_cli_flags(&deprecated_cli_flags);
    apply_robust_profile_over_deprecated_flags(&mut cli);

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
                routing_mode: cli.paste_routing_mode.into(),
                adaptive_terminal_shortcut: cli.adaptive_terminal_shortcut.into(),
                adaptive_general_shortcut: cli.adaptive_general_shortcut.into(),
                adaptive_unknown_shortcut: cli.adaptive_unknown_shortcut.into(),
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

fn build_injector(config: &ClientConfig) -> Box<dyn TextInjector> {
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

    let wtype_binary = resolve_binary(config.wtype_path.as_ref(), "wtype");
    let ydotool_binary = resolve_binary(config.ydotool_path.as_ref(), "ydotool");

    let backend_failure_fallback = |reason: String| -> Box<dyn TextInjector> {
        match config.clipboard.backend_failure_policy {
            PasteBackendFailurePolicy::CopyOnly => {
                warn!(
                    reason = %reason,
                    "paste backend unavailable; falling back to copy-only injection"
                );
                Box::new(ClipboardInjector::new(
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
                Box::new(FailInjector::new(reason))
            }
        }
    };

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
                            return backend_failure_fallback(
                                "paste_key_backend=wtype but wtype was not found".to_string(),
                            );
                        };
                        PasteKeySender::Wtype(path)
                    }
                    PasteKeyBackend::Ydotool => {
                        let Some(path) = ydotool_binary.clone() else {
                            return backend_failure_fallback(
                                "paste_key_backend=ydotool but ydotool was not found".to_string(),
                            );
                        };
                        PasteKeySender::Ydotool(path)
                    }
                    PasteKeyBackend::Uinput => match UinputChordSender::new(config.uinput_dwell_ms)
                    {
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
                            if let Some(path) = wtype_binary.clone() {
                                senders.push(PasteKeySender::Wtype(path));
                            }
                            PasteKeySender::Chain(senders)
                        }
                        Err(err) => {
                            warn!(
                                error = %err,
                                dwell_ms = config.uinput_dwell_ms,
                                "paste_key_backend=auto could not initialize uinput; trying ydotool/wtype backends"
                            );
                            let mut senders = Vec::new();
                            if let Some(path) = ydotool_binary.clone() {
                                senders.push(PasteKeySender::Ydotool(path));
                            }
                            if let Some(path) = wtype_binary.clone() {
                                senders.push(PasteKeySender::Wtype(path));
                            }
                            if senders.is_empty() {
                                return backend_failure_fallback(
                                    "paste_key_backend=auto could not initialize uinput and could not find ydotool or wtype".to_string(),
                                );
                            }
                            if senders.len() == 1 {
                                senders.remove(0)
                            } else {
                                PasteKeySender::Chain(senders)
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
                paste_routing_mode = ?config.clipboard.routing_mode,
                adaptive_terminal_shortcut = ?config.clipboard.adaptive_terminal_shortcut,
                adaptive_general_shortcut = ?config.clipboard.adaptive_general_shortcut,
                adaptive_unknown_shortcut = ?config.clipboard.adaptive_unknown_shortcut,
                uinput_dwell_ms = config.uinput_dwell_ms,
                paste_seat = ?config.clipboard.seat,
                paste_write_primary = config.clipboard.write_primary,
                "Using clipboard injector"
            );
            if let Some(message) = shortcut_interop_warning(config) {
                warn!(
                    hint = message,
                    "paste shortcut may not work across all app types"
                );
            }

            Box::new(ClipboardInjector::new(
                sender,
                config.clipboard.clone(),
                matches!(config.injection_mode, InjectionMode::CopyOnly),
            ))
        }
    }
}

fn shortcut_interop_warning(config: &ClientConfig) -> Option<&'static str> {
    use crate::config::{InjectionMode, PasteRoutingMode, PasteShortcut, PasteStrategy};

    if !matches!(config.injection_mode, InjectionMode::Paste) {
        return None;
    }
    if matches!(config.clipboard.routing_mode, PasteRoutingMode::Adaptive) {
        return None;
    }
    if !matches!(config.clipboard.paste_strategy, PasteStrategy::Single) {
        return None;
    }
    if config.clipboard.shortcut_fallback.is_some() {
        return None;
    }

    match config.clipboard.paste_shortcut {
        PasteShortcut::CtrlV => None,
        PasteShortcut::CtrlShiftV => Some(
            "paste_shortcut=ctrl-shift-v with single strategy is app-specific; browsers/terminals often accept it, but editors like VS Code and COSMIC Text usually require ctrl-v",
        ),
        PasteShortcut::ShiftInsert => Some(
            "paste_shortcut=shift-insert with single strategy is app-specific; many browser/native editor fields require ctrl-v",
        ),
    }
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
                let to_inject = override_text.as_deref().unwrap_or(&text);
                info!(
                    session = %session_id,
                    latency_ms,
                    audio_ms,
                    "final result received"
                );
                audio_feedback.play_completion();
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
                                                    Ok(message) => handle_server_message(message, &mut state, injector.as_ref(), &audio_feedback)?,
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

#[cfg(test)]
mod tests {
    use clap::Parser;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::config::{
        ClientConfig, ClipboardOptions, InjectionConfig, InjectionMode, PasteBackendFailurePolicy,
        PasteKeyBackend, PasteRestorePolicy, PasteRoutingMode, PasteShortcut, PasteStrategy,
    };

    use super::{
        apply_robust_profile_over_deprecated_flags, build_injector, shortcut_interop_warning, Cli,
        CliPasteKeyBackend, CliPasteRoutingMode, CliPasteShortcut, CliPasteShortcutFallback,
        CliPasteStrategy,
    };

    fn clipboard_options(policy: PasteBackendFailurePolicy) -> ClipboardOptions {
        ClipboardOptions {
            paste_shortcut: PasteShortcut::CtrlV,
            shortcut_fallback: None,
            paste_strategy: PasteStrategy::Single,
            chain_delay_ms: 45,
            restore_policy: PasteRestorePolicy::Never,
            restore_delay_ms: 250,
            post_chord_hold_ms: 700,
            copy_foreground: true,
            mime_type: "text/plain;charset=utf-8".to_string(),
            key_backend: PasteKeyBackend::Wtype,
            backend_failure_policy: policy,
            routing_mode: PasteRoutingMode::Static,
            adaptive_terminal_shortcut: PasteShortcut::CtrlShiftV,
            adaptive_general_shortcut: PasteShortcut::CtrlV,
            adaptive_unknown_shortcut: PasteShortcut::CtrlShiftV,
            seat: None,
            write_primary: false,
        }
    }

    #[test]
    fn backend_failure_policy_error_returns_injector_error() {
        let config = ClientConfig::new(
            "ws://127.0.0.1:8765/ws",
            None,
            "KEY_RIGHTCTRL".to_string(),
            InjectionConfig {
                wtype_path: Some(PathBuf::from("/definitely/missing/wtype")),
                ydotool_path: None,
                wtype_delay_ms: 6,
                uinput_dwell_ms: 18,
                injection_mode: InjectionMode::Paste,
                clipboard: clipboard_options(PasteBackendFailurePolicy::Error),
            },
            Duration::from_secs(5),
        )
        .expect("config should parse");

        let injector = build_injector(&config);
        let err = injector
            .inject("test")
            .expect_err("policy=error should fail injection");
        let message = format!("{err:#}");
        assert!(message.contains("wtype"));
        assert!(message.contains("not found"));
    }

    #[test]
    fn cli_default_paste_strategy_is_single() {
        let cli = Cli::parse_from(["parakeet-ptt"]);
        assert!(matches!(cli.paste_strategy, CliPasteStrategy::Single));
    }

    #[test]
    fn cli_default_paste_key_backend_is_auto() {
        let cli = Cli::parse_from(["parakeet-ptt"]);
        assert!(matches!(cli.paste_key_backend, CliPasteKeyBackend::Auto));
    }

    #[test]
    fn cli_default_routing_profile_matches_robust_wayland_path() {
        let cli = Cli::parse_from(["parakeet-ptt"]);
        assert!(matches!(
            cli.paste_routing_mode,
            CliPasteRoutingMode::Adaptive
        ));
        assert!(matches!(
            cli.adaptive_terminal_shortcut,
            CliPasteShortcut::CtrlShiftV
        ));
        assert!(matches!(
            cli.adaptive_general_shortcut,
            CliPasteShortcut::CtrlV
        ));
        assert!(matches!(
            cli.adaptive_unknown_shortcut,
            CliPasteShortcut::CtrlShiftV
        ));
    }

    #[test]
    fn cli_accepts_explicit_always_chain_strategy() {
        let cli = Cli::parse_from(["parakeet-ptt", "--paste-strategy", "always-chain"]);
        assert!(matches!(cli.paste_strategy, CliPasteStrategy::AlwaysChain));
    }

    #[test]
    fn interop_warning_triggers_for_ctrl_shift_v_single_no_fallback() {
        let mut config = ClientConfig::new(
            "ws://127.0.0.1:8765/ws",
            None,
            "KEY_RIGHTCTRL".to_string(),
            InjectionConfig {
                wtype_path: None,
                ydotool_path: None,
                wtype_delay_ms: 6,
                uinput_dwell_ms: 18,
                injection_mode: InjectionMode::Paste,
                clipboard: clipboard_options(PasteBackendFailurePolicy::CopyOnly),
            },
            Duration::from_secs(5),
        )
        .expect("config should parse");
        config.clipboard.paste_shortcut = PasteShortcut::CtrlShiftV;
        config.clipboard.paste_strategy = PasteStrategy::Single;
        config.clipboard.shortcut_fallback = None;

        let warning =
            shortcut_interop_warning(&config).expect("expected interop warning for ctrl-shift-v");
        assert!(warning.contains("ctrl-shift-v"));
        assert!(warning.contains("VS Code"));
    }

    #[test]
    fn interop_warning_not_emitted_for_ctrl_v_single() {
        let mut config = ClientConfig::new(
            "ws://127.0.0.1:8765/ws",
            None,
            "KEY_RIGHTCTRL".to_string(),
            InjectionConfig {
                wtype_path: None,
                ydotool_path: None,
                wtype_delay_ms: 6,
                uinput_dwell_ms: 18,
                injection_mode: InjectionMode::Paste,
                clipboard: clipboard_options(PasteBackendFailurePolicy::CopyOnly),
            },
            Duration::from_secs(5),
        )
        .expect("config should parse");
        config.clipboard.paste_shortcut = PasteShortcut::CtrlV;
        config.clipboard.paste_strategy = PasteStrategy::Single;
        config.clipboard.shortcut_fallback = None;

        assert!(shortcut_interop_warning(&config).is_none());
    }

    #[test]
    fn interop_warning_not_emitted_in_adaptive_mode() {
        let mut config = ClientConfig::new(
            "ws://127.0.0.1:8765/ws",
            None,
            "KEY_RIGHTCTRL".to_string(),
            InjectionConfig {
                wtype_path: None,
                ydotool_path: None,
                wtype_delay_ms: 6,
                uinput_dwell_ms: 18,
                injection_mode: InjectionMode::Paste,
                clipboard: clipboard_options(PasteBackendFailurePolicy::CopyOnly),
            },
            Duration::from_secs(5),
        )
        .expect("config should parse");
        config.clipboard.routing_mode = PasteRoutingMode::Adaptive;
        config.clipboard.paste_shortcut = PasteShortcut::CtrlShiftV;
        config.clipboard.paste_strategy = PasteStrategy::Single;
        config.clipboard.shortcut_fallback = None;

        assert!(shortcut_interop_warning(&config).is_none());
    }

    #[test]
    fn collect_deprecated_cli_flags_detects_short_and_equals_forms() {
        let args = vec![
            "--paste-shortcut".to_string(),
            "ctrl-v".to_string(),
            "--paste-routing-mode=adaptive".to_string(),
            "--completion-sound".to_string(),
            "true".to_string(),
        ];
        let flags = super::collect_deprecated_cli_flags(&args);
        assert!(flags.contains(&"--paste-shortcut"));
        assert!(flags.contains(&"--paste-routing-mode"));
        assert!(!flags.contains(&"--completion-sound"));
    }

    #[test]
    fn robust_profile_ignores_deprecated_cli_overrides() {
        let mut cli = Cli::parse_from([
            "parakeet-ptt",
            "--paste-shortcut",
            "ctrl-v",
            "--paste-shortcut-fallback",
            "shift-insert",
            "--paste-strategy",
            "always-chain",
            "--paste-chain-delay-ms",
            "999",
            "--paste-restore-delay-ms",
            "999",
            "--paste-post-chord-hold-ms",
            "999",
            "--paste-restore-policy",
            "delayed",
            "--paste-copy-foreground",
            "false",
            "--paste-mime-type",
            "text/plain",
            "--paste-routing-mode",
            "static",
            "--adaptive-terminal-shortcut",
            "ctrl-v",
            "--adaptive-general-shortcut",
            "ctrl-shift-v",
            "--adaptive-unknown-shortcut",
            "ctrl-v",
        ]);

        apply_robust_profile_over_deprecated_flags(&mut cli);

        assert!(matches!(cli.paste_shortcut, CliPasteShortcut::CtrlShiftV));
        assert!(matches!(
            cli.paste_shortcut_fallback,
            CliPasteShortcutFallback::None
        ));
        assert!(matches!(cli.paste_strategy, CliPasteStrategy::Single));
        assert_eq!(cli.paste_chain_delay_ms, 45);
        assert_eq!(cli.paste_restore_delay_ms, 250);
        assert_eq!(cli.paste_post_chord_hold_ms, 700);
        assert!(matches!(
            cli.paste_routing_mode,
            CliPasteRoutingMode::Adaptive
        ));
    }
}
