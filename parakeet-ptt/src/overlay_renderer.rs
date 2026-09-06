//! The overlay renderer process: a Wayland layer-shell (or xdg fallback) surface
//! that paints the Galley sheet from the shared overlay state machine.
//!
//! The process reads overlay IPC as JSON lines on stdin, folds them through
//! `OverlayStateMachine`, and paints with the `galley` composition into a
//! premultiplied ARGB shm buffer on every state change and on a 33 ms tick
//! while anything animates. Frames go through two shm slots gated by
//! `wl_buffer.release`, and every present drains the socket without blocking
//! so releases, configures, and `Closed` reach their handlers.
//! `--preview-dir` writes scripted frames to disk instead of talking to a
//! compositor.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::env_parse::parse_bool_override;
use crate::overlay_ipc::OverlayIpcMessage;
use crate::overlay_state::{ApplyOutcome, OverlayStateMachine, OverlayVisibility};

mod coil;
mod fonts;
mod galley;
mod paint;
mod preview;
mod prose;

use fonts::FontSet;
use galley::{Galley, SheetSpec};
use paint::{Frame, Rect};

pub const INTERNAL_OVERLAY_MODE_ARG: &str = "__overlay-renderer";

const FALLBACK_WINDOW_TITLE: &str = "Parakeet Overlay";
const LAYER_NAMESPACE: &str = "parakeet-overlay";
const OVERLAY_ADAPTIVE_WIDTH_ENV: &str = "PARAKEET_OVERLAY_ADAPTIVE_WIDTH";
/// Frame period while anything on the sheet animates.
const TICK_MS: u64 = 33;

#[derive(Parser, Debug)]
#[command(
    name = "parakeet-overlay",
    version,
    about = "Parakeet overlay renderer process"
)]
struct Cli {
    /// Rendering backend mode: auto, layer-shell, or fallback-window
    #[arg(long, value_enum, default_value_t = CliBackendMode::Auto)]
    backend: CliBackendMode,

    /// Auto-hide delay after session end.
    #[arg(long, default_value_t = 600)]
    auto_hide_ms: u64,

    /// Overlay opacity (0.0-1.0).
    #[arg(long, default_value_t = 1.0)]
    opacity: f32,

    /// Screen anchor for overlay placement.
    #[arg(long, value_enum, default_value_t = CliAnchor::BottomCenter)]
    anchor: CliAnchor,

    /// Horizontal margin from anchor reference point.
    #[arg(long, default_value_t = 24)]
    margin_x: u32,

    /// Vertical margin from anchor reference point.
    #[arg(long, default_value_t = 32)]
    margin_y: u32,

    /// Sheet width in pixels (clamped to 480..=1600).
    #[arg(long, default_value_t = galley::DEFAULT_SHEET_WIDTH)]
    max_width: u32,

    /// Maximum rendered prose lines.
    #[arg(long, default_value_t = 4)]
    max_lines: u32,

    /// Preferred wl_output name for the layer surface target.
    #[arg(long)]
    output_name: Option<String>,

    /// Enable or disable adaptive overlay width.
    #[arg(long, action = clap::ArgAction::Set)]
    adaptive_width: Option<bool>,

    /// Write scripted preview frames (PPM) into this directory and exit.
    #[arg(long, hide = true)]
    preview_dir: Option<std::path::PathBuf>,
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

impl CliAnchor {
    fn is_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopCenter | Self::TopRight)
    }
}

fn resolve_adaptive_width_override(cli_override: Option<bool>) -> bool {
    if let Some(adaptive_width) = cli_override {
        return adaptive_width;
    }

    std::env::var(OVERLAY_ADAPTIVE_WIDTH_ENV)
        .ok()
        .as_deref()
        .and_then(parse_bool_override)
        .unwrap_or(true)
}

