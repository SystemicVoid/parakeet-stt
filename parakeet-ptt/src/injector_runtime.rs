//! Injection runtime ports for Parakeet Client PTT Sessions.
//!
//! This module owns Client Injection job execution: the `InjectionJobRunner`
//! port, in-process adapter construction, worker queue, and subprocess test
//! harness used to verify child lifecycle behavior.

use std::collections::BTreeSet;
#[cfg(test)]
use std::io::Read;
#[cfg(test)]
use std::io::Write;
#[cfg(all(test, unix))]
use std::os::fd::AsRawFd;
#[cfg(all(test, unix))]
use std::os::unix::process::CommandExt;
#[cfg(test)]
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

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration as TokioDuration, Instant as TokioInstant};
#[cfg(test)]
use tracing::debug;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::ClientConfig;
use crate::injector::{
    BackendAttemptReport, ClipboardInjector, FailInjector, InjectorChildReport, InjectorContext,
    ParentFocusCapture, PasteChordSender, PasteKeySender, TextInjector, UinputAttemptMetadata,
    UinputChordSender, INJECTOR_REPORT_PREFIX,
};
#[cfg(test)]
use crate::injector::{
    INJECTOR_CONTEXT_ENV, INJECTOR_PIPE_DRAIN_TIMEOUT_MS, INJECTOR_PIPE_READER_JOIN_SLACK_MS,
    INJECTOR_SUBPROCESS_POLL_INTERVAL_MS,
};
use crate::surface_focus::WaylandFocusCache;

pub(crate) const INJECTION_QUEUE_CAPACITY: usize = 32;
pub(crate) const INJECTION_ENQUEUE_TIMEOUT_MS: u64 = 20;
const INJECTOR_SUBPROCESS_STDERR_LOG_LINE_LIMIT: usize = 120;
const IN_PROCESS_UINPUT_WARMUP_MS: u64 = 200;
const IN_PROCESS_UINPUT_RETRY_BACKOFF_MS: u64 = 500;

#[derive(Debug, Clone)]
pub struct InjectionJob {
    pub(crate) session_id: Uuid,
    pub(crate) text: String,
    pub(crate) daemon_latency_ms: u64,
    pub(crate) daemon_audio_ms: u64,
    pub(crate) origin: InjectionOrigin,
    pub(crate) hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
    pub(crate) stop_message_elapsed_ms_at_enqueue: Option<u64>,
    pub(crate) parent_focus: Option<ParentFocusCapture>,
    pub(crate) enqueued_at: TokioInstant,
}

