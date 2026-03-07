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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
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
    resolve_overlay_adaptive_width, resolve_overlay_capability, ClientConfig, ClipboardOptions,
    InjectionConfig, OverlayMode, DEFAULT_ENDPOINT,
};
use crate::hotkey::{
    ensure_input_access, parse_pre_modifier_key_names, spawn_hotkey_loop, HotkeyEvent, HotkeyIntent,
};
use crate::injector::{injector_metrics_snapshot, TextInjector};
use crate::overlay_process::OverlayProcessManager;
use crate::protocol::{
    decode_server_message, start_message, stop_message, DecodedServerMessage, ServerMessage,
};
use crate::state::PttState;
use crate::surface_focus::WaylandFocusCache;
use parakeet_ptt::overlay_ipc::OverlayIpcMessage;
use parakeet_ptt::overlay_renderer::INTERNAL_OVERLAY_MODE_ARG;

const INJECTION_QUEUE_CAPACITY: usize = 32;
const INJECTION_ENQUEUE_TIMEOUT_MS: u64 = 20;
#[cfg(not(test))]
const INJECTION_EXECUTION_TIMEOUT_MS: u64 = 1_500;
#[cfg(test)]
const INJECTION_EXECUTION_TIMEOUT_MS: u64 = 150;
const EVENT_LOOP_LAG_TICK_MS: u64 = 10;
const EVENT_LOOP_LAG_LOG_INTERVAL_SECS: u64 = 30;
const HOTKEY_INTENT_DIAGNOSTIC_LOG_INTERVAL_EVENTS: u64 = 20;
const DEFAULT_LLM_PRE_MODIFIER_KEY: &str = "KEY_SHIFT";
const DEFAULT_LLM_BASE_URL: &str = "http://127.0.0.1:8080/v1";
const DEFAULT_LLM_MODEL: &str = "local";
const DEFAULT_LLM_SYSTEM_PROMPT: &str =
    "You are a concise assistant. Return only the final answer text for direct insertion.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIntent {
    Dictate,
    LlmQuery,
}

#[derive(Debug, Clone)]
struct LlmRuntimeConfig {
    base_url: url::Url,
    model: String,
    timeout: Duration,
    max_tokens: u32,
    temperature: f32,
    system_prompt: String,
    overlay_stream: bool,
}

#[derive(Debug)]
enum LlmProgress {
    Delta {
        session_id: Uuid,
        delta: String,
    },
    Finished {
        session_id: Uuid,
        transcript: String,
        daemon_latency_ms: u64,
        daemon_audio_ms: u64,
        result: std::result::Result<String, String>,
    },
}

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
    error_kind: Option<InjectionErrorKind>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectionErrorKind {
    BackendFailure,
    ExecutionTimeout,
    WorkerTaskFailed,
}

impl InjectionErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::BackendFailure => "backend_failure",
            Self::ExecutionTimeout => "execution_timeout",
            Self::WorkerTaskFailed => "worker_task_failed",
        }
    }
}

#[derive(Debug, Default)]
struct InjectorQueueMetrics {
    queued_total: AtomicU64,
    enqueue_blocked_total: AtomicU64,
    enqueue_timeout_total: AtomicU64,
    enqueue_worker_gone_total: AtomicU64,
    worker_success_total: AtomicU64,
    worker_failure_total: AtomicU64,
    worker_backend_failure_total: AtomicU64,
    worker_execution_timeout_total: AtomicU64,
    worker_task_failed_total: AtomicU64,
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

#[derive(Debug, Default)]
struct HotkeyIntentDiagnostics {
    hotkey_down_total: u64,
    hotkey_down_dictate_total: u64,
    hotkey_down_llm_query_total: u64,
    hotkey_down_ignored_total: u64,
    hotkey_up_total: u64,
    hotkey_up_ignored_total: u64,
    llm_busy_reject_total: u64,
    last_logged_hotkey_events: u64,
}

impl HotkeyIntentDiagnostics {
    fn note_hotkey_down(&mut self, intent: SessionIntent) {
        self.hotkey_down_total += 1;
        match intent {
            SessionIntent::Dictate => self.hotkey_down_dictate_total += 1,
            SessionIntent::LlmQuery => self.hotkey_down_llm_query_total += 1,
        }
    }

    fn note_hotkey_down_ignored(&mut self) {
        self.hotkey_down_ignored_total += 1;
    }

    fn note_hotkey_up(&mut self) {
        self.hotkey_up_total += 1;
    }

    fn note_hotkey_up_ignored(&mut self) {
        self.hotkey_up_ignored_total += 1;
    }

    fn note_llm_busy_reject(&mut self) {
        self.llm_busy_reject_total += 1;
    }

    fn maybe_log_summary(&mut self, reason: &'static str) {
        let hotkey_events = self.hotkey_down_total + self.hotkey_up_total;
        if hotkey_events == 0 {
            return;
        }
        if hotkey_events
            < self.last_logged_hotkey_events + HOTKEY_INTENT_DIAGNOSTIC_LOG_INTERVAL_EVENTS
        {
            return;
        }
        self.last_logged_hotkey_events = hotkey_events;
        self.log_summary(reason);
    }

