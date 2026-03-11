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

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
#[cfg(test)]
use std::io::Write;
#[cfg(all(test, unix))]
use std::os::fd::AsRawFd;
#[cfg(all(test, unix))]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
#[cfg(test)]
use std::process::{Child, ExitStatus};
#[cfg(test)]
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
use crate::injector::{
    injector_metrics_snapshot, BackendAttemptReport, ClipboardInjector, FailInjector,
    InjectorChildReport, InjectorContext, ParentFocusCapture, PasteChordSender, PasteKeySender,
    TextInjector, UinputAttemptMetadata, UinputChordSender, INJECTOR_CONTEXT_ENV,
    INJECTOR_REPORT_PREFIX,
};
#[cfg(test)]
use crate::injector::{
    INJECTOR_JOB_TIMEOUT_MS, INJECTOR_PIPE_DRAIN_TIMEOUT_MS, INJECTOR_PIPE_READER_JOIN_SLACK_MS,
    INJECTOR_SUBPROCESS_POLL_INTERVAL_MS,
};
use crate::overlay_process::OverlayProcessManager;
use crate::protocol::{
    decode_server_message, start_message, stop_message, DecodedServerMessage, ServerMessage,
};
use crate::state::PttState;
use crate::surface_focus::{WaylandFocusCache, WaylandFocusObservation};
use parakeet_ptt::overlay_ipc::OverlayIpcMessage;
use parakeet_ptt::overlay_renderer::INTERNAL_OVERLAY_MODE_ARG;

const INJECTION_QUEUE_CAPACITY: usize = 32;
const INJECTION_ENQUEUE_TIMEOUT_MS: u64 = 20;
const INJECTOR_SUBPROCESS_STDERR_LOG_LINE_LIMIT: usize = 120;
const EVENT_LOOP_LAG_TICK_MS: u64 = 10;
const EVENT_LOOP_LAG_LOG_INTERVAL_SECS: u64 = 30;
const HOTKEY_INTENT_DIAGNOSTIC_LOG_INTERVAL_EVENTS: u64 = 20;
const IN_PROCESS_UINPUT_WARMUP_MS: u64 = 200;
const IN_PROCESS_UINPUT_RETRY_BACKOFF_MS: u64 = 500;
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
    origin: InjectionOrigin,
    hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
    stop_message_elapsed_ms_at_enqueue: Option<u64>,
    parent_focus: Option<ParentFocusCapture>,
    enqueued_at: TokioInstant,
}

impl InjectionJob {
    fn new(session_id: Uuid, text: String, daemon_latency_ms: u64, daemon_audio_ms: u64) -> Self {
        Self {
            session_id,
            text,
            daemon_latency_ms,
            daemon_audio_ms,
            origin: InjectionOrigin::Unspecified,
            hotkey_up_elapsed_ms_at_enqueue: None,
            stop_message_elapsed_ms_at_enqueue: None,
            parent_focus: None,
            enqueued_at: TokioInstant::now(),
        }
    }

    fn with_origin(mut self, origin: InjectionOrigin) -> Self {
        self.origin = origin;
        self
    }

    fn with_enqueue_timing(
        mut self,
        hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
        stop_message_elapsed_ms_at_enqueue: Option<u64>,
    ) -> Self {
        self.hotkey_up_elapsed_ms_at_enqueue = hotkey_up_elapsed_ms_at_enqueue;
        self.stop_message_elapsed_ms_at_enqueue = stop_message_elapsed_ms_at_enqueue;
        self
    }

    fn with_parent_focus(mut self, parent_focus: Option<ParentFocusCapture>) -> Self {
        self.parent_focus = parent_focus;
        self
    }
}

#[derive(Debug)]
struct InjectionReport {
    session_id: Uuid,
    daemon_latency_ms: u64,
    daemon_audio_ms: u64,
    origin: InjectionOrigin,
    queue_wait_ms: u64,
    run_ms: u64,
    total_worker_ms: u64,
    hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
    stop_message_elapsed_ms_at_enqueue: Option<u64>,
    hotkey_up_elapsed_ms_at_worker_start: Option<u64>,
    stop_message_elapsed_ms_at_worker_start: Option<u64>,
    error_kind: Option<InjectionErrorKind>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectionOrigin {
    RawFinalResult,
    LlmAnswer,
    Demo,
    Unspecified,
}

#[derive(Debug, Clone)]
struct CapturedParentFocus {
    focus: ParentFocusCapture,
    captured_at: TokioInstant,
}

impl InjectionOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::RawFinalResult => "raw_final_result",
            Self::LlmAnswer => "llm_answer",
            Self::Demo => "demo",
            Self::Unspecified => "unspecified",
        }
    }
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
enum InjectionRunError {
    BackendFailure(String),
    ExecutionTimeout(String),
    WorkerTaskFailed(String),
}

#[derive(Debug, Default)]
struct InjectionRunOutput {
    stderr: Vec<u8>,
}

trait InjectionJobRunner: Send + Sync {
    fn run(&self, job: &InjectionJob)
        -> std::result::Result<InjectionRunOutput, InjectionRunError>;
}

type InProcessInjectorBuilder = dyn Fn(&ClientConfig, PasteKeySender, Option<WaylandFocusCache>) -> Arc<dyn TextInjector>
    + Send
    + Sync;
type InProcessPasteSenderFactory =
    dyn Fn(&ClientConfig) -> Result<Arc<dyn PasteChordSender>> + Send + Sync;
type InProcessSleepFn = dyn Fn(Duration) + Send + Sync;

#[derive(Debug)]
struct HealthyUinputSender {
    sender: Arc<dyn PasteChordSender>,
    generation: u64,
    created_at: Instant,
    use_count: u64,
    fresh_pending: bool,
    recovered_after_failure: bool,
}

#[derive(Debug)]
struct FailedUinputSender {
    last_error: String,
    retry_after: Instant,
}

#[derive(Debug)]
enum UinputSenderState {
    Uninitialized,
    Healthy(HealthyUinputSender),
    CreateFailed(FailedUinputSender),
}

#[derive(Debug)]
struct UinputSenderManager {
    state: UinputSenderState,
    next_generation: u64,
}

impl Default for UinputSenderManager {
    fn default() -> Self {
        Self {
            state: UinputSenderState::Uninitialized,
            next_generation: 1,
        }
    }
}

#[derive(Clone)]
struct InProcessInjectorRunner {
    config: ClientConfig,
    sender_manager: Arc<Mutex<UinputSenderManager>>,
    focus_cache: Option<WaylandFocusCache>,
    injector_builder: Arc<InProcessInjectorBuilder>,
    sender_factory: Arc<InProcessPasteSenderFactory>,
    sleep_fn: Arc<InProcessSleepFn>,
    uinput_warmup: Duration,
    uinput_retry_backoff: Duration,
}

impl InProcessInjectorRunner {
    fn new(config: &ClientConfig) -> Self {
        let focus_cache = Some(WaylandFocusCache::new());
        Self {
            config: config.clone(),
            sender_manager: Arc::new(Mutex::new(UinputSenderManager::default())),
            focus_cache,
            injector_builder: Arc::new(|config, sender, focus_cache| {
                build_clipboard_injector_with_sender(
                    config,
                    sender,
                    matches!(
                        config.injection_mode,
                        crate::config::InjectionMode::CopyOnly
                    ),
                    None,
                    None,
                    focus_cache,
                )
            }),
            sender_factory: Arc::new(build_uinput_chord_sender),
            sleep_fn: Arc::new(std::thread::sleep),
            uinput_warmup: Duration::from_millis(IN_PROCESS_UINPUT_WARMUP_MS),
            uinput_retry_backoff: Duration::from_millis(IN_PROCESS_UINPUT_RETRY_BACKOFF_MS),
        }
    }

    #[cfg(test)]
    fn new_for_tests(
        config: &ClientConfig,
        injector_builder: Arc<InProcessInjectorBuilder>,
        sender_factory: Arc<InProcessPasteSenderFactory>,
        sleep_fn: Arc<InProcessSleepFn>,
        uinput_warmup: Duration,
        uinput_retry_backoff: Duration,
        focus_cache: Option<WaylandFocusCache>,
    ) -> Self {
        Self {
            config: config.clone(),
            sender_manager: Arc::new(Mutex::new(UinputSenderManager::default())),
            focus_cache,
            injector_builder,
            sender_factory,
            sleep_fn,
            uinput_warmup,
            uinput_retry_backoff,
        }
    }