impl InjectionJob {
    pub(crate) fn new(
        session_id: Uuid,
        text: String,
        daemon_latency_ms: u64,
        daemon_audio_ms: u64,
    ) -> Self {
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

    pub(crate) fn with_origin(mut self, origin: InjectionOrigin) -> Self {
        self.origin = origin;
        self
    }

    pub(crate) fn with_enqueue_timing(
        mut self,
        hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
        stop_message_elapsed_ms_at_enqueue: Option<u64>,
    ) -> Self {
        self.hotkey_up_elapsed_ms_at_enqueue = hotkey_up_elapsed_ms_at_enqueue;
        self.stop_message_elapsed_ms_at_enqueue = stop_message_elapsed_ms_at_enqueue;
        self
    }

    pub(crate) fn with_parent_focus(mut self, parent_focus: Option<ParentFocusCapture>) -> Self {
        self.parent_focus = parent_focus;
        self
    }
}

#[derive(Debug)]
pub(crate) struct InjectionReport {
    pub(crate) session_id: Uuid,
    pub(crate) daemon_latency_ms: u64,
    pub(crate) daemon_audio_ms: u64,
    pub(crate) origin: InjectionOrigin,
    pub(crate) queue_wait_ms: u64,
    pub(crate) run_ms: u64,
    pub(crate) total_worker_ms: u64,
    pub(crate) hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
    pub(crate) stop_message_elapsed_ms_at_enqueue: Option<u64>,
    pub(crate) hotkey_up_elapsed_ms_at_worker_start: Option<u64>,
    pub(crate) stop_message_elapsed_ms_at_worker_start: Option<u64>,
    pub(crate) error_kind: Option<InjectionErrorKind>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InjectionOrigin {
    RawFinalResult,
    LlmAnswer,
    Demo,
    Unspecified,
}

impl InjectionOrigin {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RawFinalResult => "raw_final_result",
            Self::LlmAnswer => "llm_answer",
            Self::Demo => "demo",
            Self::Unspecified => "unspecified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InjectionErrorKind {
    BackendFailure,
    ExecutionTimeout,
    WorkerTaskFailed,
}

impl InjectionErrorKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BackendFailure => "backend_failure",
            Self::ExecutionTimeout => "execution_timeout",
            Self::WorkerTaskFailed => "worker_task_failed",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct InjectorQueueMetrics {
    pub(crate) queued_total: AtomicU64,
    pub(crate) enqueue_blocked_total: AtomicU64,
    pub(crate) enqueue_timeout_total: AtomicU64,
    pub(crate) enqueue_worker_gone_total: AtomicU64,
    pub(crate) worker_success_total: AtomicU64,
    pub(crate) worker_failure_total: AtomicU64,
    pub(crate) worker_backend_failure_total: AtomicU64,
    pub(crate) worker_execution_timeout_total: AtomicU64,
    pub(crate) worker_task_failed_total: AtomicU64,
    pub(crate) worker_queue_wait_ms_total: AtomicU64,
    pub(crate) worker_run_ms_total: AtomicU64,
    pub(crate) queue_depth_high_water: AtomicU64,
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

    pub(crate) fn note_report(&self, report: &InjectionReport) {
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

    pub(crate) fn log_summary(&self) {
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
pub(crate) enum EnqueueFailure {
    Timeout,
    WorkerGone,
}

#[derive(Clone)]
pub(crate) struct InjectorWorkerHandle {
    sender: mpsc::Sender<InjectionJob>,
    metrics: Arc<InjectorQueueMetrics>,
}

impl InjectorWorkerHandle {
    pub(crate) async fn enqueue(
        &self,
        job: InjectionJob,
    ) -> std::result::Result<(), EnqueueFailure> {
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

    pub(crate) fn metrics(&self) -> &Arc<InjectorQueueMetrics> {
        &self.metrics
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub enum InjectionRunError {
    BackendFailure(String),
    ExecutionTimeout(String),
    WorkerTaskFailed(String),
}

#[derive(Debug, Default)]
pub struct InjectionRunOutput {
    stderr: Vec<u8>,
}

pub trait InjectionJobRunner: Send + Sync {
    fn run(&self, job: &InjectionJob)
        -> std::result::Result<InjectionRunOutput, InjectionRunError>;
}

pub(crate) type InProcessInjectorBuilder = dyn Fn(&ClientConfig, PasteKeySender, Option<WaylandFocusCache>) -> Arc<dyn TextInjector>
    + Send
    + Sync;
pub(crate) type InProcessPasteSenderFactory =
    dyn Fn(&ClientConfig) -> Result<Arc<dyn PasteChordSender>> + Send + Sync;
pub(crate) type InProcessSleepFn = dyn Fn(Duration) + Send + Sync;

#[derive(Debug)]
pub(crate) struct HealthyUinputSender {
    sender: Arc<dyn PasteChordSender>,
    generation: u64,
    created_at: Instant,
    pub(crate) use_count: u64,
    pub(crate) fresh_pending: bool,
    recovered_after_failure: bool,
}

#[derive(Debug)]
pub(crate) struct FailedUinputSender {
    last_error: String,
    retry_after: Instant,
}

#[derive(Debug)]
pub(crate) enum UinputSenderState {
    Uninitialized,
    Healthy(HealthyUinputSender),
    CreateFailed(FailedUinputSender),
}

#[derive(Debug)]
pub(crate) struct UinputSenderManager {
    pub(crate) state: UinputSenderState,
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
pub(crate) struct InProcessInjectorRunner {
    config: ClientConfig,
    pub(crate) sender_manager: Arc<Mutex<UinputSenderManager>>,
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
    pub(crate) fn new_for_tests(
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

    pub(crate) fn prepare_paste_key_sender(&self) -> std::result::Result<PasteKeySender, String> {
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

    pub(crate) fn commit_successful_paste_key_sender(&self, sender: &PasteKeySender) {
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
pub(crate) struct InjectorSubprocessRunner {
    executable: PathBuf,
    base_args: Vec<std::ffi::OsString>,
    timeout: Duration,
}

#[cfg(test)]
impl InjectorSubprocessRunner {
    pub(crate) fn new_for_tests(
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

pub fn build_injection_runner(config: &ClientConfig) -> Arc<dyn InjectionJobRunner> {
    Arc::new(InProcessInjectorRunner::new(config))
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct PipeReadOutcome {
    pub(crate) bytes: Vec<u8>,
    pub(crate) timed_out: bool,
}

#[cfg(test)]
pub(crate) struct PipeReaderHandle {
    pub(crate) receiver: std::sync::mpsc::Receiver<std::io::Result<PipeReadOutcome>>,
    pub(crate) deadline_started: Arc<AtomicBool>,
}

#[cfg(test)]
impl PipeReaderHandle {
    pub(crate) fn start_deadline(&self) {
        self.deadline_started.store(true, Ordering::Release);
    }
}

#[cfg(test)]
pub(crate) fn spawn_pipe_reader<R>(reader: R, timeout: Duration) -> PipeReaderHandle
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
pub(crate) fn collect_pipe_reader(
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

pub(crate) fn spawn_injector_worker_with_capacity(
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

pub(crate) fn spawn_injector_worker(
    runner: Arc<dyn InjectionJobRunner>,
) -> (
    InjectorWorkerHandle,
    mpsc::UnboundedReceiver<InjectionReport>,
) {
    spawn_injector_worker_with_capacity(runner, INJECTION_QUEUE_CAPACITY)
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

pub fn build_injector_with_shortcut_override(
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::time::timeout;
    use uuid::Uuid;

    use super::{
        spawn_injector_worker_with_capacity, InjectionErrorKind, InjectionJob,
        InjectorSubprocessRunner,
    };
    use crate::injector::INJECTOR_JOB_TIMEOUT_MS;

    fn make_test_script(content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "parakeet-ptt-injector-runtime-test-{}-{}",
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

    #[tokio::test(flavor = "current_thread")]
    async fn subprocess_timeout_kills_child_tree_before_next_job_runs() {
        let log_path = std::env::temp_dir().join(format!(
            "parakeet-ptt-injector-runtime-log-{}-{}",
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
}