#[cfg(test)]
fn resolve_adaptive_width_with_env_input(
    cli_override: Option<bool>,
    env_override: Option<&str>,
) -> bool {
    if let Some(adaptive_width) = cli_override {
        return adaptive_width;
    }

    env_override.and_then(parse_bool_override).unwrap_or(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    LayerShell,
    FallbackWindow,
    Noop,
}

#[derive(Debug, Clone)]
struct OverlayUiConfig {
    opacity: f32,
    anchor: CliAnchor,
    margin_x: u32,
    margin_y: u32,
    max_width: u32,
    max_lines: u32,
    adaptive_width_enabled: bool,
}

impl OverlayUiConfig {
    /// The sheet width, clamped to the range the layout supports.
    fn content_width(&self) -> u32 {
        self.max_width
            .clamp(galley::MIN_SHEET_WIDTH, galley::MAX_SHEET_WIDTH)
    }

    fn max_lines(&self) -> u32 {
        self.max_lines.clamp(1, 10)
    }

    fn sheet_spec(&self) -> SheetSpec {
        SheetSpec {
            content_width: self.content_width(),
            max_lines: self.max_lines(),
            opacity: self.opacity.clamp(0.0, 1.0),
            adaptive_width: self.adaptive_width_enabled,
            anchor_top: self.anchor.is_top(),
        }
    }

    fn surface_dimensions(&self) -> SurfaceDimensions {
        let (width, height) = galley::buffer_size(self.content_width(), self.max_lines());
        SurfaceDimensions { width, height }
    }
}

#[derive(Debug, Clone, Copy)]
struct SurfaceDimensions {
    width: u32,
    height: u32,
}

trait OverlayBackend {
    fn render(&mut self, state: &OverlayVisibility, now_ms: u64) -> Result<()>;
    fn is_animating(&self, _now_ms: u64) -> bool {
        false
    }
    fn push_audio_level(&mut self, _level_db: f32, _now_ms: u64) {}
}

#[derive(Debug)]
struct NoopBackend {
    reason: String,
}

impl OverlayBackend for NoopBackend {
    fn render(&mut self, state: &OverlayVisibility, _now_ms: u64) -> Result<()> {
        debug!(reason = %self.reason, ?state, "overlay renderer running in noop mode");
        Ok(())
    }
}

struct WaylandOverlayBackend {
    kind: BackendKind,
    spec: SheetSpec,
    runtime: WaylandRuntime,
    galley: Galley,
}

impl WaylandOverlayBackend {
    fn new(
        kind: BackendKind,
        ui: &OverlayUiConfig,
        runtime: WaylandRuntime,
        fonts: FontSet,
    ) -> Self {
        let spec = ui.sheet_spec();
        Self {
            kind,
            spec,
            runtime,
            galley: Galley::new(fonts, &spec),
        }
    }
}

impl OverlayBackend for WaylandOverlayBackend {
    fn render(&mut self, state: &OverlayVisibility, now_ms: u64) -> Result<()> {
        let Self {
            kind,
            spec,
            runtime,
            galley,
        } = self;
        galley.observe(state, spec, now_ms);
        let title = galley.title();
        runtime
            .present(|frame| galley.paint(frame, spec, now_ms), title.as_deref())
            .with_context(|| format!("overlay renderer backend failed for {kind:?}"))
    }

    fn is_animating(&self, now_ms: u64) -> bool {
        self.galley.is_animating(now_ms) || self.runtime.frame_pending
    }

    fn push_audio_level(&mut self, level_db: f32, now_ms: u64) {
        self.galley.push_audio_level(level_db, now_ms);
    }
}

struct BuiltBackend {
    kind: BackendKind,
    reason: String,
    backend: Box<dyn OverlayBackend + Send>,
}

#[derive(Debug, Clone, Copy, Default)]
struct BackendSignals {
    has_layer_shell: bool,
    has_wl_compositor: bool,
    has_xdg_wm_base: bool,
    has_wl_shm: bool,
}

impl BackendSignals {
    fn supports_layer_shell(self) -> bool {
        self.has_layer_shell && self.has_wl_compositor && self.has_wl_shm
    }

    fn supports_fallback_window(self) -> bool {
        self.has_wl_compositor && self.has_xdg_wm_base && self.has_wl_shm
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BackendSelection {
    LayerShell,
    FallbackWindow,
    Noop { reason: String },
}

fn resolve_backend_selection(
    mode: CliBackendMode,
    probe: std::result::Result<BackendSignals, String>,
) -> BackendSelection {
    let signals = match probe {
        Ok(signals) => signals,
        Err(err) => {
            return BackendSelection::Noop {
                reason: format!("wayland_probe_failed:{err}"),
            };
        }
    };

    match mode {
        CliBackendMode::Auto => {
            if signals.supports_layer_shell() {
                BackendSelection::LayerShell
            } else if signals.supports_fallback_window() {
                BackendSelection::FallbackWindow
            } else {
                BackendSelection::Noop {
                    reason: "unsupported_wayland_backend:auto".to_string(),
                }
            }
        }
        CliBackendMode::LayerShell => {
            if signals.supports_layer_shell() {
                BackendSelection::LayerShell
            } else {
                BackendSelection::Noop {
                    reason: "unsupported_wayland_backend:layer_shell".to_string(),
                }
            }
        }
        CliBackendMode::FallbackWindow => {
            if signals.supports_fallback_window() {
                BackendSelection::FallbackWindow
            } else {
                BackendSelection::Noop {
                    reason: "unsupported_wayland_backend:fallback_window".to_string(),
                }
            }
        }
    }
}

/// Picks a backend and brings up its Wayland runtime. Init failures degrade in
/// the same order as before: layer-shell → fallback window (auto only) → noop.
fn build_backend(
    mode: CliBackendMode,
    ui: &OverlayUiConfig,
    output_name: Option<&str>,
    fonts: FontSet,
) -> BuiltBackend {
    let probe_result = probe_backend_signals().map_err(|err| err.to_string());
    let selection = resolve_backend_selection(mode, probe_result);

    let runtime: std::result::Result<(BackendKind, String, WaylandRuntime), String> =
        match selection {
            BackendSelection::LayerShell => {
                match WaylandRuntime::new(BackendKind::LayerShell, ui, output_name) {
                Ok(runtime) => Ok((BackendKind::LayerShell, "layer_shell".to_string(), runtime)),
                Err(layer_err) if matches!(mode, CliBackendMode::Auto) => {
                    match WaylandRuntime::new(BackendKind::FallbackWindow, ui, None) {
                        Ok(runtime) => Ok((
                            BackendKind::FallbackWindow,
                            format!("layer_shell_init_failed:{layer_err};using_fallback_window"),
                            runtime,
                        )),
                        Err(fallback_err) => Err(format!(
                            "layer_shell_init_failed:{layer_err};fallback_init_failed:{fallback_err}"
                        )),
                    }
                }
                Err(layer_err) => Err(format!("layer_shell_init_failed:{layer_err}")),
            }
            }
            BackendSelection::FallbackWindow => {
                match WaylandRuntime::new(BackendKind::FallbackWindow, ui, None) {
                    Ok(runtime) => Ok((
                        BackendKind::FallbackWindow,
                        "fallback_window".to_string(),
                        runtime,
                    )),
                    Err(err) => Err(format!("fallback_window_init_failed:{err}")),
                }
            }
            BackendSelection::Noop { reason } => {
                return BuiltBackend {
                    kind: BackendKind::Noop,
                    reason: reason.clone(),
                    backend: Box::new(NoopBackend { reason }),
                };
            }
        };

    match runtime {
        Ok((kind, reason, runtime)) => BuiltBackend {
            kind,
            reason,
            backend: Box::new(WaylandOverlayBackend::new(kind, ui, runtime, fonts)),
        },
        Err(reason) => BuiltBackend {
            kind: BackendKind::Noop,
            reason,
            backend: Box::new(NoopBackend {
                reason: "runtime_backend_init_failed".to_string(),
            }),
        },
    }
}

fn output_name_index(outputs: &[RuntimeOutputBinding], requested: &str) -> Option<usize> {
    output_name_match_index(
        &outputs
            .iter()
            .map(|entry| entry.name.as_deref())
            .collect::<Vec<_>>(),
        requested,
    )
}

fn output_name_match_index(output_names: &[Option<&str>], requested: &str) -> Option<usize> {
    output_names
        .iter()
        .position(|name| name.is_some_and(|name| name == requested))
}

struct WaylandRuntime {
    connection: Connection,
    event_queue: EventQueue<WaylandRuntimeState>,
    state: WaylandRuntimeState,
    surface: wl_surface::WlSurface,
    shell: ShellSurface,
    shm_buffer: ShmBuffer,
    dimensions: SurfaceDimensions,
    last_geometry: Option<(i32, i32, i32, i32)>,
    /// A painted frame could not be handed over because the compositor still
    /// holds every shm slot; the tick loop repaints until one is released.
    frame_pending: bool,
}

enum ShellSurface {
    Layer {
        layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    },
    Fallback {
        xdg_surface: xdg_surface::XdgSurface,
        toplevel: xdg_toplevel::XdgToplevel,
    },
}

/// Layer-shell margins are measured to the surface edge, so the shadow pads
/// (and the slide room above a top-anchored sheet) are taken off the requested
/// margins to keep the paper where the operator asked. Margins may go negative.
fn layer_margins_for_sheet(
    anchor: CliAnchor,
    margin_x: u32,
    margin_y: u32,
) -> (i32, i32, i32, i32) {
    let pad_y = if anchor.is_top() {
        galley::SHADOW_PAD_TOP + galley::SLIDE_ROOM
    } else {
        galley::SHADOW_PAD_BOTTOM
    };
    let x = margin_x as i32 - galley::SHADOW_PAD_SIDE as i32;
    let y = margin_y as i32 - pad_y as i32;
    layer_margins(anchor, x, y)
}

impl WaylandRuntime {
    fn new(kind: BackendKind, ui: &OverlayUiConfig, output_name: Option<&str>) -> Result<Self> {
        if kind == BackendKind::Noop {
            return Err(anyhow!(
                "cannot initialize Wayland runtime for noop backend"
            ));
        }

        let connection = Connection::connect_to_env().context("failed to connect to Wayland")?;
        let display = connection.display();
        let mut event_queue = connection.new_event_queue();
        let queue_handle = event_queue.handle();
        let _registry = display.get_registry(&queue_handle, ());

        let mut state = WaylandRuntimeState::default();
        event_queue
            .roundtrip(&mut state)
            .context("failed initial Wayland registry roundtrip")?;
        event_queue
            .roundtrip(&mut state)
            .context("failed secondary Wayland registry roundtrip")?;

        let compositor = state
            .globals
            .compositor
            .clone()
            .ok_or_else(|| anyhow!("wl_compositor unavailable"))?;
        let shm = state
            .globals
            .shm
            .clone()
            .ok_or_else(|| anyhow!("wl_shm unavailable"))?;

        let surface = compositor.create_surface(&queue_handle, ());
        let dimensions = ui.surface_dimensions();
        let shm_buffer = ShmBuffer::new(&shm, &queue_handle, dimensions)?;

        let shell = match kind {
            BackendKind::LayerShell => {
                let layer_shell = state
                    .globals
                    .layer_shell
                    .clone()
                    .ok_or_else(|| anyhow!("zwlr_layer_shell_v1 unavailable"))?;
                let target_output = output_name.and_then(|requested| {
                    output_name_index(&state.globals.outputs, requested)
                        .map(|index| state.globals.outputs[index].output.clone())
                });
                let layer_surface = layer_shell.get_layer_surface(
                    &surface,
                    target_output.as_ref(),
                    zwlr_layer_shell_v1::Layer::Overlay,
                    LAYER_NAMESPACE.to_string(),
                    &queue_handle,
                    (),
                );
                layer_surface.set_anchor(layer_anchor(ui.anchor));
                let (top, right, bottom, left) =
                    layer_margins_for_sheet(ui.anchor, ui.margin_x, ui.margin_y);
                layer_surface.set_margin(top, right, bottom, left);
                layer_surface.set_exclusive_zone(0);
                layer_surface
                    .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                // A fully transparent surface is still hit-testable on Wayland unless it
                // advertises an empty input region. Without this, the hidden bottom-center
                // layer surface leaves behind a "dead zone" over dock icons and web inputs.
                let input_region = compositor.create_region(&queue_handle, ());
                surface.set_input_region(Some(&input_region));
                input_region.destroy();
                layer_surface.set_size(dimensions.width, dimensions.height);
                ShellSurface::Layer { layer_surface }
            }
            BackendKind::FallbackWindow => {
                let xdg_wm_base = state
                    .globals
                    .xdg_wm_base
                    .clone()
                    .ok_or_else(|| anyhow!("xdg_wm_base unavailable"))?;
                let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &queue_handle, ());
                let toplevel = xdg_surface.get_toplevel(&queue_handle, ());
                toplevel.set_app_id("dev.parakeet.overlay".to_string());
                toplevel.set_title(FALLBACK_WINDOW_TITLE.to_string());
                xdg_surface.set_window_geometry(
                    0,
                    0,
                    dimensions.width as i32,
                    dimensions.height as i32,
                );
                ShellSurface::Fallback {
                    xdg_surface,
                    toplevel,
                }
            }
            BackendKind::Noop => return Err(anyhow!("unexpected noop backend kind")),
        };

        surface.commit();
        connection
            .flush()
            .context("failed to flush Wayland setup commit")?;
        event_queue
            .roundtrip(&mut state)
            .context("failed waiting for initial configure")?;

        Ok(Self {
            connection,
            event_queue,
            state,
            surface,
            shell,
            shm_buffer,
            dimensions,
            last_geometry: None,
            frame_pending: false,
        })
    }

    /// Paints one frame through `paint` and commits it. `paint` returns the sheet
    /// rectangle it drew, or `None` when the buffer is clear. When the compositor
    /// still holds every shm slot the frame is dropped and `frame_pending` asks
    /// the tick loop to paint again.
    fn present(
        &mut self,
        paint: impl FnOnce(&mut Frame) -> Option<Rect>,
        title: Option<&str>,
    ) -> Result<()> {
        self.frame_pending = false;
        self.pump_events("failed pre-render event dispatch")?;

        if self.state.closed {
            return Err(anyhow!("overlay surface closed by compositor"));
        }

        if !self.state.configured {
            self.event_queue
                .roundtrip(&mut self.state)
                .context("failed waiting for compositor configure")?;
        }

        let dimensions = self.dimensions;
        let sheet = {
            let mut frame = Frame {
                bytes: self.shm_buffer.bytes_mut(),
                width: dimensions.width,
                height: dimensions.height,
            };
            paint(&mut frame)
        };

        match (&self.shell, sheet) {
            (ShellSurface::Layer { layer_surface }, Some(sheet)) => {
                // The surface shrinks with the sheet so a centred anchor stays centred.
                let surface_width = (sheet.w.round() as u32 + 2 * galley::SHADOW_PAD_SIDE)
                    .clamp(1, dimensions.width);
                layer_surface.set_size(surface_width, dimensions.height);
                if !self.attach_full()? {
                    return Ok(());
                }
            }
            (ShellSurface::Layer { .. }, None) => {
                // Keep the (transparent) layer surface mapped so the next session
                // does not pay for a remap.
                if !self.attach_full()? {
                    return Ok(());
                }
            }
            (
                ShellSurface::Fallback {
                    xdg_surface,
                    toplevel,
                },
                Some(sheet),
            ) => {
                let geometry = (
                    sheet.x.round() as i32,
                    sheet.y.round() as i32,
                    sheet.w.round().max(1.0) as i32,
                    sheet.h.round().max(1.0) as i32,
                );
                if self.last_geometry != Some(geometry) {
                    xdg_surface.set_window_geometry(geometry.0, geometry.1, geometry.2, geometry.3);
                    self.last_geometry = Some(geometry);
                }
                toplevel.set_title(match title {
                    Some(title) if !title.is_empty() => {
                        format!("{FALLBACK_WINDOW_TITLE}: {}", truncate_for_title(title))
                    }
                    _ => FALLBACK_WINDOW_TITLE.to_string(),
                });
                if !self.attach_full()? {
                    return Ok(());
                }
            }
            (ShellSurface::Fallback { toplevel, .. }, None) => {
                toplevel.set_title(FALLBACK_WINDOW_TITLE.to_string());
                self.surface.attach(None, 0, 0);
                self.last_geometry = None;
            }
        }

        self.surface.commit();
        self.pump_events("failed post-render event dispatch")?;

        if self.state.closed {
            return Err(anyhow!("overlay surface closed by compositor"));
        }

        Ok(())
    }

    /// Copies the painted frame into a released shm slot and attaches it with
    /// full damage. Returns `false` (and flags `frame_pending`) when the
    /// compositor has not released any slot yet.
    fn attach_full(&mut self) -> Result<bool> {
        let Some(slot) = self.state.free_slot() else {
            debug!("overlay frame dropped: every shm slot is still held by the compositor");
            self.frame_pending = true;
            return Ok(false);
        };
        self.shm_buffer.sync_to_slot(slot)?;
        self.surface
            .attach(Some(&self.shm_buffer.slots[slot]), 0, 0);
        self.surface.damage_buffer(
            0,
            0,
            self.dimensions.width as i32,
            self.dimensions.height as i32,
        );
        self.state.busy_slots[slot] = true;
        Ok(true)
    }

    /// Flushes requests, reads whatever the compositor has sent without blocking,
    /// and dispatches it: buffer releases, later configures, and `Closed`.
    fn pump_events(&mut self, context: &'static str) -> Result<()> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context(context)?;
        self.connection.flush().context(context)?;
        if let Some(guard) = self.event_queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(anyhow!(err)).context(context),
            }
            self.event_queue
                .dispatch_pending(&mut self.state)
                .context(context)?;
        }
        Ok(())
    }
}

