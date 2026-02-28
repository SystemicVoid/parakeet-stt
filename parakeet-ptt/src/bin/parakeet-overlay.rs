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
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use parakeet_ptt::overlay_ipc::OverlayIpcMessage;
use parakeet_ptt::overlay_state::{
    ApplyOutcome, OverlayRenderIntent, OverlayRenderPhase, OverlayStateMachine, OverlayVisibility,
};

const FALLBACK_WINDOW_TITLE: &str = "Parakeet Overlay";
const LAYER_NAMESPACE: &str = "parakeet-overlay";

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
    Noop,
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

impl OverlayUiConfig {
    fn surface_dimensions(&self) -> SurfaceDimensions {
        let width = self.max_width.clamp(320, 3840);
        let clamped_lines = self.max_lines.clamp(1, 10);
        let height = (24 + clamped_lines * 34).clamp(72, 720);
        SurfaceDimensions { width, height }
    }
}

#[derive(Debug, Clone, Copy)]
struct SurfaceDimensions {
    width: u32,
    height: u32,
}

trait OverlayBackend {
    fn render(&mut self, state: &OverlayVisibility);
}

#[derive(Debug)]
struct NoopBackend {
    reason: String,
}

impl OverlayBackend for NoopBackend {
    fn render(&mut self, state: &OverlayVisibility) {
        debug!(reason = %self.reason, ?state, "overlay renderer running in noop mode");
    }
}

struct WaylandOverlayBackend {
    kind: BackendKind,
    ui: OverlayUiConfig,
    runtime: Option<WaylandRuntime>,
}

impl WaylandOverlayBackend {
    fn new(kind: BackendKind, ui: OverlayUiConfig, runtime: WaylandRuntime) -> Self {
        Self {
            kind,
            ui,
            runtime: Some(runtime),
        }
    }
}

