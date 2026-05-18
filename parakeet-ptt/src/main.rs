mod app;
mod audio_feedback;
mod client;
mod config;
mod hotkey;
mod injector;
mod injector_runtime;
mod llm;
mod overlay_process;
mod protocol;
mod routing;
mod state;
mod surface_focus;

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::audio_feedback::AudioFeedback;
use crate::config::{
    resolve_overlay_adaptive_width, resolve_overlay_capability, ClientConfig, ClipboardOptions,
    InjectionConfig, OverlayMode, DEFAULT_ENDPOINT,
};
use crate::injector::{InjectorContext, INJECTOR_CONTEXT_ENV};
use crate::surface_focus::WaylandFocusCache;
use parakeet_ptt::overlay_renderer::INTERNAL_OVERLAY_MODE_ARG;

const DEFAULT_LLM_PRE_MODIFIER_KEY: &str = "KEY_SHIFT";
const DEFAULT_LLM_BASE_URL: &str = "http://127.0.0.1:8080/v1";
const DEFAULT_LLM_MODEL: &str = "local";
const DEFAULT_LLM_SYSTEM_PROMPT: &str =
    "You are a concise assistant. Return only the final answer text for direct insertion.";

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

    /// Hold-to-talk key (evdev key name, e.g. KEY_RIGHTCTRL)
    #[arg(long, default_value = "KEY_RIGHTCTRL")]
    hotkey: String,

    /// Pre-modifier key held before hotkey down to start in LLM query mode.
    #[arg(long, default_value = DEFAULT_LLM_PRE_MODIFIER_KEY)]
    llm_pre_modifier_key: String,

    /// Key dwell time in milliseconds for direct uinput paste chords
    #[arg(long, default_value_t = 18)]
    uinput_dwell_ms: u64,

    /// Connection timeout in seconds
    #[arg(long, default_value_t = 5)]
    timeout_seconds: u64,

    /// Test injector only (injects a fixed string then exits)
    #[arg(long)]
    test_injection: bool,

    /// Number of test-injection attempts to emit before exiting.
    #[arg(long, default_value_t = 1, requires = "test_injection")]
    test_injection_count: u32,

    /// Prefix text used for test-injection payload(s).
    #[arg(long, default_value = "Parakeet Test", requires = "test_injection")]
    test_injection_text_prefix: String,

    /// Delay between repeated test-injection attempts.
    #[arg(long, default_value_t = 150, requires = "test_injection")]
    test_injection_interval_ms: u64,

    /// Optional forced route shortcut for test-injection runs.
    #[arg(long, value_enum, requires = "test_injection")]
    test_injection_shortcut: Option<CliTestInjectionShortcut>,

    /// Internal subprocess mode: read transcript text from stdin, inject once, then exit.
    #[arg(long, hide = true)]
    internal_inject_once: bool,

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
    #[arg(long, value_enum, default_value_t = CliPasteKeyBackend::Uinput)]
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

    /// Enable or disable overlay routing (CLI takes precedence over env).
    #[arg(long, action = clap::ArgAction::Set)]
    overlay_enabled: Option<bool>,

    /// Enable or disable adaptive overlay width (CLI takes precedence over env).
    #[arg(long, action = clap::ArgAction::Set)]
    overlay_adaptive_width: Option<bool>,

    /// Base URL for llama-server OpenAI-compatible API.
    #[arg(long, default_value = DEFAULT_LLM_BASE_URL)]
    llm_base_url: String,

    /// Model name passed to llama-server.
    #[arg(long, default_value = DEFAULT_LLM_MODEL)]
    llm_model: String,

    /// Timeout in seconds for llama responses.
    #[arg(long, default_value_t = 20)]
    llm_timeout_seconds: u64,

    /// Max tokens for llama responses.
    #[arg(long, default_value_t = 512)]
    llm_max_tokens: u32,

    /// Temperature for llama responses.
    #[arg(long, default_value_t = 0.7)]
    llm_temperature: f32,

    /// System prompt used for LLM query mode responses.
    #[arg(long, default_value = DEFAULT_LLM_SYSTEM_PROMPT)]
    llm_system_prompt: String,

    /// Stream llama deltas to overlay while generating.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    llm_overlay_stream: bool,
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
    Uinput,
}