    fn prepare_paste_key_sender(&self) -> std::result::Result<PasteKeySender, String> {
        use crate::config::InjectionMode;

        if matches!(self.config.injection_mode, InjectionMode::CopyOnly) {
            return Ok(PasteKeySender::Disabled);
        }

        let mut created_this_job = false;
        let mut create_elapsed_ms = None;
        let mut recovered_after_failure = false;

        loop {
            enum Action {
                Sleep(Duration),
                Create,
            }

            let action = {
                let mut manager = self
                    .sender_manager
                    .lock()
                    .map_err(|_| "uinput sender manager lock poisoned".to_string())?;
                match &mut manager.state {
                    UinputSenderState::Healthy(healthy) => {
                        let age = healthy.created_at.elapsed();
                        if healthy.fresh_pending && age < self.uinput_warmup {
                            Action::Sleep(self.uinput_warmup - age)
                        } else {
                            let metadata = UinputAttemptMetadata {
                                generation: healthy.generation,
                                fresh_device: healthy.fresh_pending,
                                device_age_ms_at_attempt: age.as_millis() as u64,
                                use_count_before_attempt: healthy.use_count,
                                created_this_job,
                                create_elapsed_ms,
                                last_create_error: None,
                                reused_after_failure: healthy.recovered_after_failure,
                            };
                            return Ok(PasteKeySender::Uinput {
                                sender: Arc::clone(&healthy.sender),
                                metadata: Some(metadata),
                            });
                        }
                    }
                    UinputSenderState::Uninitialized => Action::Create,
                    UinputSenderState::CreateFailed(failed) => {
                        if Instant::now() < failed.retry_after {
                            return Err(failed.last_error.clone());
                        }
                        recovered_after_failure = true;
                        Action::Create
                    }
                }
            };

            match action {
                Action::Sleep(duration) => (self.sleep_fn)(duration),
                Action::Create => {
                    let started = Instant::now();
                    match (self.sender_factory)(&self.config) {
                        Ok(sender) => {
                            let mut manager = self
                                .sender_manager
                                .lock()
                                .map_err(|_| "uinput sender manager lock poisoned".to_string())?;
                            let generation = manager.next_generation;
                            manager.next_generation = manager.next_generation.saturating_add(1);
                            manager.state = UinputSenderState::Healthy(HealthyUinputSender {
                                sender,
                                generation,
                                created_at: Instant::now(),
                                use_count: 0,
                                fresh_pending: true,
                                recovered_after_failure,
                            });
                            created_this_job = true;
                            create_elapsed_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        Err(err) => {
                            let message = format!(
                                "paste_key_backend=uinput could not initialize /dev/uinput: {}",
                                err
                            );
                            let mut manager = self
                                .sender_manager
                                .lock()
                                .map_err(|_| "uinput sender manager lock poisoned".to_string())?;
                            manager.state = UinputSenderState::CreateFailed(FailedUinputSender {
                                last_error: message.clone(),
                                retry_after: Instant::now() + self.uinput_retry_backoff,
                            });
                            return Err(message);
                        }
                    }
                }
            }
        }
    }

    fn commit_successful_paste_key_sender(&self, sender: &PasteKeySender) {
        let PasteKeySender::Uinput {
            metadata: Some(metadata),
            ..
        } = sender
        else {
            return;
        };

        if let Ok(mut manager) = self.sender_manager.lock() {
            if let UinputSenderState::Healthy(healthy) = &mut manager.state {
                if healthy.generation == metadata.generation
                    && healthy.use_count == metadata.use_count_before_attempt
                {
                    healthy.fresh_pending = false;
                    healthy.use_count = healthy.use_count.saturating_add(1);
                }
            }
        }
    }
}

impl InjectionJobRunner for InProcessInjectorRunner {
    fn run(
        &self,
        job: &InjectionJob,
    ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
        let context = InjectorContext {
            session_id: job.session_id,
            origin: job.origin.as_str().to_string(),
            hotkey_up_elapsed_ms_at_enqueue: job.hotkey_up_elapsed_ms_at_enqueue,
            stop_message_elapsed_ms_at_enqueue: job.stop_message_elapsed_ms_at_enqueue,
            parent_focus: job.parent_focus.clone(),
        };
        let (injector, prepared_sender) = match self.prepare_paste_key_sender() {
            Ok(sender) => {
                let injector =
                    (self.injector_builder)(&self.config, sender.clone(), self.focus_cache.clone());
                (injector, Some(sender))
            }
            Err(reason) => (
                build_backend_failure_fallback_injector(
                    &self.config,
                    reason,
                    None,
                    self.focus_cache.clone(),
                ),
                None,
            ),
        };
        if let Err(err) = injector.inject_with_context(&job.text, Some(context)) {
            let err_text = format!("{err:#}");
            if err_text.contains("stage=backend")
                && matches!(
                    self.config.injection_mode,
                    crate::config::InjectionMode::Paste
                )
            {
                if let Ok(mut manager) = self.sender_manager.lock() {
                    if matches!(manager.state, UinputSenderState::Healthy(_)) {
                        manager.state = UinputSenderState::Uninitialized;
                    }
                }
            }
            return Err(InjectionRunError::BackendFailure(err_text));
        }
        if let Some(sender) = prepared_sender.as_ref() {
            self.commit_successful_paste_key_sender(sender);
        }
        Ok(InjectionRunOutput::default())
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct InjectorSubprocessRunner {
    executable: PathBuf,
    base_args: Vec<std::ffi::OsString>,
    timeout: Duration,
}

#[cfg(test)]
impl InjectorSubprocessRunner {
    fn new_for_tests(
        executable: PathBuf,
        base_args: Vec<std::ffi::OsString>,
        timeout: Duration,
    ) -> Self {
        Self {
            executable,
            base_args,
            timeout,
        }
    }
}

#[cfg(test)]
impl InjectionJobRunner for InjectorSubprocessRunner {
    fn run(
        &self,
        job: &InjectionJob,
    ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
        let mut command = Command::new(&self.executable);
        command
            .args(&self.base_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let context = InjectorContext {
            session_id: job.session_id,
            origin: job.origin.as_str().to_string(),
            hotkey_up_elapsed_ms_at_enqueue: job.hotkey_up_elapsed_ms_at_enqueue,
            stop_message_elapsed_ms_at_enqueue: job.stop_message_elapsed_ms_at_enqueue,
            parent_focus: job.parent_focus.clone(),
        };
        let context_json = serde_json::to_string(&context).map_err(|err| {
            InjectionRunError::WorkerTaskFailed(format!(
                "failed to serialize injector subprocess context: {err}"
            ))
        })?;
        command.env(INJECTOR_CONTEXT_ENV, context_json);
        configure_injector_subprocess(&mut command);
        let mut child = command.spawn().map_err(|err| {
            InjectionRunError::WorkerTaskFailed(format!(
                "failed to spawn injector subprocess '{}': {err}",
                self.executable.display()
            ))
        })?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(job.text.as_bytes()).map_err(|err| {
                InjectionRunError::WorkerTaskFailed(format!(
                    "failed to write injector subprocess stdin: {err}"
                ))
            })?;
        }
        let _ = child.stdin.take();

        // Drain stderr concurrently so the child cannot block on a full pipe before exit.
        let stderr_reader = child
            .stderr
            .take()
            .map(|stderr| {
                spawn_pipe_reader(
                    stderr,
                    Duration::from_millis(INJECTOR_PIPE_DRAIN_TIMEOUT_MS),
                )
            })
            .ok_or_else(|| {
                InjectionRunError::WorkerTaskFailed(
                    "injector subprocess stderr pipe unavailable".to_string(),
                )
            })?;
        let status_result = wait_for_child_exit(&mut child, self.timeout);
        stderr_reader.start_deadline();
        let stderr = collect_pipe_reader(
            stderr_reader,
            "stderr",
            Duration::from_millis(
                INJECTOR_PIPE_DRAIN_TIMEOUT_MS + INJECTOR_PIPE_READER_JOIN_SLACK_MS,
            ),
        );

        match status_result {
            Ok(status) => {
                let stderr = match stderr {
                    Ok(outcome) => {
                        if outcome.timed_out {
                            debug!(
                                timeout_ms = INJECTOR_PIPE_DRAIN_TIMEOUT_MS,
                                drained_bytes = outcome.bytes.len(),
                                "injector subprocess stderr post-exit drain hit deadline; continuing with partial output"
                            );
                        }
                        outcome.bytes
                    }
                    Err(err) => return Err(InjectionRunError::WorkerTaskFailed(err)),
                };
                if status.success() {
                    Ok(InjectionRunOutput { stderr })
                } else {
                    let detail = format_child_failure(status, &stderr);
                    Err(InjectionRunError::BackendFailure(detail))
                }
            }
            Err(err) => match stderr {
                Ok(outcome) => {
                    if outcome.timed_out {
                        debug!(
                                timeout_ms = INJECTOR_PIPE_DRAIN_TIMEOUT_MS,
                                drained_bytes = outcome.bytes.len(),
                                "injector subprocess stderr post-exit drain hit deadline after failure; returning base error with partial stderr"
                            );
                    }
                    Err(enrich_run_error_with_stderr(err, &outcome.bytes))
                }
                Err(read_err) => Err(InjectionRunError::WorkerTaskFailed(read_err)),
            },
        }
    }
}

fn build_injection_runner(config: &ClientConfig) -> Arc<dyn InjectionJobRunner> {
    Arc::new(InProcessInjectorRunner::new(config))
}

#[cfg(test)]
#[derive(Debug)]
struct PipeReadOutcome {
    bytes: Vec<u8>,
    timed_out: bool,
}

#[cfg(test)]
struct PipeReaderHandle {
    receiver: std::sync::mpsc::Receiver<std::io::Result<PipeReadOutcome>>,
    deadline_started: Arc<AtomicBool>,
}

#[cfg(test)]
impl PipeReaderHandle {
    fn start_deadline(&self) {
        self.deadline_started.store(true, Ordering::Release);
    }
}

#[cfg(test)]
fn spawn_pipe_reader<R>(reader: R, timeout: Duration) -> PipeReaderHandle
where
    R: Read + Send + AsRawFd + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let deadline_started = Arc::new(AtomicBool::new(false));
    let thread_deadline_started = Arc::clone(&deadline_started);
    std::thread::spawn(move || {
        let result = read_pipe_until_deadline(reader, timeout, thread_deadline_started);
        let _ = tx.send(result);
    });
    PipeReaderHandle {
        receiver: rx,
        deadline_started,
    }
}

#[cfg(test)]
fn read_pipe_until_deadline<R>(
    mut reader: R,
    timeout: Duration,
    deadline_started: Arc<AtomicBool>,
) -> std::io::Result<PipeReadOutcome>
where
    R: Read + AsRawFd,
{
    set_pipe_nonblocking(reader.as_raw_fd())?;
    let mut deadline_started_at = None;
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];