    fn log_summary(&self, reason: &'static str) {
        if self.hotkey_down_total == 0 && self.hotkey_up_total == 0 {
            return;
        }
        info!(
            reason,
            hotkey_down_total = self.hotkey_down_total,
            hotkey_down_dictate_total = self.hotkey_down_dictate_total,
            hotkey_down_llm_query_total = self.hotkey_down_llm_query_total,
            hotkey_down_ignored_total = self.hotkey_down_ignored_total,
            hotkey_up_total = self.hotkey_up_total,
            hotkey_up_ignored_total = self.hotkey_up_ignored_total,
            llm_busy_reject_total = self.llm_busy_reject_total,
            "hotkey intent routing diagnostics"
        );
    }
}

#[derive(Debug, Clone, PartialEq)]
enum OverlayEvent {
    OutputHint {
        output_name: String,
    },
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
    AudioLevel {
        session_id: Uuid,
        level_db: f32,
    },
    SessionEnded {
        session_id: Uuid,
        reason: Option<String>,
    },
    InjectionComplete {
        session_id: Uuid,
        success: bool,
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
    Process(Box<OverlayProcessManager>),
}

impl OverlaySink for RuntimeOverlaySink {
    fn on_overlay_event(&mut self, event: OverlayEvent) {
        match self {
            Self::Noop(sink) => sink.on_overlay_event(event),
            Self::Process(manager) => manager.send(overlay_event_to_ipc(event)),
        }
    }
}

fn overlay_event_to_ipc(event: OverlayEvent) -> OverlayIpcMessage {
    match event {
        OverlayEvent::OutputHint { output_name } => OverlayIpcMessage::OutputHint { output_name },
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
        OverlayEvent::AudioLevel {
            session_id,
            level_db,
        } => OverlayIpcMessage::AudioLevel {
            session_id,
            level_db,
        },
        OverlayEvent::SessionEnded { session_id, reason } => {
            OverlayIpcMessage::SessionEnded { session_id, reason }
        }
        OverlayEvent::InjectionComplete {
            session_id,
            success,
        } => OverlayIpcMessage::InjectionComplete {
            session_id,
            success,
        },
    }
}

fn build_runtime_overlay_sink(
    mode: OverlayMode,
    overlay_adaptive_width: bool,
    focus_cache: Option<WaylandFocusCache>,
) -> RuntimeOverlaySink {
    match mode {
        OverlayMode::Disabled => RuntimeOverlaySink::Noop(NoopOverlaySink),
        OverlayMode::LayerShell | OverlayMode::FallbackWindow => {
            let manager = OverlayProcessManager::new(mode, overlay_adaptive_width, focus_cache);
            let metrics = manager.metrics();
            info!(
                overlay_spawn_attempt_total = metrics.spawn_attempt_total.load(Ordering::Relaxed),
                overlay_spawn_success_total = metrics.spawn_success_total.load(Ordering::Relaxed),
                overlay_spawn_failure_total = metrics.spawn_failure_total.load(Ordering::Relaxed),
                overlay_active_sink = manager.has_active_sink(),
                overlay_adaptive_width,
                "overlay process routing enabled with respawn manager"
            );
            RuntimeOverlaySink::Process(Box::new(manager))
        }
    }
}

struct OverlayRouter<S: OverlaySink> {
    sink: S,
    metrics: Arc<OverlayRoutingMetrics>,
    active_session_id: Option<Uuid>,
    last_seq: Option<u64>,
    focus_cache: Option<WaylandFocusCache>,
    last_output_name: Option<String>,
}

impl<S: OverlaySink> OverlayRouter<S> {
    fn new(sink: S, focus_cache: Option<WaylandFocusCache>) -> Self {
        Self {
            sink,
            metrics: Arc::new(OverlayRoutingMetrics::default()),
            active_session_id: None,
            last_seq: None,
            focus_cache,
            last_output_name: None,
        }
    }

    #[cfg(test)]
    fn metrics(&self) -> &Arc<OverlayRoutingMetrics> {
        &self.metrics
    }

    fn note_session_started(&mut self, session_id: Uuid) {
        if self.active_session_id != Some(session_id) {
            self.active_session_id = Some(session_id);
            self.last_seq = None;
            self.last_output_name = None;
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

        self.maybe_emit_output_hint();
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

        self.maybe_emit_output_hint();
        self.sink.on_overlay_event(OverlayEvent::InterimText {
            session_id,
            seq,
            text,
        });
        self.metrics.note_interim_text();
    }

    fn route_audio_level(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        level_db: f32,
    ) {
        if !self.allow_session(expected_session_id, session_id) {
            return;
        }

        self.sink.on_overlay_event(OverlayEvent::AudioLevel {
            session_id,
            level_db,
        });
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
            self.last_output_name = None;
        }
    }

    fn route_injection_complete(&mut self, session_id: Uuid, success: bool) {
        self.sink.on_overlay_event(OverlayEvent::InjectionComplete {
            session_id,
            success,
        });
    }

