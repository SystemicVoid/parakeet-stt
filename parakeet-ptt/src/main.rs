mod audio_feedback;
mod client;
mod config;
mod hotkey;
mod injector;
mod overlay_process;
mod protocol;
mod routing;
mod state;
mod surface_focus;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::{
    sleep, timeout, Duration as TokioDuration, Instant as TokioInstant, MissedTickBehavior,
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use crate::audio_feedback::AudioFeedback;
use crate::client::WsClient;
use crate::config::{
    probe_overlay_capability, ClientConfig, ClipboardOptions, InjectionConfig, OverlayMode,
    DEFAULT_ENDPOINT,
};
use crate::hotkey::{ensure_input_access, spawn_hotkey_loop, HotkeyEvent};
use crate::injector::{injector_metrics_snapshot, TextInjector};
use crate::overlay_process::OverlayProcessSink;
use crate::protocol::{
    decode_server_message, start_message, stop_message, DecodedServerMessage, ServerMessage,
};
use crate::state::PttState;
use parakeet_ptt::overlay_ipc::OverlayIpcMessage;

const INJECTION_QUEUE_CAPACITY: usize = 32;
const INJECTION_ENQUEUE_TIMEOUT_MS: u64 = 20;
const EVENT_LOOP_LAG_TICK_MS: u64 = 10;
const EVENT_LOOP_LAG_LOG_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone)]
struct InjectionJob {
    session_id: Uuid,
    text: String,
    daemon_latency_ms: u64,
    daemon_audio_ms: u64,
    enqueued_at: TokioInstant,
}

impl InjectionJob {
    fn new(session_id: Uuid, text: String, daemon_latency_ms: u64, daemon_audio_ms: u64) -> Self {
        Self {
            session_id,
            text,
            daemon_latency_ms,
            daemon_audio_ms,
            enqueued_at: TokioInstant::now(),
        }
    }
}