    loop {
        if deadline_started.load(Ordering::Acquire) {
            let started = deadline_started_at.get_or_insert_with(std::time::Instant::now);
            if started.elapsed() >= timeout {
                return Ok(PipeReadOutcome {
                    bytes: buffer,
                    timed_out: true,
                });
            }
        }
        match reader.read(&mut chunk) {
            Ok(0) => {
                return Ok(PipeReadOutcome {
                    bytes: buffer,
                    timed_out: false,
                });
            }
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if deadline_started.load(Ordering::Acquire) {
                    let started = deadline_started_at.get_or_insert_with(std::time::Instant::now);
                    let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                        return Ok(PipeReadOutcome {
                            bytes: buffer,
                            timed_out: true,
                        });
                    };
                    if !wait_for_pipe_read_ready(reader.as_raw_fd(), remaining)? {
                        return Ok(PipeReadOutcome {
                            bytes: buffer,
                            timed_out: true,
                        });
                    }
                } else if !wait_for_pipe_read_ready(
                    reader.as_raw_fd(),
                    Duration::from_millis(INJECTOR_SUBPROCESS_POLL_INTERVAL_MS),
                )? {
                    continue;
                }
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
fn wait_for_pipe_read_ready(fd: std::os::fd::RawFd, timeout: Duration) -> std::io::Result<bool> {
    let timeout_ms = timeout
        .as_millis()
        .min(i32::MAX as u128)
        .try_into()
        .unwrap_or(i32::MAX);
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
        revents: 0,
    };

    loop {
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready > 0 {
            if poll_fd.revents & libc::POLLNVAL != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pipe fd became invalid while draining output",
                ));
            }
            return Ok(true);
        }
        if ready == 0 {
            return Ok(false);
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

#[cfg(test)]
fn set_pipe_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let current_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if current_flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if current_flags & libc::O_NONBLOCK != 0 {
        return Ok(());
    }

    let updated_flags = unsafe { libc::fcntl(fd, libc::F_SETFL, current_flags | libc::O_NONBLOCK) };
    if updated_flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
fn collect_pipe_reader(
    handle: PipeReaderHandle,
    label: &str,
    timeout: Duration,
) -> std::result::Result<PipeReadOutcome, String> {
    // The read thread owns the real drain deadline. This outer wait only tolerates
    // scheduler jitter while proving that the thread actually returned.
    match handle.receiver.recv_timeout(timeout) {
        Ok(read_result) => read_result.map_err(|err| format!("failed to read {label} pipe: {err}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "{label} reader thread exceeded the drain deadline without returning"
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(format!(
            "{label} reader thread disconnected before returning output"
        )),
    }
}

#[cfg(test)]
fn wait_for_child_exit(
    child: &mut Child,
    timeout: Duration,
) -> std::result::Result<ExitStatus, InjectionRunError> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() >= timeout => {
                kill_injector_subprocess(child);
                if let Err(err) = child.wait() {
                    warn!(error = %err, "failed to reap timed-out injector subprocess");
                }
                return Err(InjectionRunError::ExecutionTimeout(format!(
                    "injector execution timed out after {} ms",
                    timeout.as_millis()
                )));
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(INJECTOR_SUBPROCESS_POLL_INTERVAL_MS))
            }
            Err(err) => {
                return Err(InjectionRunError::WorkerTaskFailed(format!(
                    "failed to query injector subprocess state: {err}"
                )));
            }
        }
    }
}

#[cfg(test)]
fn configure_injector_subprocess(command: &mut Command) {
    #[cfg(unix)]
    {
        // Give each injection job its own process group so timeout can kill the
        // entire subprocess tree, not just the wrapper process.
        command.process_group(0);
    }
}

#[cfg(test)]
fn kill_injector_subprocess(child: &mut Child) {
    #[cfg(unix)]
    {
        let process_group_id = -(child.id() as i32);
        let rc = unsafe { libc::kill(process_group_id, libc::SIGKILL) };
        if rc == 0 {
            return;
        }

        let err = std::io::Error::last_os_error();
        warn!(
            pid = child.id(),
            error = %err,
            "failed to kill timed-out injector subprocess tree; falling back to direct child kill"
        );
    }

    if let Err(err) = child.kill() {
        warn!(pid = child.id(), error = %err, "failed to kill timed-out injector subprocess");
    }
}

#[cfg(test)]
fn format_child_failure(status: ExitStatus, stderr: &[u8]) -> String {
    let trimmed = String::from_utf8_lossy(stderr).trim().to_string();
    if trimmed.is_empty() {
        return format!("injector subprocess exited with status {status}");
    }
    format!("injector subprocess exited with status {status}: {trimmed}")
}

#[cfg(test)]
fn enrich_run_error_with_stderr(error: InjectionRunError, stderr: &[u8]) -> InjectionRunError {
    let trimmed = String::from_utf8_lossy(stderr).trim().to_string();
    if trimmed.is_empty() {
        return error;
    }

    match error {
        InjectionRunError::BackendFailure(message) => {
            InjectionRunError::BackendFailure(format!("{message}; stderr: {trimmed}"))
        }
        InjectionRunError::ExecutionTimeout(message) => {
            InjectionRunError::ExecutionTimeout(format!("{message}; stderr: {trimmed}"))
        }
        InjectionRunError::WorkerTaskFailed(message) => {
            InjectionRunError::WorkerTaskFailed(format!("{message}; stderr: {trimmed}"))
        }
    }
}

