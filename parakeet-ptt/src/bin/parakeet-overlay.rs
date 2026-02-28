use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use parakeet_ptt::overlay_ipc::OverlayIpcMessage;
use parakeet_ptt::overlay_state::{ApplyOutcome, OverlayStateMachine, OverlayVisibility};

#[derive(Parser, Debug)]
#[command(
    name = "parakeet-overlay",
    version,
    about = "Parakeet overlay renderer process (Phase 4 MVP)"
)]
struct Cli {
    /// Rendering backend mode: auto, layer-shell, or fallback-window
    #[arg(long, value_enum, default_value_t = CliBackendMode::Auto)]
    backend: CliBackendMode,

    /// Auto-hide delay after session end.
    #[arg(long, default_value_t = 1200)]
    auto_hide_ms: u64,

    /// Overlay opacity (0.0-1.0).
    #[arg(long, default_value_t = 0.92)]
    opacity: f32,

    /// Font descriptor used for text rendering.
    #[arg(long, default_value = "Sans 16")]
    font: String,

    /// Screen anchor for overlay placement.
    #[arg(long, value_enum, default_value_t = CliAnchor::TopCenter)]
    anchor: CliAnchor,

    /// Horizontal margin from anchor reference point.
    #[arg(long, default_value_t = 24)]
    margin_x: u32,

    /// Vertical margin from anchor reference point.
    #[arg(long, default_value_t = 24)]
    margin_y: u32,

    /// Maximum text box width in pixels.
    #[arg(long, default_value_t = 960)]
    max_width: u32,

    /// Maximum rendered lines.
    #[arg(long, default_value_t = 4)]
    max_lines: u32,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CliBackendMode {
    Auto,
    LayerShell,
    FallbackWindow,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CliAnchor {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    LayerShell,
    FallbackWindow,
}

#[derive(Debug, Clone)]
struct OverlayUiConfig {
    opacity: f32,
    font: String,
    anchor: CliAnchor,
    margin_x: u32,
    margin_y: u32,
    max_width: u32,
    max_lines: u32,
}

trait OverlayBackend {
    fn render(&mut self, state: &OverlayVisibility);
}

#[derive(Debug)]
struct StubBackend {
    kind: BackendKind,
    ui: OverlayUiConfig,
}

impl OverlayBackend for StubBackend {
    fn render(&mut self, state: &OverlayVisibility) {
        debug!(
            backend = ?self.kind,
            opacity = self.ui.opacity,
            font = %self.ui.font,
            anchor = ?self.ui.anchor,
            margin_x = self.ui.margin_x,
            margin_y = self.ui.margin_y,
            max_width = self.ui.max_width,
            max_lines = self.ui.max_lines,
            ?state,
            "overlay state rendered"
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let ui = OverlayUiConfig {
        opacity: cli.opacity.clamp(0.0, 1.0),
        font: cli.font,
        anchor: cli.anchor,
        margin_x: cli.margin_x,
        margin_y: cli.margin_y,
        max_width: cli.max_width,
        max_lines: cli.max_lines,
    };
    let backend_kind = select_backend(cli.backend);
    let mut backend: Box<dyn OverlayBackend + Send> = Box::new(StubBackend {
        kind: backend_kind,
        ui,
    });

    info!(backend = ?backend_kind, "overlay process started");

    let mut machine = OverlayStateMachine::new(Duration::from_millis(cli.auto_hide_ms.max(1)));
    backend.render(machine.visibility());

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let started = Instant::now();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(raw)) => {
                        if raw.trim().is_empty() {
                            continue;
                        }
                        let now_ms = started.elapsed().as_millis() as u64;
                        match serde_json::from_str::<OverlayIpcMessage>(&raw) {
                            Ok(message) => {
                                match machine.apply_event(message, now_ms) {
                                    ApplyOutcome::Applied => backend.render(machine.visibility()),
                                    ApplyOutcome::DroppedStaleSeq => {
                                        debug!("overlay process dropped stale sequence event");
                                    }
                                    ApplyOutcome::DroppedSessionMismatch => {
                                        debug!("overlay process dropped session mismatch event");
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(error = %err, payload = %raw, "failed to decode overlay IPC event");
                            }
                        }
                    }
                    Ok(None) => {
                        info!("overlay stdin closed; shutting down");
                        break;
                    }
                    Err(err) => {
                        warn!(error = %err, "overlay stdin read error; shutting down");
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                let now_ms = started.elapsed().as_millis() as u64;
                if machine.advance_time(now_ms) {
                    backend.render(machine.visibility());
                }
            }
        }
    }

    Ok(())
}

fn select_backend(mode: CliBackendMode) -> BackendKind {
    match mode {
        CliBackendMode::Auto | CliBackendMode::LayerShell => BackendKind::LayerShell,
        CliBackendMode::FallbackWindow => BackendKind::FallbackWindow,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