/// Number of shm slots. Two is enough: the compositor releases the previous
/// frame's slot once the newly committed one replaces it.
const SHM_SLOTS: usize = 2;

/// One frame painted on the CPU (`bytes`) plus `SHM_SLOTS` wl_buffers over a
/// shared pool. A slot is only rewritten after the compositor released it, as
/// wl_shm requires; painting into a buffer the compositor still reads tears.
struct ShmBuffer {
    file: File,
    _pool: wl_shm_pool::WlShmPool,
    slots: Vec<wl_buffer::WlBuffer>,
    slot_bytes: u64,
    bytes: Vec<u8>,
}

impl ShmBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        queue_handle: &QueueHandle<WaylandRuntimeState>,
        dimensions: SurfaceDimensions,
    ) -> Result<Self> {
        let stride = dimensions
            .width
            .checked_mul(4)
            .ok_or_else(|| anyhow!("overlay stride overflow"))?;
        let size_bytes = stride
            .checked_mul(dimensions.height)
            .ok_or_else(|| anyhow!("overlay buffer size overflow"))?;
        let pool_bytes = size_bytes
            .checked_mul(SHM_SLOTS as u32)
            .ok_or_else(|| anyhow!("overlay pool size overflow"))?;
        let pool_bytes_i32 = i32::try_from(pool_bytes).context("overlay buffer too large")?;
        let width_i32 = i32::try_from(dimensions.width).context("overlay width too large")?;
        let height_i32 = i32::try_from(dimensions.height).context("overlay height too large")?;
        let stride_i32 = i32::try_from(stride).context("overlay stride too large")?;

        let file = tempfile::tempfile().context("failed to create overlay shm tempfile")?;
        file.set_len(u64::from(pool_bytes))
            .context("failed to size overlay shm tempfile")?;

        let pool = shm.create_pool(file.as_fd(), pool_bytes_i32, queue_handle, ());
        let slots = (0..SHM_SLOTS)
            .map(|slot| {
                pool.create_buffer(
                    slot as i32 * (size_bytes as i32),
                    width_i32,
                    height_i32,
                    stride_i32,
                    wl_shm::Format::Argb8888,
                    queue_handle,
                    slot,
                )
            })
            .collect();

        Ok(Self {
            file,
            _pool: pool,
            slots,
            slot_bytes: u64::from(size_bytes),
            bytes: vec![0; usize::try_from(size_bytes).unwrap_or(0)],
        })
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    fn sync_to_slot(&mut self, slot: usize) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(slot as u64 * self.slot_bytes))
            .context("failed to seek overlay shm file")?;
        self.file
            .write_all(&self.bytes)
            .context("failed to write overlay shm pixel data")?;
        Ok(())
    }
}