impl From<CliPasteKeyBackend> for crate::config::PasteKeyBackend {
    fn from(backend: CliPasteKeyBackend) -> Self {
        match backend {
            CliPasteKeyBackend::Uinput => crate::config::PasteKeyBackend::Uinput,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliPasteBackendFailurePolicy {
    CopyOnly,
    Error,
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Eq)]
enum CliTestInjectionShortcut {
    CtrlV,
    CtrlShiftV,
}

impl From<CliTestInjectionShortcut> for crate::config::PasteShortcut {
    fn from(shortcut: CliTestInjectionShortcut) -> Self {
        match shortcut {
            CliTestInjectionShortcut::CtrlV => crate::config::PasteShortcut::CtrlV,
            CliTestInjectionShortcut::CtrlShiftV => crate::config::PasteShortcut::CtrlShiftV,
        }
    }
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

fn internal_overlay_args_from_env() -> Option<Vec<std::ffi::OsString>> {
    let raw_args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if raw_args
        .get(1)
        .is_some_and(|arg| arg == INTERNAL_OVERLAY_MODE_ARG)
    {
        let mut overlay_args = Vec::with_capacity(raw_args.len().saturating_sub(1));
        overlay_args.push(std::ffi::OsString::from("parakeet-overlay"));
        overlay_args.extend(raw_args.into_iter().skip(2));
        return Some(overlay_args);
    }
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(overlay_args) = internal_overlay_args_from_env() {
        return parakeet_ptt::overlay_renderer::run_from_args(overlay_args).await;
    }

    let cli = Cli::parse();
    init_tracing();

    let config = ClientConfig::new(
        &cli.endpoint,
        cli.shared_secret.clone(),
        cli.hotkey.clone(),
        InjectionConfig {
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

    if cli.internal_inject_once {
        return run_internal_inject_once(&config);
    }

    let llm_base_url = url::Url::parse(&cli.llm_base_url)
        .with_context(|| format!("invalid LLM base URL: {}", cli.llm_base_url))?;
    let llm_config = llm::LlmRuntimeConfig {
        base_url: llm_base_url,
        model: cli.llm_model.clone(),
        timeout: Duration::from_secs(cli.llm_timeout_seconds.max(1)),
        max_tokens: cli.llm_max_tokens.max(1),
        temperature: cli.llm_temperature.clamp(0.0, 2.0),
        system_prompt: cli.llm_system_prompt.clone(),
        overlay_stream: cli.llm_overlay_stream,
    };

    if cli.test_injection {
        let forced_shortcut = cli.test_injection_shortcut.clone().map(Into::into);
        let injector =
            injector_runtime::build_injector_with_shortcut_override(&config, None, forced_shortcut);
        let attempt_total = cli.test_injection_count.max(1);
        for attempt_index in 0..attempt_total {
            let payload = if attempt_total == 1 {
                cli.test_injection_text_prefix.clone()
            } else {
                format!(
                    "{} {:02}",
                    cli.test_injection_text_prefix,
                    attempt_index + 1
                )
            };
            injector.inject(&payload).with_context(|| {
                format!("injector test failed at attempt {}", attempt_index + 1)
            })?;
            info!(
                test_attempt_index = attempt_index + 1,
                test_attempt_total = attempt_total,
                forced_shortcut = ?forced_shortcut,
                payload_len = payload.len(),
                "injector test attempt completed"
            );
            if attempt_index + 1 < attempt_total && cli.test_injection_interval_ms > 0 {
                std::thread::sleep(Duration::from_millis(cli.test_injection_interval_ms));
            }
        }
        info!(
            test_attempt_total = attempt_total,
            forced_shortcut = ?forced_shortcut,
            "injector test run completed"
        );
        return Ok(());
    }

    if cli.demo {
        let audio_feedback = AudioFeedback::new(
            cli.completion_sound,
            cli.completion_sound_path.clone(),
            cli.completion_sound_volume,
        );
        app::run_demo(config, cli.demo_text, audio_feedback).await?;
        return Ok(());
    }

    let audio_feedback = AudioFeedback::new(
        cli.completion_sound,
        cli.completion_sound_path,
        cli.completion_sound_volume,
    );
    run_client_app(
        config,
        audio_feedback,
        cli.overlay_enabled,
        cli.overlay_adaptive_width,
        llm_config,
        cli.llm_pre_modifier_key.clone(),
    )
    .await
}

fn run_internal_inject_once(config: &ClientConfig) -> Result<()> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("failed to read injector subprocess stdin")?;
    let context = std::env::var(INJECTOR_CONTEXT_ENV)
        .ok()
        .and_then(|raw| match serde_json::from_str::<InjectorContext>(&raw) {
            Ok(context) => Some(context),
            Err(err) => {
                warn!(error = %err, "failed to parse injector subprocess context env; continuing without parent focus context");
                None
            }
        });
    injector_runtime::build_injector_with_shortcut_override(config, context, None)
        .inject(&text)
        .context("internal injection failed")
}

async fn run_client_app(
    config: ClientConfig,
    audio_feedback: AudioFeedback,
    overlay_enabled_override: Option<bool>,
    overlay_adaptive_width_override: Option<bool>,
    llm_config: llm::LlmRuntimeConfig,
    llm_pre_modifier_key_name: String,
) -> Result<()> {
    let overlay_capability = resolve_overlay_capability(overlay_enabled_override);
    let overlay_adaptive_width = resolve_overlay_adaptive_width(overlay_adaptive_width_override);
    match overlay_capability.mode {
        OverlayMode::Disabled => {
            warn!(
                overlay_mode = overlay_capability.mode.as_str(),
                overlay_reason = %overlay_capability.reason,
                overlay_adaptive_width,
                "overlay capability probe completed with disabled mode"
            );
        }
        OverlayMode::LayerShell | OverlayMode::FallbackWindow => {
            info!(
                overlay_mode = overlay_capability.mode.as_str(),
                overlay_reason = %overlay_capability.reason,
                overlay_adaptive_width,
                "overlay capability probe completed"
            );
        }
    }

    let focus_cache = Some(WaylandFocusCache::new());
    let overlay_sink = app::build_runtime_overlay_sink(
        overlay_capability.mode,
        overlay_adaptive_width,
        focus_cache.clone(),
    );
    let ports = app::ClientPorts::new(
        audio_feedback,
        Arc::new(app::WsDaemonConnector),
        injector_runtime::build_injection_runner(&config),
        overlay_sink,
        focus_cache,
        Box::new(app::EvdevHotkeySource::new(llm_pre_modifier_key_name)),
        llm::build_http_llm_answerer(llm_config),
    );
    app::run(config, ports).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