    fn maybe_emit_output_hint(&mut self) {
        let Some(focus_cache) = self.focus_cache.as_ref() else {
            return;
        };

        let Some(output_name) = focus_cache.current_output_name() else {
            return;
        };

        if self.last_output_name.as_deref() == Some(output_name.as_str()) {
            return;
        }

        self.last_output_name = Some(output_name.clone());
        self.sink
            .on_overlay_event(OverlayEvent::OutputHint { output_name });
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
        match report.error_kind {
            Some(InjectionErrorKind::BackendFailure) => {
                self.worker_failure_total.fetch_add(1, Ordering::Relaxed);
                self.worker_backend_failure_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            Some(InjectionErrorKind::ExecutionTimeout) => {
                self.worker_failure_total.fetch_add(1, Ordering::Relaxed);
                self.worker_execution_timeout_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            Some(InjectionErrorKind::WorkerTaskFailed) => {
                self.worker_failure_total.fetch_add(1, Ordering::Relaxed);
                self.worker_task_failed_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.worker_success_total.fetch_add(1, Ordering::Relaxed);
            }
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
            worker_backend_failure_total =
                self.worker_backend_failure_total.load(Ordering::Relaxed),
            worker_execution_timeout_total =
                self.worker_execution_timeout_total.load(Ordering::Relaxed),
            worker_task_failed_total = self.worker_task_failed_total.load(Ordering::Relaxed),
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
            let injection_task =
                tokio::task::spawn_blocking(move || injector_for_job.inject(&text));
            tokio::pin!(injection_task);
            let result = timeout(
                TokioDuration::from_millis(INJECTION_EXECUTION_TIMEOUT_MS),
                &mut injection_task,
            )
            .await;
            let run_ms = worker_started.elapsed().as_millis() as u64;
            let total_worker_ms = queue_wait_ms.saturating_add(run_ms);

            let (error_kind, error) = match result {
                Ok(Ok(Ok(()))) => (None, None),
                Ok(Ok(Err(err))) => (
                    Some(InjectionErrorKind::BackendFailure),
                    Some(format!("{err:#}")),
                ),
                Ok(Err(err)) => (
                    Some(InjectionErrorKind::WorkerTaskFailed),
                    Some(format!("injector worker task failed: {err}")),
                ),
                Err(_) => {
                    injection_task.as_ref().abort();
                    (
                        Some(InjectionErrorKind::ExecutionTimeout),
                        Some(format!(
                            "injector execution timed out after {INJECTION_EXECUTION_TIMEOUT_MS} ms"
                        )),
                    )
                }
            };
            let report = InjectionReport {
                session_id,
                daemon_latency_ms,
                daemon_audio_ms,
                queue_wait_ms,
                run_ms,
                total_worker_ms,
                error_kind,
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

    /// Hold-to-talk key (evdev key name, e.g. KEY_RIGHTCTRL)
    #[arg(long, default_value = "KEY_RIGHTCTRL")]
    hotkey: String,

    /// Pre-modifier key held before hotkey down to start in LLM query mode.
    #[arg(long, default_value = DEFAULT_LLM_PRE_MODIFIER_KEY)]
    llm_pre_modifier_key: String,

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

    let llm_base_url = url::Url::parse(&cli.llm_base_url)
        .with_context(|| format!("invalid LLM base URL: {}", cli.llm_base_url))?;
    let llm_config = LlmRuntimeConfig {
        base_url: llm_base_url,
        model: cli.llm_model.clone(),
        timeout: Duration::from_secs(cli.llm_timeout_seconds.max(1)),
        max_tokens: cli.llm_max_tokens.max(1),
        temperature: cli.llm_temperature.clamp(0.0, 2.0),
        system_prompt: cli.llm_system_prompt.clone(),
        overlay_stream: cli.llm_overlay_stream,
    };

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
    run_hotkey_mode(
        config,
        audio_feedback,
        cli.overlay_enabled,
        cli.overlay_adaptive_width,
        llm_config,
        cli.llm_pre_modifier_key.clone(),
    )
    .await
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

fn llm_chat_completions_url(base: &url::Url) -> Result<url::Url> {
    let mut url = base.clone();
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url.join("chat/completions")
        .context("failed to build llama chat/completions URL")
}

fn llm_health_url(base: &url::Url) -> url::Url {
    let mut url = base.clone();
    url.set_path("/health");
    url
}

fn extract_delta_content(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("choices")?
        .get(0)?
        .get("delta")?
        .get("content")?
        .as_str()
}

fn sanitize_model_answer(raw: &str) -> String {
    let mut output = raw.to_string();
    while let Some(start) = output.find("<think>") {
        let Some(end_relative) = output[start..].find("</think>") else {
            output.truncate(start);
            break;
        };
        let end = start + end_relative + "</think>".len();
        output.replace_range(start..end, "");
    }

    let trimmed = output.trim();
    trimmed.to_string()
}

fn drain_sse_lines(buffer: &mut Vec<u8>, flush_partial: bool) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    loop {
        let Some(line_end) = buffer.iter().position(|byte| *byte == b'\n') else {
            break;
        };

        let mut raw_line = buffer.drain(..=line_end).collect::<Vec<_>>();
        raw_line.pop();
        if raw_line.ends_with(b"\r") {
            raw_line.pop();
        }
        let line = std::str::from_utf8(&raw_line)
            .context("llama SSE stream contained invalid UTF-8 in a line")?;
        lines.push(line.to_string());
    }

    if flush_partial && !buffer.is_empty() {
        let line = std::str::from_utf8(buffer)
            .context("llama SSE stream ended with invalid UTF-8 in trailing bytes")?;
        lines.push(line.trim_end_matches('\r').to_string());
        buffer.clear();
    }

    Ok(lines)
}

fn maybe_defer_llm_session_end(
    message: &ServerMessage,
    state: &PttState,
    active_intent: Option<SessionIntent>,
    llm_in_flight_session: Option<Uuid>,
) -> Option<(Uuid, Option<String>)> {
    let ServerMessage::SessionEnded { session_id, reason } = message else {
        return None;
    };

    let waiting_for_llm_final = active_intent == Some(SessionIntent::LlmQuery)
        && session_id_from_state(state) == Some(*session_id);
    let llm_generation_running = llm_in_flight_session == Some(*session_id);

    if waiting_for_llm_final || llm_generation_running {
        Some((*session_id, reason.clone()))
    } else {
        None
    }
}

async fn fetch_llm_streamed_answer(
    llm: &LlmRuntimeConfig,
    session_id: Uuid,
    transcript: &str,
    progress_tx: &mpsc::UnboundedSender<LlmProgress>,
) -> Result<String> {
    let request_url = llm_chat_completions_url(&llm.base_url)?;
    let client = reqwest::Client::builder()
        .timeout(llm.timeout)
        .build()
        .context("failed to build reqwest client for llama")?;

    let request_body = json!({
        "model": llm.model,
        "stream": true,
        "messages": [
            {"role": "system", "content": llm.system_prompt},
            {"role": "user", "content": transcript},
        ],
        "max_tokens": llm.max_tokens,
        "temperature": llm.temperature,
        "chat_template_kwargs": {"enable_thinking": false},
        "reasoning_format": "none",
        "reasoning_in_content": false
    });

    let response = client
        .post(request_url.clone())
        .json(&request_body)
        .send()
        .await
        .with_context(|| format!("failed to reach llama endpoint {}", request_url))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        anyhow::bail!("llama returned status {} with body: {}", status, body);
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut assembled = String::new();

    let mut process_sse_line = |line: &str| -> Result<bool> {
        if line.is_empty() || !line.starts_with("data:") {
            return Ok(false);
        }

        let payload = line[5..].trim();
        if payload == "[DONE]" {
            return Ok(true);
        }

        let parsed: serde_json::Value = serde_json::from_str(payload).with_context(|| {
            format!("failed to parse llama SSE data payload as JSON: {payload}")
        })?;
        if let Some(delta) = extract_delta_content(&parsed).filter(|value| !value.is_empty()) {
            assembled.push_str(delta);
            if llm.overlay_stream {
                let _ = progress_tx.send(LlmProgress::Delta {
                    session_id,
                    delta: delta.to_string(),
                });
            }
        }

        Ok(false)
    };

    while let Some(next_chunk) = stream.next().await {
        let chunk = next_chunk.context("failed reading llama stream chunk")?;
        buffer.extend_from_slice(&chunk);

        for line in drain_sse_lines(&mut buffer, false)? {
            if process_sse_line(line.trim())? {
                return Ok(assembled);
            }
        }
    }

    for line in drain_sse_lines(&mut buffer, true)? {
        if process_sse_line(line.trim())? {
            return Ok(assembled);
        }
    }

    Ok(assembled)
}

async fn probe_llm_health_once(llm: &LlmRuntimeConfig) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build();
    let Ok(client) = client else {
        return false;
    };

    match client.get(llm_health_url(&llm.base_url)).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
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
                audio_feedback.play_completion();
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

async fn run_hotkey_mode(
    config: ClientConfig,
    audio_feedback: AudioFeedback,
    overlay_enabled_override: Option<bool>,
    overlay_adaptive_width_override: Option<bool>,
    llm_config: LlmRuntimeConfig,
    llm_pre_modifier_key_name: String,
) -> Result<()> {
    let talk_key = crate::hotkey::parse_key_name(&config.hotkey)
        .with_context(|| format!("invalid --hotkey value '{}'", config.hotkey))?;
    let llm_pre_modifier_keys = parse_pre_modifier_key_names(&llm_pre_modifier_key_name)
        .with_context(|| {
            format!(
                "invalid --llm-pre-modifier-key value '{}'",
                llm_pre_modifier_key_name
            )
        })?;

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

    info!(
        endpoint = %config.endpoint,
        hotkey = %config.hotkey,
        llm_pre_modifier_key = %llm_pre_modifier_key_name,
        completion_sound = audio_feedback.is_enabled(),
        "Starting hotkey loop"
    );
    ensure_input_access()?;
    let injector = build_injector(&config);
    let focus_cache = Some(WaylandFocusCache::new());
    let (injector_worker, mut injection_reports) = spawn_injector_worker(Arc::clone(&injector));
    let mut overlay_router = OverlayRouter::new(
        build_runtime_overlay_sink(
            overlay_capability.mode,
            overlay_adaptive_width,
            focus_cache.clone(),
        ),
        focus_cache,
    );
    spawn_event_loop_lag_monitor();

    let mut state = PttState::new();
    let (hk_tx, mut hk_rx) = mpsc::unbounded_channel();
    let hotkey_tasks = spawn_hotkey_loop(hk_tx, talk_key, llm_pre_modifier_keys.clone())?;
    info!(
        devices = hotkey_tasks.len(),
        talk_key = ?talk_key,
        llm_pre_modifier_keys = ?llm_pre_modifier_keys,
        "Hotkey listeners started"
    );

    let llm_health = probe_llm_health_once(&llm_config).await;
    if llm_health {
        info!(base_url = %llm_config.base_url, "llama-server health probe succeeded");
    } else {
        warn!(
            base_url = %llm_config.base_url,
            "llama-server health probe failed; LLM query mode will fall back to raw transcript on error"
        );
    }

    fetch_status_once(&config).await;

    let mut backoff = TokioDuration::from_millis(500);
    let mut llm_busy = false;
    let mut active_intent: Option<SessionIntent> = None;
    let mut llm_in_flight_session: Option<Uuid> = None;
    let mut llm_seq: HashMap<Uuid, u64> = HashMap::new();
    let mut llm_overlay_text: HashMap<Uuid, String> = HashMap::new();
    let mut llm_deferred_session_end: HashMap<Uuid, Option<String>> = HashMap::new();
    let mut llm_busy_overlay_seq: u64 = 0;
    let llm_busy_overlay_session = Uuid::nil();
    let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<LlmProgress>();
    let mut hotkey_intent_diagnostics = HotkeyIntentDiagnostics::default();

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
                                    HotkeyEvent::Down { intent } => {
                                        let intent = session_intent_from_hotkey(intent);
                                        hotkey_intent_diagnostics.note_hotkey_down(intent);
                                        if llm_busy {
                                            warn!("ignoring hotkey down while LLM response is in progress");
                                            llm_busy_overlay_seq = llm_busy_overlay_seq.saturating_add(1);
                                            overlay_router.route_interim_state(
                                                None,
                                                llm_busy_overlay_session,
                                                llm_busy_overlay_seq,
                                                "LLM busy; wait for current answer".to_string(),
                                            );
                                            overlay_router.route_session_ended(
                                                None,
                                                llm_busy_overlay_session,
                                                Some("busy".to_string()),
                                            );
                                            hotkey_intent_diagnostics.note_llm_busy_reject();
                                            hotkey_intent_diagnostics.maybe_log_summary("hotkey_down_busy");
                                            continue;
                                        }
                                        if let Some(session_id) = state.begin_listening() {
                                            active_intent = Some(intent);
                                            let message = start_message(session_id, Some("auto".to_string()));
                                            send_message(&mut ws_write, &message).await?;
                                            info!(session = %session_id, ?intent, "start_session sent (hotkey down)");
                                        } else {
                                            hotkey_intent_diagnostics.note_hotkey_down_ignored();
                                            debug!(
                                                ?state,
                                                ?active_intent,
                                                "ignoring hotkey down because client is not idle"
                                            );
                                        }
                                        hotkey_intent_diagnostics.maybe_log_summary("hotkey_down");
                                    }
                                    HotkeyEvent::Up => {
                                        hotkey_intent_diagnostics.note_hotkey_up();
                                        if let Some(session_id) = state.stop_listening() {
                                            let message = stop_message(session_id);
                                            send_message(&mut ws_write, &message).await?;
                                            info!(session = %session_id, "stop_session sent (hotkey up)");
                                        } else {
                                            hotkey_intent_diagnostics.note_hotkey_up_ignored();
                                            debug!(
                                                ?state,
                                                ?active_intent,
                                                "ignoring hotkey up because no listening session is active"
                                            );
                                        }
                                        hotkey_intent_diagnostics.maybe_log_summary("hotkey_up");
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
                                                        let message = *message;
                                                        if let Some((session_id, reason)) = maybe_defer_llm_session_end(
                                                            &message,
                                                            &state,
                                                            active_intent,
                                                            llm_in_flight_session,
                                                        ) {
                                                            llm_deferred_session_end.insert(session_id, reason);
                                                            debug!(
                                                                session = %session_id,
                                                                "deferring daemon session_ended until llm answer injection"
                                                            );
                                                            continue;
                                                        }

                                                        match message {
                                                            ServerMessage::FinalResult {
                                                                session_id,
                                                                text,
                                                                latency_ms,
                                                                audio_ms,
                                                                ..
                                                            } if active_intent == Some(SessionIntent::LlmQuery) => {
                                                                info!(
                                                                    session = %session_id,
                                                                    latency_ms,
                                                                    audio_ms,
                                                                    "final result received in llm_query mode"
                                                                );
                                                                llm_busy = true;
                                                                llm_in_flight_session = Some(session_id);
                                                                let seq = llm_seq.entry(session_id).or_insert(0);
                                                                *seq = seq.saturating_add(1);
                                                                overlay_router.route_interim_state(
                                                                    None,
                                                                    session_id,
                                                                    *seq,
                                                                    "Generating answer...".to_string(),
                                                                );
                                                                state.reset();
                                                                let llm = llm_config.clone();
                                                                let progress_tx = llm_tx.clone();
                                                                tokio::spawn(async move {
                                                                    let llm_result = fetch_llm_streamed_answer(
                                                                        &llm,
                                                                        session_id,
                                                                        &text,
                                                                        &progress_tx
                                                                    )
                                                                    .await
                                                                    .map_err(|err| format!("{err:#}"));
                                                                    let _ = progress_tx.send(LlmProgress::Finished {
                                                                        session_id,
                                                                        transcript: text,
                                                                        daemon_latency_ms: latency_ms,
                                                                        daemon_audio_ms: audio_ms,
                                                                        result: llm_result,
                                                                    });
                                                                });
                                                                active_intent = None;
                                                            }
                                                            known => {
                                                                let clear_intent = matches!(
                                                                    &known,
                                                                    ServerMessage::FinalResult { .. } | ServerMessage::Error { .. }
                                                                );
                                                                handle_server_message(
                                                                    known,
                                                                    &mut state,
                                                                    &mut overlay_router,
                                                                    &injector_worker,
                                                                    &audio_feedback,
                                                                ).await?;
                                                                if clear_intent {
                                                                    active_intent = None;
                                                                }
                                                            }
                                                        }
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
                                handle_injection_report(
                                    &injector_worker,
                                    report,
                                    &mut overlay_router,
                                    &audio_feedback,
                                );
                            }
                            Some(progress) = llm_rx.recv() => {
                                match progress {
                                    LlmProgress::Delta { session_id, delta } => {
                                        let entry = llm_overlay_text.entry(session_id).or_default();
                                        entry.push_str(&delta);
                                        let seq = llm_seq.entry(session_id).or_insert(0);
                                        *seq = seq.saturating_add(1);
                                        overlay_router.route_interim_text(None, session_id, *seq, entry.clone());
                                    }
                                    LlmProgress::Finished {
                                        session_id,
                                        transcript,
                                        daemon_latency_ms,
                                        daemon_audio_ms,
                                        result,
                                    } => {
                                        if llm_in_flight_session != Some(session_id) {
                                            warn!(
                                                session = %session_id,
                                                in_flight_session = ?llm_in_flight_session,
                                                "ignoring stale llm completion for non-active session"
                                            );
                                            llm_seq.remove(&session_id);
                                            llm_overlay_text.remove(&session_id);
                                            llm_deferred_session_end.remove(&session_id);
                                            continue;
                                        }

                                        llm_busy = false;
                                        llm_in_flight_session = None;
                                        llm_seq.remove(&session_id);
                                        llm_overlay_text.remove(&session_id);
                                        let session_end_reason =
                                            llm_deferred_session_end.remove(&session_id).flatten();
                                        overlay_router.route_session_ended(
                                            None,
                                            session_id,
                                            session_end_reason,
                                        );
                                        let fallback_transcript = transcript.clone();
                                        let response_text = match result {
                                            Ok(answer) => {
                                                let sanitized = sanitize_model_answer(&answer);
                                                info!(
                                                    session = %session_id,
                                                    answer_chars = sanitized.chars().count(),
                                                    "llm response completed"
                                                );
                                                sanitized
                                            }
                                            Err(error) => {
                                                warn!(
                                                    session = %session_id,
                                                    error = %error,
                                                    "llm generation failed; falling back to raw transcript"
                                                );
                                                fallback_transcript.clone()
                                            }
                                        };

                                        let to_inject = if response_text.trim().is_empty() {
                                            warn!(session = %session_id, "llm response empty after sanitization; falling back to transcript");
                                            fallback_transcript
                                        } else {
                                            response_text
                                        };

                                        match injector_worker
                                            .enqueue(InjectionJob::new(session_id, to_inject, daemon_latency_ms, daemon_audio_ms))
                                            .await
                                        {
                                            Ok(()) => debug!(session = %session_id, "llm final answer queued for injector worker"),
                                            Err(EnqueueFailure::Timeout) => {
                                                warn!(
                                                    session = %session_id,
                                                    queue_capacity = INJECTION_QUEUE_CAPACITY,
                                                    enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                                                    "injector queue remained full; dropping llm final answer"
                                                );
                                            }
                                            Err(EnqueueFailure::WorkerGone) => {
                                                warn!(session = %session_id, "injector worker unavailable; dropping llm final answer");
                                            }
                                        }
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
                hotkey_intent_diagnostics.log_summary("daemon_connection_drop");
                state.reset();
                active_intent = None;
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

fn handle_injection_report(
    worker: &InjectorWorkerHandle,
    report: InjectionReport,
    overlay_router: &mut OverlayRouter<impl OverlaySink>,
    audio_feedback: &AudioFeedback,
) {
    worker.metrics().note_report(&report);
    let success = report.error_kind.is_none() && report.error.is_none();
    match (report.error_kind, report.error) {
        (Some(error_kind), Some(error)) => {
            warn!(
                session = %report.session_id,
                error_kind = error_kind.as_str(),
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                error = %error,
                "injector worker reported failure"
            );
        }
        (None, None) => {
            info!(
                session = %report.session_id,
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                "injector worker completed job"
            );
            audio_feedback.play_completion();
        }
        (error_kind, error) => {
            warn!(
                session = %report.session_id,
                error_kind = error_kind.map(InjectionErrorKind::as_str),
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                error = ?error,
                "injector worker reported inconsistent error classification"
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

    overlay_router.route_injection_complete(report.session_id, success);
}

async fn handle_server_message(
    message: ServerMessage,
    state: &mut PttState,
    overlay_router: &mut OverlayRouter<impl OverlaySink>,
    injector_worker: &InjectorWorkerHandle,
    _audio_feedback: &AudioFeedback,
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
        ServerMessage::AudioLevel {
            session_id,
            level_db,
        } => {
            overlay_router.route_audio_level(session_id_from_state(state), session_id, level_db);
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

fn session_intent_from_hotkey(intent: HotkeyIntent) -> SessionIntent {
    match intent {
        HotkeyIntent::Dictate => SessionIntent::Dictate,
        HotkeyIntent::LlmQuery => SessionIntent::LlmQuery,
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
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use std::time::Instant;

    use anyhow::{anyhow, Result};
    use clap::Parser;
    use tokio::sync::mpsc;
    use tokio::task::yield_now;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crate::audio_feedback::AudioFeedback;
    use crate::config::{
        ClientConfig, ClipboardOptions, InjectionConfig, InjectionMode, OverlayMode,
        PasteBackendFailurePolicy, PasteKeyBackend,
    };
    use crate::injector::TextInjector;
    use crate::overlay_process::{
        OverlayProcessManager, OverlayProcessMetrics, OverlayProcessSink,
    };
    use crate::protocol::ServerMessage;
    use crate::state::PttState;

    use super::{
        build_injector, drain_sse_lines, handle_server_message, maybe_defer_llm_session_end,
        sanitize_model_answer, spawn_injector_worker_with_capacity, EnqueueFailure,
        HotkeyIntentDiagnostics, InjectionErrorKind, InjectionJob, NoopOverlaySink, OverlayEvent,
        OverlayRouter, OverlaySink, RuntimeOverlaySink, SessionIntent,
        INJECTION_EXECUTION_TIMEOUT_MS,
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

    struct TimeoutThenRecordingInjector {
        calls: Arc<AtomicU64>,
        seen: Arc<Mutex<Vec<String>>>,
        timeout_sleep_ms: u64,
    }

    impl TextInjector for TimeoutThenRecordingInjector {
        fn inject(&self, text: &str) -> Result<()> {
            let call_index = self.calls.fetch_add(1, Ordering::Relaxed);
            if call_index == 0 {
                std::thread::sleep(Duration::from_millis(self.timeout_sleep_ms));
                return Ok(());
            }

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

    #[test]
    fn cli_overlay_enabled_defaults_to_none() {
        let cli = super::Cli::parse_from(["parakeet-ptt"]);
        assert_eq!(cli.overlay_enabled, None);
    }

    #[test]
    fn cli_overlay_enabled_accepts_boolean_values() {
        let cli_enabled = super::Cli::parse_from(["parakeet-ptt", "--overlay-enabled", "true"]);
        assert_eq!(cli_enabled.overlay_enabled, Some(true));

        let cli_disabled = super::Cli::parse_from(["parakeet-ptt", "--overlay-enabled", "false"]);
        assert_eq!(cli_disabled.overlay_enabled, Some(false));
    }

    #[test]
    fn cli_overlay_adaptive_width_defaults_to_none() {
        let cli = super::Cli::parse_from(["parakeet-ptt"]);
        assert_eq!(cli.overlay_adaptive_width, None);
    }

    #[test]
    fn cli_overlay_adaptive_width_accepts_boolean_values() {
        let cli_enabled =
            super::Cli::parse_from(["parakeet-ptt", "--overlay-adaptive-width", "true"]);
        assert_eq!(cli_enabled.overlay_adaptive_width, Some(true));

        let cli_disabled =
            super::Cli::parse_from(["parakeet-ptt", "--overlay-adaptive-width", "false"]);
        assert_eq!(cli_disabled.overlay_adaptive_width, Some(false));
    }

    #[test]
    fn cli_llm_pre_modifier_and_llm_defaults_are_set() {
        let cli = super::Cli::parse_from(["parakeet-ptt"]);
        assert_eq!(
            cli.llm_pre_modifier_key,
            super::DEFAULT_LLM_PRE_MODIFIER_KEY
        );
        assert_eq!(cli.llm_base_url, super::DEFAULT_LLM_BASE_URL);
        assert_eq!(cli.llm_model, super::DEFAULT_LLM_MODEL);
        assert!(cli.llm_overlay_stream);
    }

    #[test]
    fn hotkey_intent_diagnostics_tracks_intent_split_and_ignored_paths() {
        let mut diagnostics = HotkeyIntentDiagnostics::default();
        diagnostics.note_hotkey_down(SessionIntent::Dictate);
        diagnostics.note_hotkey_down(SessionIntent::LlmQuery);
        diagnostics.note_hotkey_down_ignored();
        diagnostics.note_hotkey_up();
        diagnostics.note_hotkey_up_ignored();

        assert_eq!(diagnostics.hotkey_down_total, 2);
        assert_eq!(diagnostics.hotkey_down_dictate_total, 1);
        assert_eq!(diagnostics.hotkey_down_llm_query_total, 1);
        assert_eq!(diagnostics.hotkey_down_ignored_total, 1);
        assert_eq!(diagnostics.hotkey_up_total, 1);
        assert_eq!(diagnostics.hotkey_up_ignored_total, 1);
    }

    #[test]
    fn hotkey_intent_diagnostics_tracks_llm_busy_rejections() {
        let mut diagnostics = HotkeyIntentDiagnostics::default();
        diagnostics.note_llm_busy_reject();
        diagnostics.note_llm_busy_reject();

        assert_eq!(diagnostics.llm_busy_reject_total, 2);
    }

    #[test]
    fn sanitize_model_answer_strips_think_blocks_without_raw_fallback() {
        assert_eq!(sanitize_model_answer("<think>hidden</think>"), "");
        assert_eq!(sanitize_model_answer("<think>hidden"), "");
        assert_eq!(
            sanitize_model_answer("<think>hidden</think> visible"),
            "visible"
        );
    }

    #[test]
    fn drain_sse_lines_handles_utf8_split_across_chunks() {
        let mut buffer = Vec::<u8>::new();

        let first = b"data: {\"choices\":[{\"delta\":{\"content\":\"caf";
        let second = b"\xC3\xA9\"}}]}\n";

        buffer.extend_from_slice(first);
        let first_lines = drain_sse_lines(&mut buffer, false).expect("first parse should succeed");
        assert!(first_lines.is_empty());

        buffer.extend_from_slice(second);
        let lines = drain_sse_lines(&mut buffer, false).expect("second parse should succeed");
        assert_eq!(
            lines,
            vec!["data: {\"choices\":[{\"delta\":{\"content\":\"café\"}}]}"]
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn maybe_defer_llm_session_end_for_query_or_inflight() {
        let session_id = Uuid::new_v4();
        let state = PttState::Listening { session_id };
        let message = ServerMessage::SessionEnded {
            session_id,
            reason: Some("normal".to_string()),
        };

        let deferred_for_query =
            maybe_defer_llm_session_end(&message, &state, Some(SessionIntent::LlmQuery), None);
        assert_eq!(
            deferred_for_query,
            Some((session_id, Some("normal".to_string())))
        );

        let deferred_for_inflight =
            maybe_defer_llm_session_end(&message, &PttState::Idle, None, Some(session_id));
        assert_eq!(
            deferred_for_inflight,
            Some((session_id, Some("normal".to_string())))
        );

        let not_deferred =
            maybe_defer_llm_session_end(&message, &state, Some(SessionIntent::Dictate), None);
        assert_eq!(not_deferred, None);
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
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);
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
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
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
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
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
        let first_sink = OverlayProcessSink::from_sender_for_tests(
            overlay_tx,
            Arc::new(OverlayProcessMetrics::default()),
        );
        let sink_slot = Arc::new(Mutex::new(Some(first_sink)));
        let launcher = {
            let sink_slot = Arc::clone(&sink_slot);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                sink_slot
                    .lock()
                    .expect("sink slot lock should be available")
                    .take()
                    .ok_or_else(|| anyhow!("no overlay sink available"))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

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
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed),
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
    async fn repeated_overlay_failures_remain_non_fatal_to_final_injection() {
        fn disconnected_test_sink() -> OverlayProcessSink {
            let (overlay_tx, overlay_rx) = mpsc::unbounded_channel();
            drop(overlay_rx);
            OverlayProcessSink::from_sender_for_tests(
                overlay_tx,
                Arc::new(OverlayProcessMetrics::default()),
            )
        }

        let spawn_queue = Arc::new(Mutex::new(VecDeque::from([
            Ok(disconnected_test_sink()),
            Err(anyhow!(
                "failed to spawn overlay process '/tmp/parakeet-overlay': No such file or directory"
            )),
            Ok(disconnected_test_sink()),
            Err(anyhow!(
                "failed to spawn overlay process '/tmp/parakeet-overlay': No such file or directory"
            )),
            Ok(disconnected_test_sink()),
        ])));
        let launcher = {
            let spawn_queue = Arc::clone(&spawn_queue);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                spawn_queue
                    .lock()
                    .expect("spawn queue lock should be available")
                    .pop_front()
                    .unwrap_or_else(|| Err(anyhow!("no overlay sink available")))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

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

        for seq in 1..=4 {
            handle_server_message(
                ServerMessage::InterimText {
                    session_id,
                    seq,
                    text: format!("overlay seq {seq}"),
                },
                &mut state,
                &mut overlay_router,
                &worker,
                &feedback,
            )
            .await
            .expect("overlay failures should remain non-fatal");
        }

        handle_server_message(
            ServerMessage::FinalResult {
                session_id,
                text: "final survives repeated overlay failures".to_string(),
                latency_ms: 12,
                audio_ms: 345,
                lang: Some("en".to_string()),
                confidence: Some(0.99),
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
            vec!["final survives repeated overlay failures".to_string()]
        );
        assert!(
            manager_metrics.spawn_failure_total.load(Ordering::Relaxed) >= 1,
            "at least one spawn failure should be recorded"
        );
        assert!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed)
                >= 1,
            "at least one disconnect should be recorded"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overlay_crash_restart_replays_current_state_and_preserves_final_injection() {
        let (tx_first, mut rx_first) = mpsc::unbounded_channel();
        let first_sink = OverlayProcessSink::from_sender_for_tests(
            tx_first,
            Arc::new(OverlayProcessMetrics::default()),
        );
        let (tx_second, mut rx_second) = mpsc::unbounded_channel();
        let second_sink = OverlayProcessSink::from_sender_for_tests(
            tx_second,
            Arc::new(OverlayProcessMetrics::default()),
        );

        let spawn_queue = Arc::new(Mutex::new(VecDeque::from([
            Ok(first_sink),
            Ok(second_sink),
        ])));
        let launcher = {
            let spawn_queue = Arc::clone(&spawn_queue);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                spawn_queue
                    .lock()
                    .expect("spawn queue lock should be available")
                    .pop_front()
                    .unwrap_or_else(|| Err(anyhow!("no overlay sink available")))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

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
                text: "old-state".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("first interim text should route");
        let first_seen = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive old state")
            .expect("first sink channel should stay open");
        assert_eq!(
            first_seen,
            parakeet_ptt::overlay_ipc::OverlayIpcMessage::InterimText {
                session_id,
                seq: 1,
                text: "old-state".to_string(),
            }
        );

        drop(rx_first);

        handle_server_message(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "current-state".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &feedback,
        )
        .await
        .expect("interim text after crash should remain non-fatal");

        let second_seen = timeout(Duration::from_millis(100), rx_second.recv())
            .await
            .expect("second sink should receive replayed current state")
            .expect("second sink channel should stay open");
        assert_eq!(
            second_seen,
            parakeet_ptt::overlay_ipc::OverlayIpcMessage::InterimText {
                session_id,
                seq: 2,
                text: "current-state".to_string(),
            }
        );
        assert!(timeout(Duration::from_millis(50), rx_second.recv())
            .await
            .is_err());

        handle_server_message(
            ServerMessage::FinalResult {
                session_id,
                text: "final after overlay restart".to_string(),
                latency_ms: 45,
                audio_ms: 1000,
                lang: Some("en".to_string()),
                confidence: Some(0.98),
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
            .expect("final result should produce one report")
            .expect("report channel should remain open");
        assert!(report.error.is_none());
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["final after overlay restart".to_string()]
        );
        assert_eq!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(manager_metrics.replay_sent_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            manager_metrics.spawn_success_total.load(Ordering::Relaxed),
            2
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_interim_sequences_are_dropped_on_overlay_path_only() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
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
    async fn injector_worker_recovers_after_execution_timeout() {
        let calls = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(TimeoutThenRecordingInjector {
            calls: Arc::clone(&calls),
            seen: Arc::clone(&seen),
            timeout_sleep_ms: INJECTION_EXECUTION_TIMEOUT_MS + 75,
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);

        let first_session = Uuid::new_v4();
        let second_session = Uuid::new_v4();
        worker
            .enqueue(InjectionJob::new(
                first_session,
                "first wedges".to_string(),
                1,
                1,
            ))
            .await
            .expect("first enqueue should pass");
        worker
            .enqueue(InjectionJob::new(
                second_session,
                "second still works".to_string(),
                2,
                2,
            ))
            .await
            .expect("second enqueue should pass");

        let first_report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("first report should arrive")
            .expect("report stream should remain open");
        assert_eq!(first_report.session_id, first_session);
        assert_eq!(
            first_report.error_kind,
            Some(InjectionErrorKind::ExecutionTimeout)
        );
        assert!(
            first_report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("timed out")),
            "timeout report should explain the failure"
        );
        worker.metrics().note_report(&first_report);

        let second_report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("second report should arrive")
            .expect("report stream should remain open");
        assert_eq!(second_report.session_id, second_session);
        assert!(second_report.error.is_none());
        assert_eq!(second_report.error_kind, None);
        worker.metrics().note_report(&second_report);
        assert_eq!(
            seen.lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["second still works".to_string()]
        );
        assert_eq!(
            worker
                .metrics()
                .worker_execution_timeout_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            worker
                .metrics()
                .worker_backend_failure_total
                .load(Ordering::Relaxed),
            0
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