#[derive(Default)]
struct WaylandRuntimeState {
    globals: RuntimeGlobals,
    configured: bool,
    closed: bool,
    /// Slots attached to the surface and not yet released by the compositor.
    busy_slots: [bool; SHM_SLOTS],
}

impl WaylandRuntimeState {
    fn free_slot(&self) -> Option<usize> {
        self.busy_slots.iter().position(|busy| !busy)
    }
}

#[derive(Default)]
struct RuntimeGlobals {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    xdg_wm_base: Option<xdg_wm_base::XdgWmBase>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    outputs: Vec<RuntimeOutputBinding>,
}

#[derive(Clone)]
struct RuntimeOutputBinding {
    output: wl_output::WlOutput,
    name: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.globals.compositor =
                        Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                            name,
                            version.min(6),
                            queue_handle,
                            (),
                        ));
                }
                "wl_shm" => {
                    state.globals.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                        name,
                        version.min(1),
                        queue_handle,
                        (),
                    ));
                }
                "xdg_wm_base" => {
                    state.globals.xdg_wm_base =
                        Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(
                            name,
                            version.min(1),
                            queue_handle,
                            (),
                        ));
                }
                "zwlr_layer_shell_v1" => {
                    state.globals.layer_shell = Some(
                        registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                            name,
                            version.min(4),
                            queue_handle,
                            (),
                        ),
                    );
                }
                "wl_output" => {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(
                        name,
                        version.min(4),
                        queue_handle,
                        (),
                    );
                    state
                        .globals
                        .outputs
                        .push(RuntimeOutputBinding { output, name: None });
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            for entry in &mut state.globals.outputs {
                if &entry.output == output {
                    entry.name = Some(name.clone());
                    break;
                }
            }
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        slot: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(busy) = state.busy_slots.get_mut(*slot) {
                *busy = false;
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_region::WlRegion, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width: _,
                height: _,
            } => {
                layer_surface.ack_configure(serial);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.closed = true;
            }
            _ => {}
        }
    }
}