impl OverlayBackend for WaylandOverlayBackend {
    fn render(&mut self, state: &OverlayVisibility) {
        let Some(runtime) = self.runtime.as_mut() else {
            return;
        };

        if let Err(err) = runtime.render(&state.to_render_intent(), &self.ui) {
            warn!(
                backend = ?self.kind,
                error = %err,
                "overlay renderer backend failed; switching to noop mode"
            );
            self.runtime = None;
        }
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

fn build_backend(mode: CliBackendMode, ui: OverlayUiConfig) -> BuiltBackend {
    let probe_result = probe_backend_signals().map_err(|err| err.to_string());
    let selection = resolve_backend_selection(mode, probe_result);

    match selection {
        BackendSelection::LayerShell => match WaylandRuntime::new(BackendKind::LayerShell, &ui) {
            Ok(runtime) => BuiltBackend {
                kind: BackendKind::LayerShell,
                reason: "layer_shell".to_string(),
                backend: Box::new(WaylandOverlayBackend::new(
                    BackendKind::LayerShell,
                    ui,
                    runtime,
                )),
            },
            Err(layer_err) => {
                if matches!(mode, CliBackendMode::Auto) {
                    match WaylandRuntime::new(BackendKind::FallbackWindow, &ui) {
                            Ok(runtime) => BuiltBackend {
                                kind: BackendKind::FallbackWindow,
                                reason: format!(
                                    "layer_shell_init_failed:{layer_err};using_fallback_window"
                                ),
                                backend: Box::new(WaylandOverlayBackend::new(
                                    BackendKind::FallbackWindow,
                                    ui,
                                    runtime,
                                )),
                            },
                            Err(fallback_err) => BuiltBackend {
                                kind: BackendKind::Noop,
                                reason: format!(
                                    "layer_shell_init_failed:{layer_err};fallback_init_failed:{fallback_err}"
                                ),
                                backend: Box::new(NoopBackend {
                                    reason: "runtime_backend_init_failed".to_string(),
                                }),
                            },
                        }
                } else {
                    BuiltBackend {
                        kind: BackendKind::Noop,
                        reason: format!("layer_shell_init_failed:{layer_err}"),
                        backend: Box::new(NoopBackend {
                            reason: "runtime_backend_init_failed".to_string(),
                        }),
                    }
                }
            }
        },
        BackendSelection::FallbackWindow => {
            match WaylandRuntime::new(BackendKind::FallbackWindow, &ui) {
                Ok(runtime) => BuiltBackend {
                    kind: BackendKind::FallbackWindow,
                    reason: "fallback_window".to_string(),
                    backend: Box::new(WaylandOverlayBackend::new(
                        BackendKind::FallbackWindow,
                        ui,
                        runtime,
                    )),
                },
                Err(err) => BuiltBackend {
                    kind: BackendKind::Noop,
                    reason: format!("fallback_window_init_failed:{err}"),
                    backend: Box::new(NoopBackend {
                        reason: "runtime_backend_init_failed".to_string(),
                    }),
                },
            }
        }
        BackendSelection::Noop { reason } => BuiltBackend {
            kind: BackendKind::Noop,
            reason: reason.clone(),
            backend: Box::new(NoopBackend { reason }),
        },
    }
}

struct WaylandRuntime {
    connection: Connection,
    event_queue: EventQueue<WaylandRuntimeState>,
    state: WaylandRuntimeState,
    surface: wl_surface::WlSurface,
    shell: ShellSurface,
    shm_buffer: ShmBuffer,
    dimensions: SurfaceDimensions,
}

enum ShellSurface {
    Layer {
        _layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    },
    Fallback {
        _xdg_surface: xdg_surface::XdgSurface,
        toplevel: xdg_toplevel::XdgToplevel,
    },
}

impl WaylandRuntime {
    fn new(kind: BackendKind, ui: &OverlayUiConfig) -> Result<Self> {
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
        let mut shm_buffer = ShmBuffer::new(&shm, &queue_handle, dimensions)?;
        shm_buffer.paint(argb_pixel(0, 0, 0, 0))?;

        let shell = match kind {
            BackendKind::LayerShell => {
                let layer_shell = state
                    .globals
                    .layer_shell
                    .clone()
                    .ok_or_else(|| anyhow!("zwlr_layer_shell_v1 unavailable"))?;
                let layer_surface = layer_shell.get_layer_surface(
                    &surface,
                    None,
                    zwlr_layer_shell_v1::Layer::Overlay,
                    LAYER_NAMESPACE.to_string(),
                    &queue_handle,
                    (),
                );
                layer_surface.set_anchor(layer_anchor(ui.anchor));
                let (top, right, bottom, left) = layer_margins(ui.anchor, ui.margin_x, ui.margin_y);
                layer_surface.set_margin(top, right, bottom, left);
                layer_surface.set_exclusive_zone(0);
                layer_surface
                    .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                layer_surface.set_size(dimensions.width, dimensions.height);
                ShellSurface::Layer {
                    _layer_surface: layer_surface,
                }
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
                    _xdg_surface: xdg_surface,
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
        })
    }

    fn render(&mut self, intent: &OverlayRenderIntent, ui: &OverlayUiConfig) -> Result<()> {
        self.dispatch_pending("failed pre-render event dispatch")?;

        if self.state.closed {
            return Err(anyhow!("overlay surface closed by compositor"));
        }

        if !self.state.configured {
            self.event_queue
                .roundtrip(&mut self.state)
                .context("failed waiting for compositor configure")?;
        }

        if intent.visible {
            self.shm_buffer
                .paint(color_for_phase(intent.phase, ui.opacity))?;
            self.surface.attach(Some(&self.shm_buffer.buffer), 0, 0);
            self.surface.damage_buffer(
                0,
                0,
                self.dimensions.width as i32,
                self.dimensions.height as i32,
            );
            if let ShellSurface::Fallback { toplevel, .. } = &self.shell {
                toplevel.set_title(format!(
                    "{FALLBACK_WINDOW_TITLE}: {}",
                    truncate_for_title(&intent.headline)
                ));
            }
        } else {
            self.surface.attach(None, 0, 0);
            if let ShellSurface::Fallback { toplevel, .. } = &self.shell {
                toplevel.set_title(FALLBACK_WINDOW_TITLE.to_string());
            }
        }

        self.surface.commit();
        self.connection
            .flush()
            .context("failed flushing Wayland render updates")?;
        self.dispatch_pending("failed post-render event dispatch")?;

        if self.state.closed {
            return Err(anyhow!("overlay surface closed by compositor"));
        }

        Ok(())
    }

    fn dispatch_pending(&mut self, context: &'static str) -> Result<()> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context(context)?;
        Ok(())
    }
}

struct ShmBuffer {
    file: File,
    _pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
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
        let size_bytes_i32 = i32::try_from(size_bytes).context("overlay buffer too large")?;
        let width_i32 = i32::try_from(dimensions.width).context("overlay width too large")?;
        let height_i32 = i32::try_from(dimensions.height).context("overlay height too large")?;
        let stride_i32 = i32::try_from(stride).context("overlay stride too large")?;

        let file = tempfile::tempfile().context("failed to create overlay shm tempfile")?;
        file.set_len(u64::from(size_bytes))
            .context("failed to size overlay shm tempfile")?;

        let pool = shm.create_pool(file.as_fd(), size_bytes_i32, queue_handle, ());
        let buffer = pool.create_buffer(
            0,
            width_i32,
            height_i32,
            stride_i32,
            wl_shm::Format::Argb8888,
            queue_handle,
            (),
        );

        Ok(Self {
            file,
            _pool: pool,
            buffer,
            bytes: vec![0; usize::try_from(size_bytes).unwrap_or(0)],
        })
    }

    fn paint(&mut self, pixel: [u8; 4]) -> Result<()> {
        for chunk in self.bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }
        self.file
            .seek(SeekFrom::Start(0))
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
}

#[derive(Default)]
struct RuntimeGlobals {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    xdg_wm_base: Option<xdg_wm_base::XdgWmBase>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
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

impl Dispatch<wl_buffer::WlBuffer, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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

fn layer_margins(anchor: CliAnchor, margin_x: u32, margin_y: u32) -> (i32, i32, i32, i32) {
    let x = margin_x as i32;
    let y = margin_y as i32;

    match anchor {
        CliAnchor::TopLeft => (y, 0, 0, x),
        CliAnchor::TopCenter => (y, 0, 0, 0),
        CliAnchor::TopRight => (y, x, 0, 0),
        CliAnchor::BottomLeft => (0, 0, y, x),
        CliAnchor::BottomCenter => (0, 0, y, 0),
        CliAnchor::BottomRight => (0, x, y, 0),
    }
}

fn color_for_phase(phase: OverlayRenderPhase, opacity: f32) -> [u8; 4] {
    let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;

    match phase {
        OverlayRenderPhase::Hidden => argb_pixel(0, 0, 0, 0),
        OverlayRenderPhase::Listening => argb_pixel(38, 113, 199, alpha),
        OverlayRenderPhase::Interim => argb_pixel(26, 146, 92, alpha),
        OverlayRenderPhase::Finalizing => argb_pixel(184, 126, 36, alpha),
    }
}

fn argb_pixel(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    [b, g, r, a]
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

    let mut built_backend = build_backend(cli.backend, ui.clone());
    info!(
        backend = ?built_backend.kind,
        reason = %built_backend.reason,
        opacity = ui.opacity,
        font = %ui.font,
        anchor = ?ui.anchor,
        margin_x = ui.margin_x,
        margin_y = ui.margin_y,
        max_width = ui.max_width,
        max_lines = ui.max_lines,
        "overlay process started"
    );

    let mut machine = OverlayStateMachine::new(Duration::from_millis(cli.auto_hide_ms.max(1)));
    built_backend.backend.render(machine.visibility());

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
                                    ApplyOutcome::Applied => built_backend.backend.render(machine.visibility()),
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
                    built_backend.backend.render(machine.visibility());
                }
            }
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::{
        color_for_phase, resolve_backend_selection, BackendSelection, BackendSignals,
        CliBackendMode,
    };
    use parakeet_ptt::overlay_state::OverlayRenderPhase;

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
    fn hidden_phase_color_is_transparent() {
        assert_eq!(
            color_for_phase(OverlayRenderPhase::Hidden, 0.8),
            [0, 0, 0, 0]
        );
    }
}