#[derive(Debug)]
struct InjectionReport {
    session_id: Uuid,
    daemon_latency_ms: u64,
    daemon_audio_ms: u64,
    queue_wait_ms: u64,
    run_ms: u64,
    total_worker_ms: u64,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct InjectorQueueMetrics {
    queued_total: AtomicU64,
    enqueue_blocked_total: AtomicU64,
    enqueue_timeout_total: AtomicU64,
    enqueue_worker_gone_total: AtomicU64,
    worker_success_total: AtomicU64,
    worker_failure_total: AtomicU64,
    worker_queue_wait_ms_total: AtomicU64,
    worker_run_ms_total: AtomicU64,
    queue_depth_high_water: AtomicU64,
}

#[derive(Debug, Default)]
struct OverlayRoutingMetrics {
    routed_interim_state_total: AtomicU64,
    routed_interim_text_total: AtomicU64,
    routed_session_ended_total: AtomicU64,
    dropped_stale_seq_total: AtomicU64,
    dropped_session_mismatch_total: AtomicU64,
}

impl OverlayRoutingMetrics {
    fn note_interim_state(&self) {
        self.routed_interim_state_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_interim_text(&self) {
        self.routed_interim_text_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_session_ended(&self) {
        self.routed_session_ended_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_stale_seq_drop(&self) {
        self.dropped_stale_seq_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_session_mismatch_drop(&self) {
        self.dropped_session_mismatch_total
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OverlayEvent {
    InterimState {
        session_id: Uuid,
        seq: u64,
        state: String,
    },
    InterimText {
        session_id: Uuid,
        seq: u64,
        text: String,
    },
    SessionEnded {
        session_id: Uuid,
        reason: Option<String>,
    },
}

trait OverlaySink {
    fn on_overlay_event(&mut self, event: OverlayEvent);
}

#[derive(Debug, Default)]
struct NoopOverlaySink;

impl OverlaySink for NoopOverlaySink {
    fn on_overlay_event(&mut self, event: OverlayEvent) {
        debug!(?event, "overlay event dropped by noop sink");
    }
}

enum RuntimeOverlaySink {
    Noop(NoopOverlaySink),
    Process(OverlayProcessSink),
}

impl OverlaySink for RuntimeOverlaySink {
    fn on_overlay_event(&mut self, event: OverlayEvent) {
        match self {
            Self::Noop(sink) => sink.on_overlay_event(event),
            Self::Process(sink) => sink.send(overlay_event_to_ipc(event)),
        }
    }
}

fn overlay_event_to_ipc(event: OverlayEvent) -> OverlayIpcMessage {
    match event {
        OverlayEvent::InterimState {
            session_id,
            seq,
            state,
        } => OverlayIpcMessage::InterimState {
            session_id,
            seq,
            state,
        },
        OverlayEvent::InterimText {
            session_id,
            seq,
            text,
        } => OverlayIpcMessage::InterimText {
            session_id,
            seq,
            text,
        },
        OverlayEvent::SessionEnded { session_id, reason } => {
            OverlayIpcMessage::SessionEnded { session_id, reason }
        }
    }
}

fn build_runtime_overlay_sink(mode: OverlayMode) -> RuntimeOverlaySink {
    match mode {
        OverlayMode::Disabled => RuntimeOverlaySink::Noop(NoopOverlaySink),
        OverlayMode::LayerShell | OverlayMode::FallbackWindow => {
            match OverlayProcessSink::spawn(mode) {
                Ok(process) => {
                    let metrics = process.metrics();
                    info!(
                        overlay_launch_success_total =
                            metrics.launch_success_total.load(Ordering::Relaxed),
                        overlay_launch_failure_total =
                            metrics.launch_failure_total.load(Ordering::Relaxed),
                        "overlay process routing enabled"
                    );
                    RuntimeOverlaySink::Process(process)
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        "overlay process unavailable; continuing with no-op overlay sink"
                    );
                    RuntimeOverlaySink::Noop(NoopOverlaySink)
                }
            }
        }
    }
}

struct OverlayRouter<S: OverlaySink> {
    sink: S,
    metrics: Arc<OverlayRoutingMetrics>,
    active_session_id: Option<Uuid>,
    last_seq: Option<u64>,
}

impl<S: OverlaySink> OverlayRouter<S> {
    fn new(sink: S) -> Self {
        Self {
            sink,
            metrics: Arc::new(OverlayRoutingMetrics::default()),
            active_session_id: None,
            last_seq: None,
        }
    }

    fn metrics(&self) -> &Arc<OverlayRoutingMetrics> {
        &self.metrics
    }

    fn note_session_started(&mut self, session_id: Uuid) {
        if self.active_session_id != Some(session_id) {
            self.active_session_id = Some(session_id);
            self.last_seq = None;
        }
    }

    fn route_interim_state(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        state: String,
    ) {
        if !self.allow_session(expected_session_id, session_id) || !self.accept_seq(session_id, seq)
        {
            return;
        }

        self.sink.on_overlay_event(OverlayEvent::InterimState {
            session_id,
            seq,
            state,
        });
        self.metrics.note_interim_state();
    }

    fn route_interim_text(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        text: String,
    ) {
        if !self.allow_session(expected_session_id, session_id) || !self.accept_seq(session_id, seq)
        {
            return;
        }

        self.sink.on_overlay_event(OverlayEvent::InterimText {
            session_id,
            seq,
            text,
        });
        self.metrics.note_interim_text();
    }

    fn route_session_ended(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        reason: Option<String>,
    ) {
        if !self.allow_session(expected_session_id, session_id) {
            return;
        }

        self.sink
            .on_overlay_event(OverlayEvent::SessionEnded { session_id, reason });
        self.metrics.note_session_ended();

        if self.active_session_id == Some(session_id) {
            self.active_session_id = None;
            self.last_seq = None;
        }
    }

    fn allow_session(&self, expected_session_id: Option<Uuid>, incoming_session_id: Uuid) -> bool {
        match expected_session_id {
            Some(expected) if expected != incoming_session_id => {
                self.metrics.note_session_mismatch_drop();
                debug!(
                    expected_session = %expected,
                    incoming_session = %incoming_session_id,
                    "dropping overlay event for mismatched active session"
                );
                false
            }
            _ => true,
        }
    }

    fn accept_seq(&mut self, incoming_session_id: Uuid, seq: u64) -> bool {
        if self.active_session_id != Some(incoming_session_id) {
            self.active_session_id = Some(incoming_session_id);
            self.last_seq = None;
        }

        if let Some(last_seq) = self.last_seq {
            if seq <= last_seq {
                self.metrics.note_stale_seq_drop();
                debug!(
                    session = %incoming_session_id,
                    seq,
                    last_seq,
                    "dropping stale overlay event sequence"
                );
                return false;
            }
        }

        self.last_seq = Some(seq);
        true
    }
}

impl InjectorQueueMetrics {
    fn note_queued(&self, queue_depth: usize) {
        self.queued_total.fetch_add(1, Ordering::Relaxed);
        let depth = queue_depth as u64;
        let mut current = self.queue_depth_high_water.load(Ordering::Relaxed);
        while depth > current {
            match self.queue_depth_high_water.compare_exchange_weak(
                current,
                depth,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn note_blocked(&self) {
        self.enqueue_blocked_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_timeout(&self) {
        self.enqueue_timeout_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_worker_gone(&self) {
        self.enqueue_worker_gone_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_report(&self, report: &InjectionReport) {
        self.worker_queue_wait_ms_total
            .fetch_add(report.queue_wait_ms, Ordering::Relaxed);
        self.worker_run_ms_total
            .fetch_add(report.run_ms, Ordering::Relaxed);
        if report.error.is_some() {
            self.worker_failure_total.fetch_add(1, Ordering::Relaxed);
        } else {
            self.worker_success_total.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn log_summary(&self) {
        info!(
            queue_queued_total = self.queued_total.load(Ordering::Relaxed),
            queue_enqueue_blocked_total = self.enqueue_blocked_total.load(Ordering::Relaxed),
            queue_enqueue_timeout_total = self.enqueue_timeout_total.load(Ordering::Relaxed),
            queue_enqueue_worker_gone_total =
                self.enqueue_worker_gone_total.load(Ordering::Relaxed),
            worker_success_total = self.worker_success_total.load(Ordering::Relaxed),
            worker_failure_total = self.worker_failure_total.load(Ordering::Relaxed),
            worker_queue_wait_ms_total = self.worker_queue_wait_ms_total.load(Ordering::Relaxed),
            worker_run_ms_total = self.worker_run_ms_total.load(Ordering::Relaxed),
            queue_depth_high_water = self.queue_depth_high_water.load(Ordering::Relaxed),
            "injector worker queue metrics summary"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnqueueFailure {
    Timeout,
    WorkerGone,
}

#[derive(Clone)]
struct InjectorWorkerHandle {
    sender: mpsc::Sender<InjectionJob>,
    metrics: Arc<InjectorQueueMetrics>,
}

impl InjectorWorkerHandle {
    async fn enqueue(&self, job: InjectionJob) -> std::result::Result<(), EnqueueFailure> {
        let current_depth = self.sender.max_capacity() - self.sender.capacity();
        let job = match self.sender.try_send(job) {
            Ok(()) => {
                self.metrics.note_queued(current_depth + 1);
                return Ok(());
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.note_worker_gone();
                return Err(EnqueueFailure::WorkerGone);
            }
            Err(mpsc::error::TrySendError::Full(job)) => {
                self.metrics.note_blocked();
                job
            }
        };

        match timeout(
            TokioDuration::from_millis(INJECTION_ENQUEUE_TIMEOUT_MS),
            self.sender.send(job),
        )
        .await
        {
            Ok(Ok(())) => {
                let new_depth = self.sender.max_capacity() - self.sender.capacity();
                self.metrics.note_queued(new_depth);
                Ok(())
            }
            Ok(Err(_)) => {
                self.metrics.note_worker_gone();
                Err(EnqueueFailure::WorkerGone)
            }
            Err(_) => {
                self.metrics.note_timeout();
                Err(EnqueueFailure::Timeout)
            }
        }
    }

    fn metrics(&self) -> &Arc<InjectorQueueMetrics> {
        &self.metrics
    }
}

fn spawn_injector_worker_with_capacity(
    injector: Arc<dyn TextInjector>,
    capacity: usize,
) -> (
    InjectorWorkerHandle,
    mpsc::UnboundedReceiver<InjectionReport>,
) {
    let (job_tx, mut job_rx) = mpsc::channel::<InjectionJob>(capacity.max(1));
    let (report_tx, report_rx) = mpsc::unbounded_channel::<InjectionReport>();
    let metrics = Arc::new(InjectorQueueMetrics::default());
    let worker_injector = Arc::clone(&injector);

    tokio::spawn(async move {
        while let Some(job) = job_rx.recv().await {
            let InjectionJob {
                session_id,
                text,
                daemon_latency_ms,
                daemon_audio_ms,
                enqueued_at,
            } = job;

            let queue_wait_ms = enqueued_at.elapsed().as_millis() as u64;
            let worker_started = TokioInstant::now();
            let injector_for_job = Arc::clone(&worker_injector);
            let result = tokio::task::spawn_blocking(move || injector_for_job.inject(&text)).await;
            let run_ms = worker_started.elapsed().as_millis() as u64;
            let total_worker_ms = queue_wait_ms.saturating_add(run_ms);

            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(err)) => Some(format!("{err:#}")),
                Err(err) => Some(format!("injector worker task failed: {err}")),
            };
            let report = InjectionReport {
                session_id,
                daemon_latency_ms,
                daemon_audio_ms,
                queue_wait_ms,
                run_ms,
                total_worker_ms,
                error,
            };

            if report_tx.send(report).is_err() {
                break;
            }
        }
    });

    (
        InjectorWorkerHandle {
            sender: job_tx,
            metrics,
        },
        report_rx,
    )
}

fn spawn_injector_worker(
    injector: Arc<dyn TextInjector>,
) -> (
    InjectorWorkerHandle,
    mpsc::UnboundedReceiver<InjectionReport>,
) {
    spawn_injector_worker_with_capacity(injector, INJECTION_QUEUE_CAPACITY)
}

fn percentile_value(sorted_samples: &[u64], percentile: u64) -> u64 {
    if sorted_samples.is_empty() {
        return 0;
    }

    let pct = percentile.min(100) as usize;
    let len = sorted_samples.len();
    let idx = ((len - 1) * pct) / 100;
    sorted_samples[idx]
}

fn spawn_event_loop_lag_monitor() {
    tokio::spawn(async move {
        let tick = TokioDuration::from_millis(EVENT_LOOP_LAG_TICK_MS.max(1));
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut last_log = TokioInstant::now();
        let mut lag_samples_ms = Vec::<u64>::with_capacity(4096);

        loop {
            let scheduled = interval.tick().await;
            let now = TokioInstant::now();
            let lag_ms = now.saturating_duration_since(scheduled).as_millis() as u64;
            lag_samples_ms.push(lag_ms);

            if last_log.elapsed() >= TokioDuration::from_secs(EVENT_LOOP_LAG_LOG_INTERVAL_SECS) {
                lag_samples_ms.sort_unstable();
                let p50 = percentile_value(&lag_samples_ms, 50);
                let p95 = percentile_value(&lag_samples_ms, 95);
                let p99 = percentile_value(&lag_samples_ms, 99);
                info!(
                    sample_count = lag_samples_ms.len(),
                    lag_p50_ms = p50,
                    lag_p95_ms = p95,
                    lag_p99_ms = p99,
                    target_p99_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                    "event loop lag window summary"
                );
                lag_samples_ms.clear();
                last_log = TokioInstant::now();
            }
        }
    });
}

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

async fn run_demo(
    config: ClientConfig,
    override_text: Option<String>,
    audio_feedback: AudioFeedback,
) -> Result<()> {
    info!(endpoint = %config.endpoint, "Connecting to parakeet-stt-daemon");
    let mut client = WsClient::connect(&config).await?;
    let injector = build_injector(&config);
    let (injector_worker, mut injection_reports) = spawn_injector_worker(Arc::clone(&injector));

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
                injector_worker
                    .enqueue(InjectionJob::new(
                        session_id, to_inject, latency_ms, audio_ms,
                    ))
                    .await
                    .map_err(|failure| {
                        anyhow!("failed to enqueue demo injection job: {:?}", failure)
                    })?;
                let report = timeout(TokioDuration::from_secs(5), injection_reports.recv())
                    .await
                    .context("timed out waiting for demo injection report")?
                    .ok_or_else(|| anyhow!("demo injection worker dropped before reporting"))?;
                if let Some(error) = report.error {
                    return Err(anyhow!("demo injection failed: {error}"));
                }
                state.reset();
                break;
            }
            ServerMessage::Error {
                session_id,
                code,
                message,
            } => {
                let error_kind = classify_error_code(&code);
                warn!(
                    session = ?session_id,
                    error_code = %code,
                    error_kind,
                    "daemon error: {}",
                    message
                );
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
    let overlay_capability = probe_overlay_capability();
    match overlay_capability.mode {
        OverlayMode::Disabled => {
            warn!(
                overlay_mode = overlay_capability.mode.as_str(),
                overlay_reason = %overlay_capability.reason,
                "overlay capability probe completed with disabled mode"
            );
        }
        OverlayMode::LayerShell | OverlayMode::FallbackWindow => {
            info!(
                overlay_mode = overlay_capability.mode.as_str(),
                overlay_reason = %overlay_capability.reason,
                "overlay capability probe completed"
            );
        }
    }

    info!(
        endpoint = %config.endpoint,
        hotkey = %config.hotkey,
        completion_sound = audio_feedback.is_enabled(),
        "Starting hotkey loop; press Right Ctrl to talk"
    );
    ensure_input_access()?;
    let injector = build_injector(&config);
    let (injector_worker, mut injection_reports) = spawn_injector_worker(Arc::clone(&injector));
    let mut overlay_router =
        OverlayRouter::new(build_runtime_overlay_sink(overlay_capability.mode));
    spawn_event_loop_lag_monitor();

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
                                                match decode_server_message(&txt) {
                                                    Ok(DecodedServerMessage::Known(message)) => {
                                                        handle_server_message(
                                                            *message,
                                                            &mut state,
                                                            &mut overlay_router,
                                                            &injector_worker,
                                                            &audio_feedback,
                                                        ).await?
                                                    }
                                                    Ok(DecodedServerMessage::UnknownType { message_type }) => {
                                                        debug!(%message_type, "ignoring unknown server message type");
                                                    }
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
                            Some(report) = injection_reports.recv() => {
                                handle_injection_report(&injector_worker, report);
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

fn handle_injection_report(worker: &InjectorWorkerHandle, report: InjectionReport) {
    worker.metrics().note_report(&report);
    match report.error {
        Some(error) => {
            warn!(
                session = %report.session_id,
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                error = %error,
                "injector worker reported failure"
            );
        }
        None => {
            info!(
                session = %report.session_id,
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                "injector worker completed job"
            );
        }
    }

    let processed = worker
        .metrics()
        .worker_success_total
        .load(Ordering::Relaxed)
        + worker
            .metrics()
            .worker_failure_total
            .load(Ordering::Relaxed);
    if processed.is_multiple_of(25) && processed > 0 {
        worker.metrics().log_summary();
        let snapshot = injector_metrics_snapshot();
        info!(
            clipboard_ready_success_total = snapshot.clipboard_ready_success_total,
            clipboard_ready_failure_total = snapshot.clipboard_ready_failure_total,
            clipboard_ready_duration_ms_total = snapshot.clipboard_ready_duration_ms_total,
            route_shortcut_success_total = snapshot.route_shortcut_success_total,
            route_shortcut_failure_total = snapshot.route_shortcut_failure_total,
            route_shortcut_duration_ms_total = snapshot.route_shortcut_duration_ms_total,
            backend_success_total = snapshot.backend_success_total,
            backend_failure_total = snapshot.backend_failure_total,
            backend_duration_ms_total = snapshot.backend_duration_ms_total,
            wl_copy_spawn_total = snapshot.wl_copy_spawn_total,
            wl_paste_spawn_total = snapshot.wl_paste_spawn_total,
            "injector stage metrics summary"
        );
    }
}

async fn handle_server_message(
    message: ServerMessage,
    state: &mut PttState,
    overlay_router: &mut OverlayRouter<impl OverlaySink>,
    injector_worker: &InjectorWorkerHandle,
    audio_feedback: &AudioFeedback,
) -> Result<()> {
    match message {
        ServerMessage::SessionStarted { session_id, .. } => {
            info!(session = %session_id, "session started ack");
            overlay_router.note_session_started(session_id);
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
            match injector_worker
                .enqueue(InjectionJob::new(session_id, text, latency_ms, audio_ms))
                .await
            {
                Ok(()) => {
                    debug!(session = %session_id, "final result queued for injector worker");
                }
                Err(EnqueueFailure::Timeout) => {
                    warn!(
                        session = %session_id,
                        queue_capacity = INJECTION_QUEUE_CAPACITY,
                        enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                        "injector queue remained full; dropping final result injection job"
                    );
                }
                Err(EnqueueFailure::WorkerGone) => {
                    warn!(
                        session = %session_id,
                        "injector worker unavailable; dropping final result injection job"
                    );
                }
            }
            state.reset();
        }
        ServerMessage::Error {
            session_id,
            code,
            message,
        } => {
            let error_kind = classify_error_code(&code);
            warn!(
                session = ?session_id,
                error_code = %code,
                error_kind,
                "daemon error: {}",
                message
            );
            state.reset();
        }
        ServerMessage::InterimState {
            session_id,
            seq,
            state: interim_state,
        } => {
            overlay_router.route_interim_state(
                session_id_from_state(state),
                session_id,
                seq,
                interim_state,
            );
        }
        ServerMessage::InterimText {
            session_id,
            seq,
            text,
        } => {
            overlay_router.route_interim_text(session_id_from_state(state), session_id, seq, text);
        }
        ServerMessage::SessionEnded { session_id, reason } => {
            overlay_router.route_session_ended(session_id_from_state(state), session_id, reason);
        }
        ServerMessage::Status { .. } => {}
    }
    Ok(())
}

fn session_id_from_state(state: &PttState) -> Option<Uuid> {
    match *state {
        PttState::Idle => None,
        PttState::Listening { session_id } | PttState::WaitingResult { session_id } => {
            Some(session_id)
        }
    }
}

fn classify_error_code(code: &str) -> &'static str {
    match code {
        "SESSION_BUSY" => "session_busy",
        "SESSION_NOT_FOUND" => "session_not_found",
        "SESSION_ABORTED" => "session_aborted",
        "AUDIO_DEVICE" => "audio_device",
        "MODEL" => "model",
        "INVALID_REQUEST" => "invalid_request",
        "UNEXPECTED" => "unexpected",
        _ => "unknown",
    }
}

#[derive(Debug, Deserialize)]
struct StatusInfo {
    state: Option<String>,
    sessions_active: Option<u32>,
    gpu_mem_mb: Option<u64>,
    device: Option<String>,
    effective_device: Option<String>,
    streaming_enabled: Option<bool>,
    stream_helper_active: Option<bool>,
    stream_fallback_reason: Option<String>,
    chunk_secs: Option<f64>,
    active_session_age_ms: Option<u64>,
    audio_stop_ms: Option<u64>,
    finalize_ms: Option<u64>,
    infer_ms: Option<u64>,
    send_ms: Option<u64>,
    last_audio_ms: Option<u64>,
    last_infer_ms: Option<u64>,
    last_send_ms: Option<u64>,
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
                    "Daemon status: state={:?}, sessions_active={:?}, device={:?}, effective_device={:?}, \
streaming={:?}, helper_active={:?}, fallback={:?}, chunk_secs={:?}, active_age_ms={:?}, \
audio_stop_ms={:?}, finalize_ms={:?}, infer_ms={:?}, send_ms={:?}, last_audio_ms={:?}, \
last_infer_ms={:?}, last_send_ms={:?}, gpu_mem_mb={:?}",
                    status.state,
                    status.sessions_active,
                    status.device,
                    status.effective_device,
                    status.streaming_enabled,
                    status.stream_helper_active,
                    status.stream_fallback_reason,
                    status.chunk_secs,
                    status.active_session_age_ms,
                    status.audio_stop_ms,
                    status.finalize_ms,
                    status.infer_ms,
                    status.send_ms,
                    status.last_audio_ms,
                    status.last_infer_ms,
                    status.last_send_ms,
                    status.gpu_mem_mb
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use std::time::Instant;

    use anyhow::Result;
    use clap::Parser;
    use tokio::sync::mpsc;
    use tokio::task::yield_now;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crate::audio_feedback::AudioFeedback;
    use crate::config::{
        ClientConfig, ClipboardOptions, InjectionConfig, InjectionMode, PasteBackendFailurePolicy,
        PasteKeyBackend,
    };
    use crate::injector::TextInjector;
    use crate::overlay_process::{OverlayProcessMetrics, OverlayProcessSink};
    use crate::protocol::ServerMessage;
    use crate::state::PttState;

    use super::{
        build_injector, handle_server_message, spawn_injector_worker_with_capacity, EnqueueFailure,
        InjectionJob, NoopOverlaySink, OverlayEvent, OverlayRouter, OverlaySink,
        RuntimeOverlaySink,
    };

    struct SlowInjector {
        calls: Arc<AtomicU64>,
        sleep_ms: u64,
    }

    impl TextInjector for SlowInjector {
        fn inject(&self, _text: &str) -> Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(self.sleep_ms));
            Ok(())
        }
    }

    struct RecordingInjector {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl TextInjector for RecordingInjector {
        fn inject(&self, text: &str) -> Result<()> {
            self.seen
                .lock()
                .expect("recording lock should be available")
                .push(text.to_string());
            Ok(())
        }
    }

    struct RecordingOverlaySink {
        seen: Arc<Mutex<Vec<OverlayEvent>>>,
    }

    impl OverlaySink for RecordingOverlaySink {
        fn on_overlay_event(&mut self, event: OverlayEvent) {
            self.seen
                .lock()
                .expect("overlay recording lock should be available")
                .push(event);
        }
    }

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

    #[tokio::test(flavor = "current_thread")]
    async fn handle_server_message_enqueues_without_waiting_for_injection_completion() {
        let calls = Arc::new(AtomicU64::new(0));
        let slow_injector = Arc::new(SlowInjector {
            calls: Arc::clone(&calls),
            sleep_ms: 120,
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(slow_injector, 8);

        let mut state = PttState::new();
        let session_id = state.begin_listening().expect("state should start");
        state.stop_listening();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink);
        let feedback = AudioFeedback::new(false, None, 100);
        let message = ServerMessage::FinalResult {
            session_id,
            text: "hello from daemon".to_string(),
            latency_ms: 60,
            audio_ms: 1900,
            lang: Some("en".to_string()),
            confidence: Some(0.99),
        };

        let started = Instant::now();
        handle_server_message(message, &mut state, &mut overlay_router, &worker, &feedback)
            .await
            .expect("server message should enqueue successfully");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "handle_server_message should not wait for blocking injection, elapsed={elapsed:?}"
        );
        assert!(matches!(state, PttState::Idle));

        let report = timeout(Duration::from_secs(2), reports.recv())
            .await
            .expect("worker should report")
            .expect("report stream should remain open");
        assert!(report.error.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn injector_worker_preserves_fifo_order() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingInjector {
            seen: Arc::clone(&seen),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);

        worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "one".to_string(), 10, 20))
            .await
            .expect("first enqueue should pass");
        worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "two".to_string(), 11, 21))
            .await
            .expect("second enqueue should pass");
        worker
            .enqueue(InjectionJob::new(
                Uuid::new_v4(),
                "three".to_string(),
                12,
                22,
            ))
            .await
            .expect("third enqueue should pass");

        for _ in 0..3 {
            let report = timeout(Duration::from_secs(1), reports.recv())
                .await
                .expect("each report should arrive")
                .expect("worker should keep report channel open");
            assert!(report.error.is_none());
        }

        let ordered = seen
            .lock()
            .expect("recording lock should be available")
            .clone();
        assert_eq!(ordered, vec!["one", "two", "three"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interim_overlay_messages_route_without_injection_enqueue() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(RecordingOverlaySink {
            seen: Arc::clone(&seen_overlay_events),
        });
        let injector_seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingInjector {
            seen: Arc::clone(&injector_seen),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 4);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        let feedback = AudioFeedback::new(false, None, 100);

        handle_server_message(
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "listening".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("interim state should route to overlay");
        handle_server_message(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "hello".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("interim text should route to overlay");
        handle_server_message(
            ServerMessage::SessionEnded {
                session_id,
                reason: Some("normal".to_string()),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("session ended should route to overlay");

        assert!(matches!(state, PttState::WaitingResult { session_id: id } if id == session_id));
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 0);
        assert_eq!(
            injector_seen
                .lock()
                .expect("recording lock should be available")
                .len(),
            0
        );

        let overlay_events = seen_overlay_events
            .lock()
            .expect("overlay recording lock should be available")
            .clone();
        assert_eq!(
            overlay_events,
            vec![
                OverlayEvent::InterimState {
                    session_id,
                    seq: 1,
                    state: "listening".to_string(),
                },
                OverlayEvent::InterimText {
                    session_id,
                    seq: 2,
                    text: "hello".to_string(),
                },
                OverlayEvent::SessionEnded {
                    session_id,
                    reason: Some("normal".to_string()),
                },
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_stream_enqueues_exactly_one_final_result() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(RecordingOverlaySink {
            seen: Arc::clone(&seen_overlay_events),
        });
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingInjector {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        let feedback = AudioFeedback::new(false, None, 100);

        handle_server_message(
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "processing".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("interim state should route");
        handle_server_message(
            ServerMessage::FinalResult {
                session_id,
                text: "only final injects".to_string(),
                latency_ms: 40,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.9),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("final result should enqueue exactly once");
        handle_server_message(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "post-final overlay".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("interim text should stay in overlay route");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should produce one report")
            .expect("report channel should remain open");
        assert!(report.error.is_none());

        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["only final injects".to_string()]
        );
        assert!(timeout(Duration::from_millis(150), reports.recv())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overlay_disconnect_does_not_block_final_result_injection() {
        let (overlay_tx, overlay_rx) = mpsc::unbounded_channel();
        drop(overlay_rx);
        let overlay_metrics = Arc::new(OverlayProcessMetrics::default());
        let sink =
            OverlayProcessSink::from_sender_for_tests(overlay_tx, Arc::clone(&overlay_metrics));
        let mut overlay_router = OverlayRouter::new(RuntimeOverlaySink::Process(sink));

        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingInjector {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        let feedback = AudioFeedback::new(false, None, 100);

        handle_server_message(
            ServerMessage::InterimText {
                session_id,
                seq: 1,
                text: "overlay event while disconnected".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("overlay disconnect should be non-fatal");
        assert_eq!(
            overlay_metrics.events_dropped_total.load(Ordering::Relaxed),
            1
        );

        handle_server_message(
            ServerMessage::FinalResult {
                session_id,
                text: "final survives overlay disconnect".to_string(),
                latency_ms: 33,
                audio_ms: 777,
                lang: Some("en".to_string()),
                confidence: Some(0.95),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("final result should still enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should report")
            .expect("report stream should remain open");
        assert!(report.error.is_none());
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["final survives overlay disconnect".to_string()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_interim_sequences_are_dropped_on_overlay_path_only() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(RecordingOverlaySink {
            seen: Arc::clone(&seen_overlay_events),
        });
        let injector = Arc::new(RecordingInjector {
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        let feedback = AudioFeedback::new(false, None, 100);

        handle_server_message(
            ServerMessage::InterimText {
                session_id,
                seq: 10,
                text: "newest".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("first interim text should route");
        handle_server_message(
            ServerMessage::InterimText {
                session_id,
                seq: 9,
                text: "stale".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("stale interim text should be dropped without failure");

        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 0);
        assert_eq!(
            overlay_router
                .metrics()
                .dropped_stale_seq_total
                .load(Ordering::Relaxed),
            1
        );
        let overlay_events = seen_overlay_events
            .lock()
            .expect("overlay recording lock should be available")
            .clone();
        assert_eq!(overlay_events.len(), 1);
        assert_eq!(
            overlay_events[0],
            OverlayEvent::InterimText {
                session_id,
                seq: 10,
                text: "newest".to_string(),
            }
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_times_out_when_queue_remains_saturated() {
        let slow = Arc::new(SlowInjector {
            calls: Arc::new(AtomicU64::new(0)),
            sleep_ms: 200,
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(slow, 1);

        worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "first".to_string(), 1, 1))
            .await
            .expect("first enqueue should pass");

        yield_now().await;

        worker
            .enqueue(InjectionJob::new(
                Uuid::new_v4(),
                "second".to_string(),
                2,
                2,
            ))
            .await
            .expect("second enqueue should fill queue");

        let third = worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "third".to_string(), 3, 3))
            .await;
        assert_eq!(third, Err(EnqueueFailure::Timeout));
    }
}