fn probe_backend_signals() -> Result<BackendSignals> {
    let connection = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let display = connection.display();
    let mut event_queue = connection.new_event_queue();
    let queue_handle = event_queue.handle();
    let _registry = display.get_registry(&queue_handle, ());

    let mut state = ProbeState::default();
    event_queue
        .roundtrip(&mut state)
        .context("failed initial probe roundtrip")?;
    event_queue
        .roundtrip(&mut state)
        .context("failed secondary probe roundtrip")?;
    Ok(state.signals)
}

#[derive(Default)]
struct ProbeState {
    signals: BackendSignals,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ProbeState {
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
                "zwlr_layer_shell_v1" => state.signals.has_layer_shell = true,
                "wl_compositor" => state.signals.has_wl_compositor = true,
                "xdg_wm_base" => state.signals.has_xdg_wm_base = true,
                "wl_shm" => state.signals.has_wl_shm = true,
                _ => {}
            }
        }
    }
}

fn layer_anchor(anchor: CliAnchor) -> zwlr_layer_surface_v1::Anchor {
    use zwlr_layer_surface_v1::Anchor;

    match anchor {
        CliAnchor::TopLeft => Anchor::Top | Anchor::Left,
        CliAnchor::TopCenter => Anchor::Top,
        CliAnchor::TopRight => Anchor::Top | Anchor::Right,
        CliAnchor::BottomLeft => Anchor::Bottom | Anchor::Left,
        CliAnchor::BottomCenter => Anchor::Bottom,
        CliAnchor::BottomRight => Anchor::Bottom | Anchor::Right,
    }
}