fn summarize_backend_attempts(attempts: &[BackendAttemptReport]) -> String {
    attempts
        .iter()
        .map(|attempt| {
            let mut summary = format!(
                "{}:{}:{}:{}ms",
                attempt.route_attempt_name, attempt.backend, attempt.status, attempt.duration_ms
            );
            if let Some(exit_status) = attempt.exit_status.as_ref() {
                summary.push_str(":exit=");
                summary.push_str(exit_status);
            }
            if !attempt.warning_tags.is_empty() {
                summary.push_str(":warn=");
                summary.push_str(&attempt.warning_tags.join(","));
            }
            if let Some(config) = attempt.backend_config.as_ref() {
                summary.push_str(":cfg=");
                summary.push_str(config);
            }
            if let Some(generation) = attempt.uinput_sender_generation {
                summary.push_str(":ugen=");
                summary.push_str(&generation.to_string());
            }
            if let Some(fresh) = attempt.uinput_fresh_device {
                summary.push_str(":ufresh=");
                summary.push_str(if fresh { "1" } else { "0" });
            }
            if let Some(age_ms) = attempt.uinput_device_age_ms_at_attempt {
                summary.push_str(":uage_ms=");
                summary.push_str(&age_ms.to_string());
            }
            if let Some(use_count) = attempt.uinput_use_count_before_attempt {
                summary.push_str(":uuse=");
                summary.push_str(&use_count.to_string());
            }
            if let Some(created_this_job) = attempt.uinput_created_this_job {
                summary.push_str(":ucreated_this_job=");
                summary.push_str(if created_this_job { "1" } else { "0" });
            }
            if let Some(create_elapsed_ms) = attempt.uinput_create_elapsed_ms {
                summary.push_str(":ucreate_ms=");
                summary.push_str(&create_elapsed_ms.to_string());
            }
            if let Some(reused_after_failure) = attempt.uinput_reused_after_failure {
                summary.push_str(":urecovered=");
                summary.push_str(if reused_after_failure { "1" } else { "0" });
            }
            if let Some(last_create_error) = attempt.uinput_last_create_error.as_ref() {
                summary.push_str(":uerr=");
                summary.push_str(last_create_error);
            }
            if let Some(stderr_excerpt) = attempt.stderr_excerpt.as_ref() {
                summary.push_str(":stderr=");
                summary.push_str(stderr_excerpt);
            }
            if let Some(error) = attempt.error.as_ref() {
                summary.push(':');
                summary.push_str(error);
            }
            summary
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn summarize_backend_warning_tags(attempts: &[BackendAttemptReport]) -> String {
    let tags = attempts
        .iter()
        .flat_map(|attempt| attempt.warning_tags.iter().cloned())
        .collect::<BTreeSet<_>>();
    if tags.is_empty() {
        return "none".to_string();
    }
    tags.into_iter().collect::<Vec<_>>().join(",")
}

fn summarize_backend_exit_statuses(attempts: &[BackendAttemptReport]) -> String {
    attempts
        .iter()
        .filter_map(|attempt| {
            attempt
                .exit_status
                .as_ref()
                .map(|status| format!("{}:{}", attempt.backend, status))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn summarize_backend_stderr_excerpts(attempts: &[BackendAttemptReport]) -> String {
    attempts
        .iter()
        .filter_map(|attempt| {
            attempt
                .stderr_excerpt
                .as_ref()
                .map(|stderr| format!("{}:{}", attempt.backend, stderr))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn log_injector_subprocess_stderr(session_id: Uuid, origin: InjectionOrigin, stderr: &[u8]) {
    if stderr.is_empty() {
        return;
    }

    let stderr_text = String::from_utf8_lossy(stderr);
    let mut logged_lines = 0usize;
    let mut dropped_lines = 0usize;
    for line in stderr_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if logged_lines >= INJECTOR_SUBPROCESS_STDERR_LOG_LINE_LIMIT {
            dropped_lines = dropped_lines.saturating_add(1);
            continue;
        }

        logged_lines = logged_lines.saturating_add(1);
        if let Some(payload) = trimmed.strip_prefix(INJECTOR_REPORT_PREFIX) {
            match serde_json::from_str::<InjectorChildReport>(payload) {
                Ok(report) => {
                    info!(
                        session = %session_id,
                        origin = origin.as_str(),
                        child_session = ?report.session_id,
                        child_origin = ?report.origin,
                        trace_id = report.trace_id,
                        outcome = report.outcome,
                        requested_len = report.requested_len,
                        requested_fingerprint = report.requested_fingerprint,
                        clipboard_ready = report.clipboard_ready,
                        clipboard_probe_count = report.clipboard_probe_count,
                        post_clipboard_matches = ?report.post_clipboard_matches,
                        parent_focus_app = report.parent_focus.as_ref().and_then(|focus| focus.snapshot.as_ref()).and_then(|snapshot| snapshot.app_name.as_deref()),
                        parent_focus_captured_elapsed_ms = report.parent_focus.as_ref().and_then(|focus| focus.captured_elapsed_ms),
                        child_focus_before_app = report.child_focus_before.as_ref().and_then(|snapshot| snapshot.app_name.as_deref()),
                        child_focus_after_app = report.child_focus_after.as_ref().and_then(|snapshot| snapshot.app_name.as_deref()),
                        child_focus_source_selected = report.child_focus_source_selected,
                        child_focus_wayland_cache_age_ms = ?report.child_focus_wayland_cache_age_ms,
                        child_focus_wayland_fallback_reason = ?report.child_focus_wayland_fallback_reason,
                        route_focus_source = report.route_focus_source,
                        route_class = report.route_class,
                        route_primary = report.route_primary,
                        route_adaptive_fallback = ?report.route_adaptive_fallback,
                        route_reason = report.route_reason,
                        error = ?report.error,
                        backend_attempt_count = report.backend_attempts.len(),
                        backend_warning_tags = summarize_backend_warning_tags(&report.backend_attempts),
                        backend_exit_statuses = summarize_backend_exit_statuses(&report.backend_attempts),
                        backend_stderr_excerpts = summarize_backend_stderr_excerpts(&report.backend_attempts),
                        backend_attempts = summarize_backend_attempts(&report.backend_attempts),
                        elapsed_ms_total = report.elapsed_ms_total,
                        "injector subprocess report"
                    );
                }
                Err(err) => {
                    warn!(
                        session = %session_id,
                        origin = origin.as_str(),
                        line_index = logged_lines,
                        error = %err,
                        line = %trimmed,
                        "failed to parse injector subprocess report line"
                    );
                }
            }
            continue;
        }
        info!(
            session = %session_id,
            origin = origin.as_str(),
            line_index = logged_lines,
            line = %trimmed,
            "injector subprocess log line"
        );
    }

    if dropped_lines > 0 {
        warn!(
            session = %session_id,
            origin = origin.as_str(),
            logged_lines,
            dropped_lines,
            line_limit = INJECTOR_SUBPROCESS_STDERR_LOG_LINE_LIMIT,
            "injector subprocess log lines were truncated"
        );
    }
}

fn spawn_injector_worker_with_capacity(
    runner: Arc<dyn InjectionJobRunner>,
    capacity: usize,
) -> (
    InjectorWorkerHandle,
    mpsc::UnboundedReceiver<InjectionReport>,
) {
    let (job_tx, mut job_rx) = mpsc::channel::<InjectionJob>(capacity.max(1));
    let (report_tx, report_rx) = mpsc::unbounded_channel::<InjectionReport>();
    let metrics = Arc::new(InjectorQueueMetrics::default());
    let worker_runner = Arc::clone(&runner);

    tokio::spawn(async move {
        while let Some(job) = job_rx.recv().await {
            let InjectionJob {
                session_id,
                text,
                daemon_latency_ms,
                daemon_audio_ms,
                origin,
                hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue,
                parent_focus,
                enqueued_at,
            } = job;

            let queue_wait_ms = enqueued_at.elapsed().as_millis() as u64;
            let hotkey_up_elapsed_ms_at_worker_start = hotkey_up_elapsed_ms_at_enqueue
                .map(|elapsed| elapsed.saturating_add(queue_wait_ms));
            let stop_message_elapsed_ms_at_worker_start = stop_message_elapsed_ms_at_enqueue
                .map(|elapsed| elapsed.saturating_add(queue_wait_ms));
            info!(
                session = %session_id,
                origin = origin.as_str(),
                daemon_latency_ms,
                daemon_audio_ms,
                queue_wait_ms,
                hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue,
                hotkey_up_elapsed_ms_at_worker_start,
                stop_message_elapsed_ms_at_worker_start,
                "injector worker starting job"
            );
            let worker_started = TokioInstant::now();
            let runner_for_job = Arc::clone(&worker_runner);
            let job_for_runner = InjectionJob {
                session_id,
                text: text.clone(),
                daemon_latency_ms,
                daemon_audio_ms,
                origin,
                hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue,
                parent_focus,
                enqueued_at,
            };
            let result =
                tokio::task::spawn_blocking(move || runner_for_job.run(&job_for_runner)).await;
            let run_ms = worker_started.elapsed().as_millis() as u64;
            let total_worker_ms = queue_wait_ms.saturating_add(run_ms);

            let mut run_output: Option<InjectionRunOutput> = None;
            let (error_kind, error) = match result {
                Ok(Ok(output)) => {
                    run_output = Some(output);
                    (None, None)
                }
                Ok(Err(InjectionRunError::BackendFailure(message))) => {
                    (Some(InjectionErrorKind::BackendFailure), Some(message))
                }
                Ok(Err(InjectionRunError::ExecutionTimeout(message))) => {
                    (Some(InjectionErrorKind::ExecutionTimeout), Some(message))
                }
                Ok(Err(InjectionRunError::WorkerTaskFailed(message))) => {
                    (Some(InjectionErrorKind::WorkerTaskFailed), Some(message))
                }
                Err(err) => (
                    Some(InjectionErrorKind::WorkerTaskFailed),
                    Some(format!("injector worker task failed: {err}")),
                ),
            };
            if let Some(output) = run_output {
                log_injector_subprocess_stderr(session_id, origin, &output.stderr);
            }
            let report = InjectionReport {
                session_id,
                daemon_latency_ms,
                daemon_audio_ms,
                origin,
                queue_wait_ms,
                run_ms,
                total_worker_ms,
                hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue,
                hotkey_up_elapsed_ms_at_worker_start,
                stop_message_elapsed_ms_at_worker_start,
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
    runner: Arc<dyn InjectionJobRunner>,
) -> (
    InjectorWorkerHandle,
    mpsc::UnboundedReceiver<InjectionReport>,
) {
    spawn_injector_worker_with_capacity(runner, INJECTION_QUEUE_CAPACITY)
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
        let forced_shortcut = cli.test_injection_shortcut.clone().map(Into::into);
        let injector = build_injector_with_shortcut_override(&config, None, forced_shortcut);
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
    build_injector_with_shortcut_override(config, context, None)
        .inject(&text)
        .context("internal injection failed")
}

fn build_uinput_chord_sender(config: &ClientConfig) -> Result<Arc<dyn PasteChordSender>> {
    Ok(Arc::new(UinputChordSender::new(config.uinput_dwell_ms)?))
}

fn build_backend_failure_fallback_injector(
    config: &ClientConfig,
    reason: String,
    context: Option<InjectorContext>,
    focus_cache: Option<WaylandFocusCache>,
) -> Arc<dyn TextInjector> {
    use crate::config::PasteBackendFailurePolicy;

    match config.clipboard.backend_failure_policy {
        PasteBackendFailurePolicy::CopyOnly => {
            warn!(
                reason = %reason,
                "paste backend unavailable; falling back to copy-only injection"
            );
            build_clipboard_injector_with_sender(
                config,
                PasteKeySender::Disabled,
                true,
                context,
                // Copy-only fallback never sends chords; forced shortcut is irrelevant.
                None,
                focus_cache,
            )
        }
        PasteBackendFailurePolicy::Error => {
            error!(
                reason = %reason,
                "paste backend unavailable and policy=error; returning explicit injector error"
            );
            Arc::new(FailInjector::new(reason))
        }
    }
}

fn build_clipboard_injector_with_sender(
    config: &ClientConfig,
    sender: PasteKeySender,
    copy_only: bool,
    context: Option<InjectorContext>,
    forced_shortcut: Option<crate::config::PasteShortcut>,
    focus_cache: Option<WaylandFocusCache>,
) -> Arc<dyn TextInjector> {
    use crate::config::InjectionMode;

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

    Arc::new(ClipboardInjector::new_with_shared_focus_cache(
        sender,
        config.clipboard.clone(),
        copy_only,
        context,
        forced_shortcut,
        focus_cache,
    ))
}

fn build_fresh_paste_key_sender(config: &ClientConfig) -> Result<PasteKeySender> {
    use crate::config::InjectionMode;

    if matches!(config.injection_mode, InjectionMode::CopyOnly) {
        return Ok(PasteKeySender::Disabled);
    }

    Ok(PasteKeySender::Uinput {
        sender: build_uinput_chord_sender(config)?,
        metadata: None,
    })
}

fn build_injector_with_shortcut_override(
    config: &ClientConfig,
    context: Option<InjectorContext>,
    forced_shortcut: Option<crate::config::PasteShortcut>,
) -> Arc<dyn TextInjector> {
    match build_fresh_paste_key_sender(config) {
        Ok(sender) => build_clipboard_injector_with_sender(
            config,
            sender,
            matches!(
                config.injection_mode,
                crate::config::InjectionMode::CopyOnly
            ),
            context,
            forced_shortcut,
            None,
        ),
        Err(err) => build_backend_failure_fallback_injector(
            config,
            format!(
                "paste_key_backend=uinput could not initialize /dev/uinput: {}",
                err
            ),
            context,
            None,
        ),
    }
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
    let injector_runner = build_injection_runner(&config);
    let (injector_worker, mut injection_reports) = spawn_injector_worker(injector_runner);

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
                    .enqueue(
                        InjectionJob::new(session_id, to_inject, latency_ms, audio_ms)
                            .with_origin(InjectionOrigin::Demo),
                    )
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
    let injector_runner = build_injection_runner(&config);
    let focus_cache = Some(WaylandFocusCache::new());
    let (injector_worker, mut injection_reports) = spawn_injector_worker(injector_runner);
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
    let mut last_hotkey_up_at: Option<TokioInstant> = None;
    let mut last_stop_message: Option<(Uuid, TokioInstant)> = None;
    let mut parent_focus_by_session = HashMap::<Uuid, CapturedParentFocus>::new();

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
                                            let now = TokioInstant::now();
                                            last_hotkey_up_at = Some(now);
                                            last_stop_message = Some((session_id, now));
                                            if let Some(focus) = capture_parent_focus(overlay_router.focus_cache.as_ref()) {
                                                parent_focus_by_session.insert(
                                                    session_id,
                                                    CapturedParentFocus {
                                                        focus,
                                                        captured_at: now,
                                                    },
                                                );
                                            }
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
                                                                    &mut parent_focus_by_session,
                                                                    last_hotkey_up_at,
                                                                    last_stop_message,
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
                                        let session_end_was_deferred = session_end_reason.is_some();
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
                                        let hotkey_up_elapsed_ms_at_enqueue =
                                            elapsed_ms_since(last_hotkey_up_at);
                                        let stop_message_elapsed_ms_at_enqueue =
                                            last_stop_message.and_then(|(stopped_session_id, instant)| {
                                                (stopped_session_id == session_id)
                                                    .then(|| instant.elapsed().as_millis() as u64)
                                            });
                                        info!(
                                            session = %session_id,
                                            origin = InjectionOrigin::LlmAnswer.as_str(),
                                            state_at_enqueue = state_label(&state),
                                            session_end_was_deferred,
                                            hotkey_up_elapsed_ms_at_enqueue,
                                            stop_message_elapsed_ms_at_enqueue,
                                            response_chars = to_inject.chars().count(),
                                            "queueing llm answer injection job"
                                        );

                                        match injector_worker
                                            .enqueue(
                                                InjectionJob::new(
                                                    session_id,
                                                    to_inject,
                                                    daemon_latency_ms,
                                                    daemon_audio_ms,
                                                )
                                                .with_origin(InjectionOrigin::LlmAnswer)
                                                .with_enqueue_timing(
                                                    hotkey_up_elapsed_ms_at_enqueue,
                                                    stop_message_elapsed_ms_at_enqueue,
                                                )
                                                .with_parent_focus(take_parent_focus_for_enqueue(
                                                    &mut parent_focus_by_session,
                                                    session_id,
                                                )),
                                            )
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
                origin = report.origin.as_str(),
                error_kind = error_kind.as_str(),
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                hotkey_up_elapsed_ms_at_worker_start = report.hotkey_up_elapsed_ms_at_worker_start,
                stop_message_elapsed_ms_at_worker_start = report.stop_message_elapsed_ms_at_worker_start,
                error = %error,
                "injector worker reported failure"
            );
        }
        (None, None) => {
            info!(
                session = %report.session_id,
                origin = report.origin.as_str(),
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                hotkey_up_elapsed_ms_at_worker_start = report.hotkey_up_elapsed_ms_at_worker_start,
                stop_message_elapsed_ms_at_worker_start = report.stop_message_elapsed_ms_at_worker_start,
                "injector worker completed job"
            );
            audio_feedback.play_completion();
        }
        (error_kind, error) => {
            warn!(
                session = %report.session_id,
                origin = report.origin.as_str(),
                error_kind = error_kind.map(InjectionErrorKind::as_str),
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                hotkey_up_elapsed_ms_at_worker_start = report.hotkey_up_elapsed_ms_at_worker_start,
                stop_message_elapsed_ms_at_worker_start = report.stop_message_elapsed_ms_at_worker_start,
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

fn capture_parent_focus(focus_cache: Option<&WaylandFocusCache>) -> Option<ParentFocusCapture> {
    let cache = focus_cache?;
    match cache.observe(30_000, 500) {
        WaylandFocusObservation::Fresh {
            snapshot,
            cache_age_ms,
        } => Some(ParentFocusCapture {
            snapshot: Some(snapshot),
            source_selected: "wayland_cache".to_string(),
            wayland_cache_age_ms: Some(cache_age_ms),
            wayland_fallback_reason: None,
            captured_elapsed_ms: Some(0),
        }),
        WaylandFocusObservation::LowConfidence {
            snapshot,
            cache_age_ms,
            reason,
        } => Some(ParentFocusCapture {
            snapshot: Some(snapshot),
            source_selected: "wayland_cache_low_confidence".to_string(),
            wayland_cache_age_ms: Some(cache_age_ms),
            wayland_fallback_reason: Some(reason.to_string()),
            captured_elapsed_ms: Some(0),
        }),
        WaylandFocusObservation::Unavailable {
            reason,
            cache_age_ms,
        } => Some(ParentFocusCapture {
            snapshot: None,
            source_selected: "wayland_unavailable".to_string(),
            wayland_cache_age_ms: cache_age_ms,
            wayland_fallback_reason: Some(reason.to_string()),
            captured_elapsed_ms: Some(0),
        }),
    }
}

fn take_parent_focus_for_enqueue(
    parent_focus_by_session: &mut HashMap<Uuid, CapturedParentFocus>,
    session_id: Uuid,
) -> Option<ParentFocusCapture> {
    parent_focus_by_session.remove(&session_id).map(|captured| {
        let mut focus = captured.focus;
        focus.captured_elapsed_ms = Some(captured.captured_at.elapsed().as_millis() as u64);
        focus
    })
}

async fn handle_server_message(
    message: ServerMessage,
    state: &mut PttState,
    overlay_router: &mut OverlayRouter<impl OverlaySink>,
    injector_worker: &InjectorWorkerHandle,
    parent_focus_by_session: &mut HashMap<Uuid, CapturedParentFocus>,
    last_hotkey_up_at: Option<TokioInstant>,
    last_stop_message: Option<(Uuid, TokioInstant)>,
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
            let hotkey_up_elapsed_ms_at_enqueue = elapsed_ms_since(last_hotkey_up_at);
            let stop_message_elapsed_ms_at_enqueue =
                last_stop_message.and_then(|(stopped_session_id, instant)| {
                    (stopped_session_id == session_id).then(|| instant.elapsed().as_millis() as u64)
                });
            info!(
                session = %session_id,
                origin = InjectionOrigin::RawFinalResult.as_str(),
                latency_ms,
                audio_ms,
                state_at_enqueue = state_label(state),
                hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue,
                "final result received"
            );
            match injector_worker
                .enqueue(
                    InjectionJob::new(session_id, text, latency_ms, audio_ms)
                        .with_origin(InjectionOrigin::RawFinalResult)
                        .with_enqueue_timing(
                            hotkey_up_elapsed_ms_at_enqueue,
                            stop_message_elapsed_ms_at_enqueue,
                        )
                        .with_parent_focus(take_parent_focus_for_enqueue(
                            parent_focus_by_session,
                            session_id,
                        )),
                )
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
            if let Some(session_id) = session_id {
                parent_focus_by_session.remove(&session_id);
            }
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
            parent_focus_by_session.remove(&session_id);
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

fn state_label(state: &PttState) -> &'static str {
    match state {
        PttState::Idle => "idle",
        PttState::Listening { .. } => "listening",
        PttState::WaitingResult { .. } => "waiting_result",
    }
}

fn elapsed_ms_since(instant: Option<TokioInstant>) -> Option<u64> {
    instant.map(|value| value.elapsed().as_millis() as u64)
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
    use std::collections::{HashMap, VecDeque};
    use std::ffi::OsString;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use std::time::Instant;

    use anyhow::anyhow;
    use clap::Parser;
    use tokio::sync::mpsc;
    use tokio::task::yield_now;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crate::config::{
        ClientConfig, ClipboardOptions, InjectionConfig, InjectionMode, OverlayMode,
        PasteBackendFailurePolicy, PasteKeyBackend, PasteShortcut,
    };
    use crate::injector::{FailInjector, PasteChordSender, PasteKeySender, TextInjector};
    use crate::overlay_process::{
        OverlayProcessManager, OverlayProcessMetrics, OverlayProcessSink,
    };
    use crate::protocol::ServerMessage;
    use crate::state::PttState;

    use super::{
        collect_pipe_reader, drain_sse_lines, handle_server_message, maybe_defer_llm_session_end,
        sanitize_model_answer, spawn_injector_worker_with_capacity, spawn_pipe_reader,
        EnqueueFailure, HotkeyIntentDiagnostics, InjectionErrorKind, InjectionJob,
        InjectionJobRunner, InjectionRunError, InjectionRunOutput, InjectorContext,
        InjectorSubprocessRunner, NoopOverlaySink, OverlayEvent, OverlayRouter, OverlaySink,
        RuntimeOverlaySink, SessionIntent, INJECTOR_JOB_TIMEOUT_MS,
    };

    async fn handle_server_message_for_tests<S: OverlaySink>(
        message: ServerMessage,
        state: &mut PttState,
        overlay_router: &mut OverlayRouter<S>,
        injector_worker: &super::InjectorWorkerHandle,
    ) -> anyhow::Result<()> {
        let mut parent_focus_by_session = HashMap::new();
        handle_server_message(
            message,
            state,
            overlay_router,
            injector_worker,
            &mut parent_focus_by_session,
            None,
            None,
        )
        .await
    }

    struct SlowRunner {
        calls: Arc<AtomicU64>,
        sleep_ms: u64,
    }

    impl InjectionJobRunner for SlowRunner {
        fn run(
            &self,
            _job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(self.sleep_ms));
            Ok(InjectionRunOutput::default())
        }
    }

    struct RecordingRunner {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl InjectionJobRunner for RecordingRunner {
        fn run(
            &self,
            job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            self.seen
                .lock()
                .expect("recording lock should be available")
                .push(job.text.to_string());
            Ok(InjectionRunOutput::default())
        }
    }

    struct TimeoutThenRecordingRunner {
        calls: Arc<AtomicU64>,
        seen: Arc<Mutex<Vec<String>>>,
        timeout_run_ms: u64,
    }

    impl InjectionJobRunner for TimeoutThenRecordingRunner {
        fn run(
            &self,
            job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            let call_index = self.calls.fetch_add(1, Ordering::Relaxed);
            if call_index == 0 {
                std::thread::sleep(Duration::from_millis(self.timeout_run_ms));
                return Err(InjectionRunError::ExecutionTimeout(format!(
                    "injector execution timed out after {INJECTOR_JOB_TIMEOUT_MS} ms"
                )));
            }

            self.seen
                .lock()
                .expect("recording lock should be available")
                .push(job.text.to_string());
            Ok(InjectionRunOutput::default())
        }
    }

    #[derive(Clone)]
    struct RecordingTextInjector {
        seen: Arc<Mutex<Vec<(String, Uuid)>>>,
    }

    impl TextInjector for RecordingTextInjector {
        fn inject(&self, _text: &str) -> anyhow::Result<()> {
            panic!("recording injector expects inject_with_context");
        }

        fn inject_with_context(
            &self,
            text: &str,
            context: Option<InjectorContext>,
        ) -> anyhow::Result<()> {
            let session_id = context
                .as_ref()
                .map(|value| value.session_id)
                .expect("in-process runner should pass injector context");
            self.seen
                .lock()
                .expect("recording injector lock should be available")
                .push((text.to_string(), session_id));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct RecordingPasteChordSender {
        sends: Arc<AtomicU64>,
        fail: bool,
    }

    impl PasteChordSender for RecordingPasteChordSender {
        fn send_shortcut(&self, _shortcut: PasteShortcut) -> anyhow::Result<()> {
            self.sends.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                anyhow::bail!("stage=backend synthetic sender failure");
            }
            Ok(())
        }

        fn backend_config(&self) -> Option<String> {
            Some("test_sender".to_string())
        }
    }

    #[derive(Clone)]
    struct SenderDrivenInjector {
        seen: Arc<Mutex<Vec<(String, Uuid)>>>,
        sender: PasteKeySender,
    }

    impl TextInjector for SenderDrivenInjector {
        fn inject(&self, _text: &str) -> anyhow::Result<()> {
            panic!("sender-driven injector expects inject_with_context");
        }

        fn inject_with_context(
            &self,
            text: &str,
            context: Option<InjectorContext>,
        ) -> anyhow::Result<()> {
            let session_id = context
                .as_ref()
                .map(|value| value.session_id)
                .expect("sender-driven injector expects session context");
            self.seen
                .lock()
                .expect("sender-driven injector lock should be available")
                .push((text.to_string(), session_id));
            if let PasteKeySender::Uinput { sender, .. } = &self.sender {
                sender.send_shortcut(PasteShortcut::CtrlV)?;
            }
            Ok(())
        }
    }

    fn test_client_config() -> ClientConfig {
        ClientConfig::new(
            "ws://127.0.0.1:8765/ws",
            None,
            "KEY_RIGHTCTRL".to_string(),
            InjectionConfig {
                uinput_dwell_ms: 18,
                injection_mode: InjectionMode::Paste,
                clipboard: ClipboardOptions {
                    key_backend: PasteKeyBackend::Uinput,
                    backend_failure_policy: PasteBackendFailurePolicy::CopyOnly,
                    post_chord_hold_ms: 700,
                    seat: None,
                    write_primary: false,
                },
            },
            Duration::from_secs(1),
        )
        .expect("test client config should be valid")
    }

    fn make_test_script(content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "parakeet-ptt-worker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        fs::write(&path, content).expect("test script should be writable");
        let mut perms = fs::metadata(&path)
            .expect("test script should exist")
            .permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&path, perms).expect("test script should be executable");
        path
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
    fn cli_default_paste_key_backend_is_uinput() {
        let cli = super::Cli::parse_from(["parakeet-ptt"]);
        assert!(matches!(
            cli.paste_key_backend,
            super::CliPasteKeyBackend::Uinput
        ));
    }

    #[test]
    fn cli_test_injection_defaults_are_stable() {
        let cli = super::Cli::parse_from(["parakeet-ptt", "--test-injection"]);
        assert!(cli.test_injection);
        assert_eq!(cli.test_injection_count, 1);
        assert_eq!(cli.test_injection_text_prefix, "Parakeet Test");
        assert_eq!(cli.test_injection_interval_ms, 150);
        assert_eq!(cli.test_injection_shortcut, None);
    }

    #[test]
    fn cli_test_injection_accepts_forced_shortcut() {
        let cli = super::Cli::parse_from([
            "parakeet-ptt",
            "--test-injection",
            "--test-injection-shortcut",
            "ctrl-shift-v",
        ]);
        assert!(matches!(
            cli.test_injection_shortcut,
            Some(super::CliTestInjectionShortcut::CtrlShiftV)
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
    fn in_process_runner_reuses_sender_across_jobs_when_healthy() {
        let build_count = Arc::new(AtomicU64::new(0));
        let sender_create_count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let config = test_client_config();
        let runner = super::InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new({
                let build_count = Arc::clone(&build_count);
                let seen = Arc::clone(&seen);
                move |_config, _sender, _focus_cache| {
                    build_count.fetch_add(1, Ordering::Relaxed);
                    Arc::new(RecordingTextInjector {
                        seen: Arc::clone(&seen),
                    })
                }
            }),
            Arc::new({
                let sender_create_count = Arc::clone(&sender_create_count);
                move |_config| {
                    sender_create_count.fetch_add(1, Ordering::Relaxed);
                    Ok(Arc::new(RecordingPasteChordSender {
                        sends: Arc::new(AtomicU64::new(0)),
                        fail: false,
                    }) as Arc<dyn PasteChordSender>)
                }
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            None,
        );

        let session_one = Uuid::new_v4();
        let session_two = Uuid::new_v4();
        runner
            .run(&InjectionJob::new(session_one, "first".to_string(), 0, 0))
            .expect("first run should succeed");
        runner
            .run(&InjectionJob::new(session_two, "second".to_string(), 0, 0))
            .expect("second run should succeed");

        assert_eq!(build_count.load(Ordering::Relaxed), 2);
        assert_eq!(sender_create_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen.lock()
                .expect("recording injector lock should be available")
                .as_slice(),
            &[
                ("first".to_string(), session_one),
                ("second".to_string(), session_two)
            ]
        );
    }

    #[test]
    fn in_process_runner_reuses_focus_cache_across_jobs() {
        let sender_create_count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed_focus_caches = Arc::new(Mutex::new(Vec::new()));
        let config = test_client_config();
        let shared_focus_cache = crate::surface_focus::WaylandFocusCache::new();
        let runner = super::InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new({
                let seen = Arc::clone(&seen);
                let observed_focus_caches = Arc::clone(&observed_focus_caches);
                move |_config, _sender, focus_cache| {
                    observed_focus_caches
                        .lock()
                        .expect("observed focus cache lock should be available")
                        .push(focus_cache.expect("runner should reuse a shared focus cache"));
                    Arc::new(RecordingTextInjector {
                        seen: Arc::clone(&seen),
                    })
                }
            }),
            Arc::new({
                let sender_create_count = Arc::clone(&sender_create_count);
                move |_config| {
                    sender_create_count.fetch_add(1, Ordering::Relaxed);
                    Ok(Arc::new(RecordingPasteChordSender {
                        sends: Arc::new(AtomicU64::new(0)),
                        fail: false,
                    }) as Arc<dyn PasteChordSender>)
                }
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            Some(shared_focus_cache.clone()),
        );

        let session_one = Uuid::new_v4();
        let session_two = Uuid::new_v4();
        runner
            .run(&InjectionJob::new(session_one, "first".to_string(), 0, 0))
            .expect("first run should succeed");
        runner
            .run(&InjectionJob::new(session_two, "second".to_string(), 0, 0))
            .expect("second run should succeed");

        let observed_focus_caches = observed_focus_caches
            .lock()
            .expect("observed focus cache lock should be available");
        assert_eq!(observed_focus_caches.len(), 2);
        assert!(observed_focus_caches[0].shares_worker_with(&observed_focus_caches[1]));
        assert!(observed_focus_caches[0].shares_worker_with(&shared_focus_cache));
        assert_eq!(sender_create_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen.lock()
                .expect("recording injector lock should be available")
                .as_slice(),
            &[
                ("first".to_string(), session_one),
                ("second".to_string(), session_two)
            ]
        );
    }

    #[test]
    fn in_process_runner_commits_sender_usage_only_after_success() {
        let config = test_client_config();
        let runner = super::InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new(|_config, _sender, _focus_cache| {
                Arc::new(FailInjector::new("unused test injector"))
            }),
            Arc::new(|_config| {
                Ok(Arc::new(RecordingPasteChordSender {
                    sends: Arc::new(AtomicU64::new(0)),
                    fail: false,
                }) as Arc<dyn PasteChordSender>)
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            None,
        );

        let sender = runner
            .prepare_paste_key_sender()
            .expect("preparing sender should succeed");
        let PasteKeySender::Uinput {
            metadata: Some(metadata),
            ..
        } = &sender
        else {
            panic!("paste mode should prepare a uinput sender");
        };
        assert!(metadata.fresh_device);
        assert_eq!(metadata.use_count_before_attempt, 0);

        {
            let manager = runner
                .sender_manager
                .lock()
                .expect("sender manager lock should be available");
            let super::UinputSenderState::Healthy(healthy) = &manager.state else {
                panic!("prepared sender should leave manager healthy");
            };
            assert!(healthy.fresh_pending);
            assert_eq!(healthy.use_count, 0);
        }

        runner.commit_successful_paste_key_sender(&sender);
        runner.commit_successful_paste_key_sender(&sender);

        {
            let manager = runner
                .sender_manager
                .lock()
                .expect("sender manager lock should be available");
            let super::UinputSenderState::Healthy(healthy) = &manager.state else {
                panic!("committed sender should keep manager healthy");
            };
            assert!(!healthy.fresh_pending);
            assert_eq!(healthy.use_count, 1);
        }

        let sender = runner
            .prepare_paste_key_sender()
            .expect("preparing reused sender should succeed");
        let PasteKeySender::Uinput {
            metadata: Some(metadata),
            ..
        } = &sender
        else {
            panic!("paste mode should keep using a uinput sender");
        };
        assert!(!metadata.fresh_device);
        assert_eq!(metadata.use_count_before_attempt, 1);
    }

    #[test]
    fn in_process_runner_retries_after_create_failure_without_restart() {
        let build_count = Arc::new(AtomicU64::new(0));
        let sender_create_count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let config = test_client_config();
        let runner = super::InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new({
                let build_count = Arc::clone(&build_count);
                let seen = Arc::clone(&seen);
                move |_config, _sender, _focus_cache| {
                    build_count.fetch_add(1, Ordering::Relaxed);
                    Arc::new(RecordingTextInjector {
                        seen: Arc::clone(&seen),
                    })
                }
            }),
            Arc::new({
                let sender_create_count = Arc::clone(&sender_create_count);
                move |_config| {
                    let attempt = sender_create_count.fetch_add(1, Ordering::Relaxed);
                    if attempt == 0 {
                        anyhow::bail!("synthetic /dev/uinput unavailable");
                    }
                    Ok(Arc::new(RecordingPasteChordSender {
                        sends: Arc::new(AtomicU64::new(0)),
                        fail: false,
                    }) as Arc<dyn PasteChordSender>)
                }
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            None,
        );

        let _session_one = Uuid::new_v4();
        runner
            .run(&InjectionJob::new(_session_one, "first".to_string(), 0, 0))
            .expect("copy-only fallback should keep first run alive");
        std::thread::sleep(Duration::from_millis(6));
        let session_two = Uuid::new_v4();
        runner
            .run(&InjectionJob::new(session_two, "second".to_string(), 0, 0))
            .expect("second run should recover after retry backoff");

        assert_eq!(sender_create_count.load(Ordering::Relaxed), 2);
        assert_eq!(build_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen.lock()
                .expect("recording injector lock should be available")
                .as_slice(),
            &[("second".to_string(), session_two)]
        );
    }

    #[test]
    fn in_process_runner_drops_sender_after_explicit_send_error() {
        let sender_create_count = Arc::new(AtomicU64::new(0));
        let send_count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let config = test_client_config();
        let runner = super::InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new({
                let seen = Arc::clone(&seen);
                move |_config, sender, _focus_cache| {
                    Arc::new(SenderDrivenInjector {
                        seen: Arc::clone(&seen),
                        sender,
                    })
                }
            }),
            Arc::new({
                let sender_create_count = Arc::clone(&sender_create_count);
                let send_count = Arc::clone(&send_count);
                move |_config| {
                    let generation = sender_create_count.fetch_add(1, Ordering::Relaxed);
                    Ok(Arc::new(RecordingPasteChordSender {
                        sends: Arc::clone(&send_count),
                        fail: generation == 0,
                    }) as Arc<dyn PasteChordSender>)
                }
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            None,
        );

        let first = runner.run(&InjectionJob::new(
            Uuid::new_v4(),
            "first".to_string(),
            0,
            0,
        ));
        assert!(matches!(first, Err(InjectionRunError::BackendFailure(_))));

        let second = runner.run(&InjectionJob::new(
            Uuid::new_v4(),
            "second".to_string(),
            0,
            0,
        ));
        assert!(second.is_ok());
        assert_eq!(sender_create_count.load(Ordering::Relaxed), 2);
        assert_eq!(send_count.load(Ordering::Relaxed), 2);
        assert_eq!(
            seen.lock()
                .expect("sender-driven injector lock should be available")
                .len(),
            2
        );
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
        let slow_injector = Arc::new(SlowRunner {
            calls: Arc::clone(&calls),
            sleep_ms: 120,
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(slow_injector, 8);

        let mut state = PttState::new();
        let session_id = state.begin_listening().expect("state should start");
        state.stop_listening();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);
        let message = ServerMessage::FinalResult {
            session_id,
            text: "hello from daemon".to_string(),
            latency_ms: 60,
            audio_ms: 1900,
            lang: Some("en".to_string()),
            confidence: Some(0.99),
        };

        let started = Instant::now();
        handle_server_message_for_tests(message, &mut state, &mut overlay_router, &worker)
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
        let injector = Arc::new(RecordingRunner {
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
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&injector_seen),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 4);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "listening".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim state should route to overlay");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "hello".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim text should route to overlay");
        handle_server_message_for_tests(
            ServerMessage::SessionEnded {
                session_id,
                reason: Some("normal".to_string()),
            },
            &mut state,
            &mut overlay_router,
            &worker,
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
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "processing".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim state should route");
        handle_server_message_for_tests(
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
        )
        .await
        .expect("final result should enqueue exactly once");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "post-final overlay".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
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
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 1,
                text: "overlay event while disconnected".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("overlay disconnect should be non-fatal");
        assert_eq!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed),
            1
        );

        handle_server_message_for_tests(
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
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        for seq in 1..=4 {
            handle_server_message_for_tests(
                ServerMessage::InterimText {
                    session_id,
                    seq,
                    text: format!("overlay seq {seq}"),
                },
                &mut state,
                &mut overlay_router,
                &worker,
            )
            .await
            .expect("overlay failures should remain non-fatal");
        }

        handle_server_message_for_tests(
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
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 1,
                text: "old-state".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
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

        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "current-state".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
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

        handle_server_message_for_tests(
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
        let injector = Arc::new(RecordingRunner {
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 10,
                text: "newest".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("first interim text should route");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 9,
                text: "stale".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
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
        let injector = Arc::new(TimeoutThenRecordingRunner {
            calls: Arc::clone(&calls),
            seen: Arc::clone(&seen),
            timeout_run_ms: INJECTOR_JOB_TIMEOUT_MS + 75,
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
    async fn injector_worker_timeout_kills_subprocess_tree_before_next_job_runs() {
        let log_path = std::env::temp_dir().join(format!(
            "parakeet-ptt-injector-worker-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        let script = make_test_script(
            "#!/usr/bin/env bash\nset -euo pipefail\nlog_path=\"$1\"\ntext=\"$(cat)\"\nif [ \"$text\" = \"first wedges\" ]; then\n  (\n    sleep 0.35\n    printf '%s\\n' \"$text\" >>\"$log_path\"\n  ) &\n  sleep 0.35\n  wait\n  exit 0\nfi\nprintf '%s\\n' \"$text\" >>\"$log_path\"\n",
        );
        let runner = Arc::new(InjectorSubprocessRunner::new_for_tests(
            script.clone(),
            vec![OsString::from(log_path.as_os_str())],
            Duration::from_millis(INJECTOR_JOB_TIMEOUT_MS),
        ));
        let (worker, mut reports) = spawn_injector_worker_with_capacity(runner, 4);

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
                "second survives".to_string(),
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

        let second_report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("second report should arrive")
            .expect("report stream should remain open");
        assert_eq!(second_report.session_id, second_session);
        assert_eq!(second_report.error_kind, None);

        tokio::time::sleep(Duration::from_millis(450)).await;

        let written = fs::read_to_string(&log_path).expect("log file should be readable");
        assert_eq!(written, "second survives\n");

        fs::remove_file(&script).expect("test script should be removable");
        fs::remove_file(&log_path).expect("log file should be removable");
    }

    #[test]
    fn injector_subprocess_runner_does_not_wait_for_background_grandchild_stderr_close() {
        let log_path = std::env::temp_dir().join(format!(
            "parakeet-ptt-inherited-stderr-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        let script = make_test_script(
            "#!/usr/bin/env bash\nset -euo pipefail\nlog_path=\"$1\"\npayload=\"$(cat)\"\nprintf '%s %s\\n' \"$(date +%s%3N)\" \"$payload\" >>\"$log_path\"\n(sleep 0.4) &\nexit 0\n",
        );
        let runner = InjectorSubprocessRunner::new_for_tests(
            script.clone(),
            vec![OsString::from(log_path.as_os_str())],
            Duration::from_secs(2),
        );

        let started = Instant::now();
        runner
            .run(&InjectionJob::new(
                Uuid::new_v4(),
                "clipboard helper should not pin the worker".to_string(),
                0,
                0,
            ))
            .expect("runner should treat the injector subprocess as successful");
        let elapsed = started.elapsed();

        let written = fs::read_to_string(&log_path).expect("log file should be readable");
        assert!(
            written.contains("clipboard helper should not pin the worker"),
            "script should record the injected payload"
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "runner should not wait for inherited stderr handles from background helpers, elapsed={elapsed:?}"
        );

        fs::remove_file(&script).expect("test script should be removable");
        fs::remove_file(&log_path).expect("log file should be removable");
    }

    #[test]
    fn pipe_reader_applies_deadline_only_after_it_is_started() {
        let (mut writer, reader) = UnixStream::pair().expect("unix stream pair should open");
        writer
            .write_all(b"partial stderr")
            .expect("writer should accept bytes");

        let stderr_reader = spawn_pipe_reader(reader, Duration::from_millis(40));
        assert!(
            matches!(
                stderr_reader
                    .receiver
                    .recv_timeout(Duration::from_millis(80)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "reader should keep waiting for EOF until the post-exit deadline is armed"
        );

        let started = Instant::now();
        stderr_reader.start_deadline();
        let outcome = collect_pipe_reader(stderr_reader, "stderr", Duration::from_millis(200))
            .expect("reader should return once the post-exit drain deadline elapses");
        let elapsed = started.elapsed();

        assert_eq!(outcome.bytes, b"partial stderr");
        assert!(
            outcome.timed_out,
            "open writers should force a timed post-exit drain result instead of blocking for EOF"
        );
        assert!(
            elapsed < Duration::from_millis(150),
            "pipe reader should stop itself near the post-exit drain deadline, elapsed={elapsed:?}"
        );

        drop(writer);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_helper_lifetime_does_not_delay_following_injection_jobs() {
        let log_path = std::env::temp_dir().join(format!(
            "parakeet-ptt-background-helper-queue-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        let script = make_test_script(
            "#!/usr/bin/env bash\nset -euo pipefail\nlog_path=\"$1\"\npayload=\"$(cat)\"\nprintf '%s %s\\n' \"$(date +%s%3N)\" \"$payload\" >>\"$log_path\"\n(sleep 0.4) &\nexit 0\n",
        );
        let runner = Arc::new(InjectorSubprocessRunner::new_for_tests(
            script.clone(),
            vec![OsString::from(log_path.as_os_str())],
            Duration::from_secs(2),
        ));
        let (worker, mut reports) = spawn_injector_worker_with_capacity(runner, 4);

        worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "first".to_string(), 1, 1))
            .await
            .expect("first enqueue should pass");
        worker
            .enqueue(InjectionJob::new(
                Uuid::new_v4(),
                "second".to_string(),
                2,
                2,
            ))
            .await
            .expect("second enqueue should pass");

        timeout(Duration::from_millis(250), reports.recv())
            .await
            .expect("first report should not wait for background helper exit")
            .expect("report stream should remain open");
        timeout(Duration::from_millis(250), reports.recv())
            .await
            .expect("second report should not be blocked behind the prior helper lifetime")
            .expect("report stream should remain open");

        let written = fs::read_to_string(&log_path).expect("log file should be readable");
        let entries = written
            .lines()
            .map(|line| {
                let (timestamp, payload) = line
                    .split_once(' ')
                    .expect("log line should contain timestamp and payload");
                (
                    timestamp
                        .parse::<u128>()
                        .expect("timestamp should parse as milliseconds"),
                    payload.to_string(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(entries.len(), 2, "both jobs should have executed");
        assert_eq!(entries[0].1, "first");
        assert_eq!(entries[1].1, "second");
        assert!(
            entries[1].0.saturating_sub(entries[0].0) < 200,
            "second job should start promptly instead of waiting for prior clipboard helper teardown"
        );

        fs::remove_file(&script).expect("test script should be removable");
        fs::remove_file(&log_path).expect("log file should be removable");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_times_out_when_queue_remains_saturated() {
        let slow = Arc::new(SlowRunner {
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