fn layer_margins(anchor: CliAnchor, x: i32, y: i32) -> (i32, i32, i32, i32) {
    match anchor {
        CliAnchor::TopLeft => (y, 0, 0, x),
        CliAnchor::TopCenter => (y, 0, 0, 0),
        CliAnchor::TopRight => (y, x, 0, 0),
        CliAnchor::BottomLeft => (0, 0, y, x),
        CliAnchor::BottomCenter => (0, 0, y, 0),
        CliAnchor::BottomRight => (0, x, y, 0),
    }
}

fn truncate_for_title(input: &str) -> String {
    let trimmed = input.trim();
    let mut output = String::new();
    for character in trimmed.chars().take(96) {
        output.push(character);
    }
    if output.is_empty() {
        "(active)".to_string()
    } else {
        output
    }
}

fn is_probably_cosmic_session() -> bool {
    [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
    ]
    .iter()
    .filter_map(|key| std::env::var(key).ok())
    .any(|value| value.to_ascii_lowercase().contains("cosmic"))
}

pub async fn run_from_env_args() -> Result<()> {
    run_from_args(std::env::args_os()).await
}

pub async fn run_from_args<I, T>(args: I) -> Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    init_tracing();
    let cli = Cli::parse_from(args);
    let adaptive_width_enabled = resolve_adaptive_width_override(cli.adaptive_width);
    let ui = OverlayUiConfig {
        opacity: cli.opacity.clamp(0.0, 1.0),
        anchor: cli.anchor,
        margin_x: cli.margin_x,
        margin_y: cli.margin_y,
        max_width: cli.max_width,
        max_lines: cli.max_lines,
        adaptive_width_enabled,
    };
    let fonts = FontSet::load().context("failed to load the bundled overlay fonts")?;

    if let Some(dir) = cli.preview_dir.as_deref() {
        let written = preview::write_previews(dir, ui.sheet_spec(), fonts)
            .with_context(|| format!("failed writing overlay previews into {}", dir.display()))?;
        info!(count = written.len(), dir = %dir.display(), "overlay preview frames written");
        return Ok(());
    }

    let mut built_backend = build_backend(cli.backend, &ui, cli.output_name.as_deref(), fonts);
    info!(
        backend = ?built_backend.kind,
        reason = %built_backend.reason,
        opacity = ui.opacity,
        anchor = ?ui.anchor,
        margin_x = ui.margin_x,
        margin_y = ui.margin_y,
        max_width = ui.max_width,
        max_lines = ui.max_lines,
        adaptive_width_enabled = ui.adaptive_width_enabled,
        "overlay process started"
    );
    if built_backend.kind == BackendKind::FallbackWindow && is_probably_cosmic_session() {
        warn!(
            backend = "fallback_window",
            desktop = "cosmic",
            "overlay fallback-window is degraded on COSMIC (tiling/focus behavior is compositor-managed); prefer --backend layer-shell"
        );
    }

    let mut machine = OverlayStateMachine::new(Duration::from_millis(cli.auto_hide_ms.max(1)));
    let started = Instant::now();
    built_backend
        .backend
        .render(machine.visibility(), 0)
        .context("initial overlay render failed")?;

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

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
                            Ok(OverlayIpcMessage::AudioLevel { level_db, .. }) => {
                                // Levels arrive ~20 Hz; the tick paints them.
                                built_backend.backend.push_audio_level(level_db, now_ms);
                            }
                            Ok(message) => {
                                match machine.apply_event(message, now_ms) {
                                    ApplyOutcome::Applied => {
                                        built_backend
                                            .backend
                                            .render(machine.visibility(), now_ms)
                                            .context("overlay render failed while applying event")?;
                                    }
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
                let time_advanced = machine.advance_time(now_ms);
                if time_advanced || built_backend.backend.is_animating(now_ms) {
                    built_backend
                        .backend
                        .render(machine.visibility(), now_ms)
                        .context("overlay render failed on animation tick")?;
                }
            }
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::{
        layer_margins_for_sheet, output_name_match_index, resolve_adaptive_width_with_env_input,
        resolve_backend_selection, BackendSelection, BackendSignals, CliAnchor, CliBackendMode,
        OverlayUiConfig,
    };
    use clap::Parser;

    #[test]
    fn auto_prefers_layer_shell_when_available() {
        assert_eq!(
            resolve_backend_selection(
                CliBackendMode::Auto,
                Ok(BackendSignals {
                    has_layer_shell: true,
                    has_wl_compositor: true,
                    has_xdg_wm_base: true,
                    has_wl_shm: true,
                })
            ),
            BackendSelection::LayerShell
        );
    }

    #[test]
    fn auto_uses_fallback_when_layer_shell_missing() {
        assert_eq!(
            resolve_backend_selection(
                CliBackendMode::Auto,
                Ok(BackendSignals {
                    has_layer_shell: false,
                    has_wl_compositor: true,
                    has_xdg_wm_base: true,
                    has_wl_shm: true,
                })
            ),
            BackendSelection::FallbackWindow
        );
    }

    #[test]
    fn explicit_layer_shell_disables_when_unsupported() {
        assert_eq!(
            resolve_backend_selection(
                CliBackendMode::LayerShell,
                Ok(BackendSignals {
                    has_layer_shell: false,
                    has_wl_compositor: true,
                    has_xdg_wm_base: true,
                    has_wl_shm: true,
                })
            ),
            BackendSelection::Noop {
                reason: "unsupported_wayland_backend:layer_shell".to_string(),
            }
        );
    }

    #[test]
    fn probe_failure_degrades_to_noop() {
        assert_eq!(
            resolve_backend_selection(CliBackendMode::Auto, Err("no_display".to_string())),
            BackendSelection::Noop {
                reason: "wayland_probe_failed:no_display".to_string(),
            }
        );
    }

    #[test]
    fn default_cli_is_bottom_center_full_opacity_galley_width() {
        let cli = super::Cli::parse_from(["parakeet-overlay"]);
        assert!(matches!(cli.anchor, CliAnchor::BottomCenter));
        assert_eq!(cli.margin_y, 32);
        assert_eq!(cli.opacity, 1.0);
        assert_eq!(cli.max_width, super::galley::DEFAULT_SHEET_WIDTH);
        assert!(cli.preview_dir.is_none());
    }

    #[test]
    fn cli_no_longer_accepts_a_font_descriptor() {
        assert!(super::Cli::try_parse_from(["parakeet-overlay", "--font", "Sans 16"]).is_err());
    }

    #[test]
    fn cli_parses_output_name_arg() {
        let cli = super::Cli::parse_from(["parakeet-overlay", "--output-name", "HDMI-A-1"]);
        assert_eq!(cli.output_name.as_deref(), Some("HDMI-A-1"));
    }

    #[test]
    fn cli_adaptive_width_defaults_to_none() {
        let cli = super::Cli::parse_from(["parakeet-overlay"]);
        assert_eq!(cli.adaptive_width, None);
    }

    #[test]
    fn cli_parses_adaptive_width_arg() {
        let enabled = super::Cli::parse_from(["parakeet-overlay", "--adaptive-width", "true"]);
        assert_eq!(enabled.adaptive_width, Some(true));

        let disabled = super::Cli::parse_from(["parakeet-overlay", "--adaptive-width", "false"]);
        assert_eq!(disabled.adaptive_width, Some(false));
    }

    #[test]
    fn cli_parses_preview_dir() {
        let cli = super::Cli::parse_from(["parakeet-overlay", "--preview-dir", "/tmp/x"]);
        assert_eq!(
            cli.preview_dir.as_deref(),
            Some(std::path::Path::new("/tmp/x"))
        );
    }

    #[test]
    fn resolve_adaptive_width_override_defaults_to_enabled() {
        assert!(resolve_adaptive_width_with_env_input(None, None));
    }

    #[test]
    fn resolve_adaptive_width_override_honors_env_and_cli_precedence() {
        assert!(!resolve_adaptive_width_with_env_input(None, Some("false")));
        assert!(resolve_adaptive_width_with_env_input(None, Some("true")));
        assert!(resolve_adaptive_width_with_env_input(
            Some(true),
            Some("false")
        ));
        assert!(!resolve_adaptive_width_with_env_input(
            Some(false),
            Some("true")
        ));
    }

    #[test]
    fn output_matching_finds_correct_output() {
        let outputs = [Some("DP-1"), Some("HDMI-A-1")];
        assert_eq!(output_name_match_index(&outputs, "HDMI-A-1"), Some(1));
    }

    #[test]
    fn output_matching_falls_back_to_none() {
        let outputs = [Some("DP-1"), None];
        assert_eq!(output_name_match_index(&outputs, "UNKNOWN"), None);
    }

    fn ui() -> OverlayUiConfig {
        OverlayUiConfig {
            opacity: 1.0,
            anchor: CliAnchor::BottomCenter,
            margin_x: 24,
            margin_y: 32,
            max_width: super::galley::DEFAULT_SHEET_WIDTH,
            max_lines: 4,
            adaptive_width_enabled: true,
        }
    }

    #[test]
    fn surface_dimensions_come_from_the_galley_layout() {
        let dims = ui().surface_dimensions();
        let (width, height) = super::galley::buffer_size(super::galley::DEFAULT_SHEET_WIDTH, 4);
        assert_eq!((dims.width, dims.height), (width, height));

        let wide = OverlayUiConfig {
            max_width: 5000,
            ..ui()
        };
        assert_eq!(wide.content_width(), super::galley::MAX_SHEET_WIDTH);
        let narrow = OverlayUiConfig {
            max_width: 10,
            ..ui()
        };
        assert_eq!(narrow.content_width(), super::galley::MIN_SHEET_WIDTH);
    }

    #[test]
    fn layer_margins_take_the_shadow_pads_off() {
        let (top, right, bottom, left) = layer_margins_for_sheet(CliAnchor::BottomCenter, 24, 32);
        assert_eq!((top, right, left), (0, 0, 0));
        assert_eq!(bottom, 32 - super::galley::SHADOW_PAD_BOTTOM as i32);

        let (top, right, bottom, left) = layer_margins_for_sheet(CliAnchor::TopRight, 30, 40);
        assert_eq!(
            top,
            40 - (super::galley::SHADOW_PAD_TOP + super::galley::SLIDE_ROOM) as i32
        );
        assert_eq!(right, 30 - super::galley::SHADOW_PAD_SIDE as i32);
        assert_eq!((bottom, left), (0, 0));
    }
}
