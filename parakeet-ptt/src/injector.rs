use std::hash::{Hash, Hasher};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId, Key};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{ClipboardOptions, PasteShortcut};
use crate::routing::decide_route;
use crate::surface_focus::{FocusSnapshot, WaylandFocusCache, WaylandFocusObservation};

static INJECTION_TRACE_ID: AtomicU64 = AtomicU64::new(1);

/// MIME type used for all wl-copy clipboard writes.
const CLIPBOARD_MIME_TYPE: &str = "text/plain;charset=utf-8";
const STAGE_CLIPBOARD_READY: &str = "clipboard_ready";
const STAGE_ROUTE_SHORTCUT: &str = "route_shortcut";
const STAGE_BACKEND: &str = "backend";
// The command timeout still drives runtime helper subprocesses such as wl-copy.
#[cfg(not(test))]
pub(crate) const INJECTOR_CHILD_COMMAND_TIMEOUT_MS: u64 = 1_000;
#[cfg(test)]
pub(crate) const INJECTOR_CHILD_COMMAND_TIMEOUT_MS: u64 = 150;
// The worker-level timeout constants are only used by the test-only subprocess
// harness in main.rs.
#[cfg(test)]
pub(crate) const INJECTOR_JOB_TIMEOUT_SLACK_MS: u64 = 0;
#[cfg(test)]
pub(crate) const INJECTOR_JOB_TIMEOUT_MS: u64 =
    INJECTOR_CHILD_COMMAND_TIMEOUT_MS + INJECTOR_JOB_TIMEOUT_SLACK_MS;
pub(crate) const INJECTOR_SUBPROCESS_POLL_INTERVAL_MS: u64 = 5;
#[cfg(test)]
pub(crate) const INJECTOR_PIPE_DRAIN_TIMEOUT_MS: u64 = 50;
#[cfg(test)]
pub(crate) const INJECTOR_PIPE_READER_JOIN_SLACK_MS: u64 = 10;
pub(crate) const INJECTOR_CONTEXT_ENV: &str = "PARAKEET_INJECT_CONTEXT_JSON";
pub(crate) const INJECTOR_REPORT_PREFIX: &str = "PARAKEET_INJECT_REPORT ";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ParentFocusCapture {
    pub snapshot: Option<FocusSnapshot>,
    pub source_selected: String,
    pub wayland_cache_age_ms: Option<u64>,
    pub wayland_fallback_reason: Option<String>,
    pub captured_elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InjectorContext {
    pub session_id: Uuid,
    pub origin: String,
    pub hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
    pub stop_message_elapsed_ms_at_enqueue: Option<u64>,
    pub parent_focus: Option<ParentFocusCapture>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BackendAttemptReport {
    pub route_attempt_name: String,
    pub route_attempt_index: usize,
    pub route_attempt_total: usize,
    pub backend: String,
    pub backend_attempt_index: usize,
    pub backend_attempt_total: usize,
    pub shortcut: String,
    pub status: String,
    pub duration_ms: u64,
    pub exit_status: Option<String>,
    pub stderr_excerpt: Option<String>,
    pub warning_tags: Vec<String>,
    pub backend_config: Option<String>,
    pub error: Option<String>,
    pub uinput_sender_generation: Option<u64>,
    pub uinput_fresh_device: Option<bool>,
    pub uinput_device_age_ms_at_attempt: Option<u64>,
    pub uinput_use_count_before_attempt: Option<u64>,
    pub uinput_created_this_job: Option<bool>,
    pub uinput_create_elapsed_ms: Option<u64>,
    pub uinput_last_create_error: Option<String>,
    pub uinput_reused_after_failure: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InjectorChildReport {
    pub session_id: Option<Uuid>,
    pub origin: Option<String>,
    pub trace_id: u64,
    pub outcome: String,
    pub requested_len: usize,
    pub requested_fingerprint: String,
    pub clipboard_ready: bool,
    pub clipboard_probe_count: u64,
    pub post_clipboard_matches: Option<bool>,
    pub parent_focus: Option<ParentFocusCapture>,
    pub child_focus_before: Option<FocusSnapshot>,
    pub child_focus_after: Option<FocusSnapshot>,
    pub child_focus_source_selected: String,
    pub child_focus_wayland_cache_age_ms: Option<u64>,
    pub child_focus_wayland_fallback_reason: Option<String>,
    pub route_focus_source: String,
    pub route_class: String,
    pub route_primary: String,
    pub route_adaptive_fallback: Option<String>,
    pub route_reason: String,
    pub backend_attempts: Vec<BackendAttemptReport>,
    pub elapsed_ms_total: u64,
}

#[derive(Debug, Clone, Copy)]
enum InjectionStage {
    ClipboardReady,
    RouteShortcut,
    Backend,
}

impl InjectionStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClipboardReady => STAGE_CLIPBOARD_READY,
            Self::RouteShortcut => STAGE_ROUTE_SHORTCUT,
            Self::Backend => STAGE_BACKEND,
        }
    }
}

#[derive(Debug, Default)]
struct InjectorMetrics {
    clipboard_ready_success_total: AtomicU64,
    clipboard_ready_failure_total: AtomicU64,
    clipboard_ready_duration_ms_total: AtomicU64,
    route_shortcut_success_total: AtomicU64,
    route_shortcut_failure_total: AtomicU64,
    route_shortcut_duration_ms_total: AtomicU64,
    backend_success_total: AtomicU64,
    backend_failure_total: AtomicU64,
    backend_duration_ms_total: AtomicU64,
    wl_copy_spawn_total: AtomicU64,
    wl_paste_spawn_total: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct InjectorMetricsSnapshot {
    pub clipboard_ready_success_total: u64,
    pub clipboard_ready_failure_total: u64,
    pub clipboard_ready_duration_ms_total: u64,
    pub route_shortcut_success_total: u64,
    pub route_shortcut_failure_total: u64,
    pub route_shortcut_duration_ms_total: u64,
    pub backend_success_total: u64,
    pub backend_failure_total: u64,
    pub backend_duration_ms_total: u64,
    pub wl_copy_spawn_total: u64,
    pub wl_paste_spawn_total: u64,
}

impl InjectorMetrics {
    fn note_stage_success(&self, stage: InjectionStage, duration_ms: u64) {
        match stage {
            InjectionStage::ClipboardReady => {
                self.clipboard_ready_success_total
                    .fetch_add(1, Ordering::Relaxed);
                self.clipboard_ready_duration_ms_total
                    .fetch_add(duration_ms, Ordering::Relaxed);
            }
            InjectionStage::RouteShortcut => {
                self.route_shortcut_success_total
                    .fetch_add(1, Ordering::Relaxed);
                self.route_shortcut_duration_ms_total
                    .fetch_add(duration_ms, Ordering::Relaxed);
            }
            InjectionStage::Backend => {
                self.backend_success_total.fetch_add(1, Ordering::Relaxed);
                self.backend_duration_ms_total
                    .fetch_add(duration_ms, Ordering::Relaxed);
            }
        }
    }

    fn note_stage_failure(&self, stage: InjectionStage, duration_ms: u64) {
        match stage {
            InjectionStage::ClipboardReady => {
                self.clipboard_ready_failure_total
                    .fetch_add(1, Ordering::Relaxed);
                self.clipboard_ready_duration_ms_total
                    .fetch_add(duration_ms, Ordering::Relaxed);
            }
            InjectionStage::RouteShortcut => {
                self.route_shortcut_failure_total
                    .fetch_add(1, Ordering::Relaxed);
                self.route_shortcut_duration_ms_total
                    .fetch_add(duration_ms, Ordering::Relaxed);
            }
            InjectionStage::Backend => {
                self.backend_failure_total.fetch_add(1, Ordering::Relaxed);
                self.backend_duration_ms_total
                    .fetch_add(duration_ms, Ordering::Relaxed);
            }
        }
    }

    fn snapshot(&self) -> InjectorMetricsSnapshot {
        InjectorMetricsSnapshot {
            clipboard_ready_success_total: self
                .clipboard_ready_success_total
                .load(Ordering::Relaxed),
            clipboard_ready_failure_total: self
                .clipboard_ready_failure_total
                .load(Ordering::Relaxed),
            clipboard_ready_duration_ms_total: self
                .clipboard_ready_duration_ms_total
                .load(Ordering::Relaxed),
            route_shortcut_success_total: self.route_shortcut_success_total.load(Ordering::Relaxed),
            route_shortcut_failure_total: self.route_shortcut_failure_total.load(Ordering::Relaxed),
            route_shortcut_duration_ms_total: self
                .route_shortcut_duration_ms_total
                .load(Ordering::Relaxed),
            backend_success_total: self.backend_success_total.load(Ordering::Relaxed),
            backend_failure_total: self.backend_failure_total.load(Ordering::Relaxed),
            backend_duration_ms_total: self.backend_duration_ms_total.load(Ordering::Relaxed),
            wl_copy_spawn_total: self.wl_copy_spawn_total.load(Ordering::Relaxed),
            wl_paste_spawn_total: self.wl_paste_spawn_total.load(Ordering::Relaxed),
        }
    }
}

static INJECTOR_METRICS: OnceLock<InjectorMetrics> = OnceLock::new();

fn injector_metrics() -> &'static InjectorMetrics {
    INJECTOR_METRICS.get_or_init(InjectorMetrics::default)
}

pub fn injector_metrics_snapshot() -> InjectorMetricsSnapshot {
    injector_metrics().snapshot()
}

#[derive(Debug)]
struct TimedCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

fn spawn_pipe_reader<R>(reader: R) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer)?;
        Ok(buffer)
    })
}

fn join_pipe_reader(
    handle: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    label: &str,
) -> Result<Vec<u8>> {
    let read_result = handle
        .join()
        .map_err(|_| anyhow::anyhow!("{label} reader thread panicked"))?;
    read_result.with_context(|| format!("failed to read {label} pipe"))
}

fn wait_for_child_exit(
    child: &mut Child,
    timeout: Duration,
    trace_id: u64,
    command_name: &'static str,
) -> Result<ExitStatus> {
    let started = Instant::now();
    loop {
        match child
            .try_wait()
            .with_context(|| format!("failed to query {command_name} process state"))?
        {
            Some(status) => return Ok(status),
            None if started.elapsed() >= timeout => {
                warn!(
                    trace_id,
                    command = command_name,
                    timeout_ms = timeout.as_millis() as u64,
                    "subprocess exceeded timeout; killing child"
                );
                kill_subprocess_tree(child, trace_id, command_name);
                if let Err(err) = child.wait() {
                    warn!(
                        trace_id,
                        command = command_name,
                        error = %err,
                        "failed to reap timed-out subprocess"
                    );
                }
                anyhow::bail!("{command_name} timed out after {} ms", timeout.as_millis());
            }
            None => std::thread::sleep(Duration::from_millis(INJECTOR_SUBPROCESS_POLL_INTERVAL_MS)),
        }
    }
}

fn configure_subprocess_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

fn kill_subprocess_tree(child: &mut Child, trace_id: u64, command_name: &'static str) {
    #[cfg(unix)]
    {
        let process_group_id = -(child.id() as i32);
        let rc = unsafe { libc::kill(process_group_id, libc::SIGKILL) };
        if rc == 0 {
            return;
        }

        let err = std::io::Error::last_os_error();
        warn!(
            trace_id,
            command = command_name,
            pid = child.id(),
            error = %err,
            "failed to kill timed-out subprocess tree; falling back to direct child kill"
        );
    }

    if let Err(err) = child.kill() {
        warn!(
            trace_id,
            command = command_name,
            pid = child.id(),
            error = %err,
            "failed to kill timed-out subprocess"
        );
    }
}

fn command_output_with_timeout(
    mut command: Command,
    timeout: Duration,
    trace_id: u64,
    command_name: &'static str,
) -> Result<TimedCommandOutput> {
    configure_subprocess_process_group(&mut command);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {command_name}"))?;
    let stdout_reader = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("{command_name} stdout pipe unavailable"))
        .map(spawn_pipe_reader)?;
    let stderr_reader = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("{command_name} stderr pipe unavailable"))
        .map(spawn_pipe_reader)?;
    let status = wait_for_child_exit(&mut child, timeout, trace_id, command_name);
    let stdout = join_pipe_reader(stdout_reader, "stdout");
    let stderr = join_pipe_reader(stderr_reader, "stderr");
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;

    if !stderr.is_empty() {
        debug!(
            trace_id,
            command = command_name,
            stderr = %String::from_utf8_lossy(&stderr),
            "subprocess emitted stderr"
        );
    }

    Ok(TimedCommandOutput { status, stdout })
}

pub trait TextInjector: Send + Sync {
    fn inject(&self, text: &str) -> Result<()>;

    fn inject_with_context(&self, text: &str, _context: Option<InjectorContext>) -> Result<()> {
        self.inject(text)
    }
}

#[derive(Debug, Clone)]
pub struct FailInjector {
    message: Arc<str>,
}

impl FailInjector {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: Arc::<str>::from(message.into()),
        }
    }
}

impl TextInjector for FailInjector {
    fn inject(&self, _text: &str) -> Result<()> {
        anyhow::bail!("{}", self.message)
    }
}

#[derive(Debug, Clone)]
pub struct UinputAttemptMetadata {
    pub generation: u64,
    pub fresh_device: bool,
    pub device_age_ms_at_attempt: u64,
    pub use_count_before_attempt: u64,
    pub created_this_job: bool,
    pub create_elapsed_ms: Option<u64>,
    pub last_create_error: Option<String>,
    pub reused_after_failure: bool,
}

pub trait PasteChordSender: std::fmt::Debug + Send + Sync {
    fn send_shortcut(&self, shortcut: PasteShortcut) -> Result<()>;
    fn backend_config(&self) -> Option<String>;
}

#[derive(Clone)]
pub enum PasteKeySender {
    Uinput {
        sender: Arc<dyn PasteChordSender>,
        metadata: Option<UinputAttemptMetadata>,
    },
    Disabled,
}

impl std::fmt::Debug for PasteKeySender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uinput { sender, metadata } => f
                .debug_struct("Uinput")
                .field("sender", sender)
                .field("metadata", metadata)
                .finish(),
            Self::Disabled => f.write_str("Disabled"),
        }
    }
}

pub struct UinputChordSender {
    device: Mutex<VirtualDevice>,
    dwell: Duration,
}

impl std::fmt::Debug for UinputChordSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UinputChordSender")
            .field("dwell_ms", &self.dwell_ms())
            .finish()
    }
}

impl UinputChordSender {
    pub fn new(dwell_ms: u64) -> Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        keys.insert(Key::KEY_LEFTCTRL);
        keys.insert(Key::KEY_LEFTSHIFT);
        keys.insert(Key::KEY_V);

        let device = VirtualDeviceBuilder::new()
            .context("failed to open /dev/uinput for direct keyboard injection")?
            .name("Parakeet STT Virtual Keyboard")
            .input_id(InputId::new(BusType::BUS_USB, 0x1d6b, 0x1050, 0x0001))
            .with_keys(&keys)
            .context("failed to configure uinput keyboard capabilities")?
            .build()
            .context("failed to create uinput virtual keyboard device")?;

        Ok(Self {
            device: Mutex::new(device),
            dwell: Duration::from_millis(dwell_ms.max(1)),
        })
    }

    fn shortcut_plan(shortcut: PasteShortcut) -> (&'static [Key], Key) {
        const CTRL: [Key; 1] = [Key::KEY_LEFTCTRL];
        const CTRL_SHIFT: [Key; 2] = [Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT];

        match shortcut {
            PasteShortcut::CtrlV => (&CTRL, Key::KEY_V),
            PasteShortcut::CtrlShiftV => (&CTRL_SHIFT, Key::KEY_V),
        }
    }

    fn emit_key(device: &mut VirtualDevice, key: Key, value: i32) -> Result<()> {
        device
            .emit(&[InputEvent::new(EventType::KEY, key.code(), value)])
            .with_context(|| {
                format!(
                    "failed to emit uinput event key={} value={value}",
                    key.code()
                )
            })
    }

    pub fn send_shortcut(&self, shortcut: PasteShortcut) -> Result<()> {
        let (modifiers, key) = Self::shortcut_plan(shortcut);
        let mut device = self
            .device
            .lock()
            .map_err(|_| anyhow::anyhow!("uinput virtual keyboard lock poisoned"))?;

        for modifier in modifiers {
            Self::emit_key(&mut device, *modifier, 1)?;
        }

        Self::emit_key(&mut device, key, 1)?;
        std::thread::sleep(self.dwell);
        Self::emit_key(&mut device, key, 0)?;

        for modifier in modifiers.iter().rev() {
            Self::emit_key(&mut device, *modifier, 0)?;
        }

        Ok(())
    }

    pub fn dwell_ms(&self) -> u64 {
        self.dwell.as_millis() as u64
    }
}

impl PasteChordSender for UinputChordSender {
    fn send_shortcut(&self, shortcut: PasteShortcut) -> Result<()> {
        UinputChordSender::send_shortcut(self, shortcut)
    }

    fn backend_config(&self) -> Option<String> {
        Some(format!("dwell_ms={}", self.dwell_ms()))
    }
}

#[derive(Debug, Clone)]
pub struct ClipboardInjector {
    sender: PasteKeySender,
    options: ClipboardOptions,
    copy_only: bool,
    wayland_focus_cache: Option<WaylandFocusCache>,
    context: Option<InjectorContext>,
    forced_shortcut: Option<PasteShortcut>,
}

#[derive(Debug, Clone)]
struct FocusResolutionOutcome {
    snapshot: Option<FocusSnapshot>,
    source_selected: &'static str,
    wayland_cache_age_ms: Option<u64>,
    wayland_fallback_reason: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
struct ShortcutAttemptContext {
    route_attempt_name: &'static str,
    route_attempt_index: usize,
    route_attempt_total: usize,
    backend_attempt_index: usize,
    backend_attempt_total: usize,
}

#[derive(Debug)]
struct BackendAttemptOutcome {
    status: &'static str,
    duration_ms: u64,
    exit_status: Option<String>,
    stderr_excerpt: Option<String>,
    warning_tags: Vec<String>,
    backend_config: Option<String>,
    error: Option<String>,
    uinput_metadata: Option<UinputAttemptMetadata>,
}

#[derive(Debug, Clone, Copy)]
enum InjectionOutcome {
    SuccessAssumed,
    ClipboardNotReady,
    ChordFailed,
    NoEffectSuspected,
    CopyOnly,
}

impl InjectionOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::SuccessAssumed => "success_assumed",
            Self::ClipboardNotReady => "clipboard_not_ready",
            Self::ChordFailed => "chord_failed",
            Self::NoEffectSuspected => "no_effect_suspected",
            Self::CopyOnly => "copy_only",
        }
    }
}

impl ClipboardInjector {
    const CLIPBOARD_READY_TIMEOUT_MS: u64 = 250;
    const CLIPBOARD_READY_SCHEDULE_MS: [u64; 8] = [5, 10, 15, 20, 30, 40, 50, 70];
    const WAYLAND_STALE_MS: u64 = 30_000;
    const WAYLAND_TRANSITION_GRACE_MS: u64 = 500;

    #[allow(dead_code)]
    pub fn new(sender: PasteKeySender, options: ClipboardOptions, copy_only: bool) -> Self {
        Self::new_with_context(sender, options, copy_only, None)
    }

    pub fn new_with_context(
        sender: PasteKeySender,
        options: ClipboardOptions,
        copy_only: bool,
        context: Option<InjectorContext>,
    ) -> Self {
        Self::new_with_overrides(sender, options, copy_only, context, None)
    }

    pub fn new_with_overrides(
        sender: PasteKeySender,
        options: ClipboardOptions,
        copy_only: bool,
        context: Option<InjectorContext>,
        forced_shortcut: Option<PasteShortcut>,
    ) -> Self {
        Self::new_with_shared_focus_cache(
            sender,
            options,
            copy_only,
            context,
            forced_shortcut,
            Some(WaylandFocusCache::new()),
        )
    }

    pub(crate) fn new_with_shared_focus_cache(
        sender: PasteKeySender,
        options: ClipboardOptions,
        copy_only: bool,
        context: Option<InjectorContext>,
        forced_shortcut: Option<PasteShortcut>,
        wayland_focus_cache: Option<WaylandFocusCache>,
    ) -> Self {
        Self {
            sender,
            options,
            copy_only,
            wayland_focus_cache,
            context,
            forced_shortcut,
        }
    }

    fn shortcut_name(shortcut: PasteShortcut) -> String {
        match shortcut {
            PasteShortcut::CtrlV => "CtrlV".to_string(),
            PasteShortcut::CtrlShiftV => "CtrlShiftV".to_string(),
        }
    }

    fn route_class_name(route: &crate::routing::RouteDecision) -> String {
        format!("{:?}", route.class)
    }

    fn backend_attempt_report(
        attempt: ShortcutAttemptContext,
        backend_name: &str,
        shortcut_name: &str,
        outcome: BackendAttemptOutcome,
    ) -> BackendAttemptReport {
        BackendAttemptReport {
            route_attempt_name: attempt.route_attempt_name.to_string(),
            route_attempt_index: attempt.route_attempt_index,
            route_attempt_total: attempt.route_attempt_total,
            backend: backend_name.to_string(),
            backend_attempt_index: attempt.backend_attempt_index,
            backend_attempt_total: attempt.backend_attempt_total,
            shortcut: shortcut_name.to_string(),
            status: outcome.status.to_string(),
            duration_ms: outcome.duration_ms,
            exit_status: outcome.exit_status,
            stderr_excerpt: outcome.stderr_excerpt,
            warning_tags: outcome.warning_tags,
            backend_config: outcome.backend_config,
            error: outcome.error,
            uinput_sender_generation: outcome.uinput_metadata.as_ref().map(|meta| meta.generation),
            uinput_fresh_device: outcome
                .uinput_metadata
                .as_ref()
                .map(|meta| meta.fresh_device),
            uinput_device_age_ms_at_attempt: outcome
                .uinput_metadata
                .as_ref()
                .map(|meta| meta.device_age_ms_at_attempt),
            uinput_use_count_before_attempt: outcome
                .uinput_metadata
                .as_ref()
                .map(|meta| meta.use_count_before_attempt),
            uinput_created_this_job: outcome
                .uinput_metadata
                .as_ref()
                .map(|meta| meta.created_this_job),
            uinput_create_elapsed_ms: outcome
                .uinput_metadata
                .as_ref()
                .and_then(|meta| meta.create_elapsed_ms),
            uinput_last_create_error: outcome
                .uinput_metadata
                .as_ref()
                .and_then(|meta| meta.last_create_error.clone()),
            uinput_reused_after_failure: outcome
                .uinput_metadata
                .as_ref()
                .map(|meta| meta.reused_after_failure),
        }
    }

    fn emit_report(report: &InjectorChildReport) {
        info!(
            session = ?report.session_id,
            origin = ?report.origin,
            trace_id = report.trace_id,
            outcome = report.outcome,
            route_class = report.route_class,
            route_primary = report.route_primary,
            backend_attempt_count = report.backend_attempts.len(),
            elapsed_ms_total = report.elapsed_ms_total,
            "injector report"
        );
        match serde_json::to_string(report) {
            Ok(encoded) => eprintln!("{INJECTOR_REPORT_PREFIX}{encoded}"),
            Err(err) => eprintln!(
                "{INJECTOR_REPORT_PREFIX}{{\"trace_id\":{},\"outcome\":\"report_encode_failed\",\"error\":\"{}\"}}",
                report.trace_id, err
            ),
        }
    }

    fn get_clipboard(options: &ClipboardOptions, primary: bool) -> Result<String> {
        injector_metrics()
            .wl_paste_spawn_total
            .fetch_add(1, Ordering::Relaxed);
        let mut command = Command::new("wl-paste");
        command.arg("--no-newline"); // Don't add newline if not present.
        if let Some(seat) = options.seat.as_ref() {
            command.arg("--seat").arg(seat);
        }
        if primary {
            command.arg("--primary");
        }

        let output = command_output_with_timeout(
            command,
            Duration::from_millis(INJECTOR_CHILD_COMMAND_TIMEOUT_MS),
            0,
            "wl-paste",
        )?;

        // It's okay if wl-paste fails (e.g. empty clipboard), we just return empty string.
        if !output.status.success() {
            return Ok(String::new());
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn set_clipboard(
        text: &str,
        options: &ClipboardOptions,
        foreground: bool,
        primary: bool,
    ) -> Result<Option<Child>> {
        injector_metrics()
            .wl_copy_spawn_total
            .fetch_add(1, Ordering::Relaxed);
        debug!(
            len = text.len(),
            foreground,
            primary,
            mime_type = CLIPBOARD_MIME_TYPE,
            seat = ?options.seat,
            "setting clipboard via wl-copy"
        );
        let mut command = Command::new("wl-copy");
        command
            .arg("--type")
            .arg(CLIPBOARD_MIME_TYPE)
            .stdin(Stdio::piped())
            // Internal injector subprocesses pipe stderr back to the parent
            // worker. If wl-copy helpers inherit that pipe, the worker can stay
            // blocked until some unrelated clipboard change tears the helper
            // down. Detach wl-copy stdio so clipboard ownership lifetime does
            // not masquerade as injector job lifetime.
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(seat) = options.seat.as_ref() {
            command.arg("--seat").arg(seat);
        }
        if primary {
            command.arg("--primary");
        }
        if foreground {
            command.arg("--foreground");
        }

        configure_subprocess_process_group(&mut command);
        let mut child = command.spawn().context("failed to spawn wl-copy")?;

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin
                .write_all(text.as_bytes())
                .context("failed to write to wl-copy stdin")?;
        }

        if foreground {
            debug!(primary, "wl-copy foreground source started");
            return Ok(Some(child));
        }

        // wl-copy forks a background helper by default. Piping stderr and reading
        // with wait_with_output can hang because the helper keeps the pipe open.
        let status = wait_for_child_exit(
            &mut child,
            Duration::from_millis(INJECTOR_CHILD_COMMAND_TIMEOUT_MS),
            0,
            "wl-copy",
        )
        .context("failed to wait for wl-copy")?;
        debug!(?status, primary, "wl-copy finished");
        if !status.success() {
            anyhow::bail!("wl-copy exited with status {}", status);
        }
        Ok(None)
    }

    fn write_clipboards(
        &self,
        text: &str,
        foreground: bool,
    ) -> Result<(Option<Child>, Option<Child>)> {
        let clipboard_source = Self::set_clipboard(text, &self.options, foreground, false)
            .with_context(|| {
                format!("failed to write transcript to clipboard (foreground={foreground})")
            })?;

        let primary_source = if self.options.write_primary {
            let primary_foreground = foreground && clipboard_source.is_none();
            match Self::set_clipboard(text, &self.options, primary_foreground, true) {
                Ok(source) => source,
                Err(err) => {
                    warn!(
                        error = %err,
                        "failed to write transcript to primary selection"
                    );
                    None
                }
            }
        } else {
            None
        };

        Ok((clipboard_source, primary_source))
    }

    fn wait_for_clipboard_value(
        options: &ClipboardOptions,
        expected: &str,
        timeout: Duration,
        trace_id: u64,
    ) -> (bool, Option<String>, u64) {
        let started = Instant::now();
        let mut last_observed = None;
        let mut probe_count = 0_u64;

        loop {
            probe_count += 1;
            match Self::get_clipboard(options, false) {
                Ok(value) => {
                    let matches = value == expected;
                    last_observed = Some(value);
                    if matches {
                        return (true, last_observed, probe_count);
                    }
                }
                Err(err) => {
                    debug!(
                        trace_id,
                        error = %err,
                        elapsed_ms = started.elapsed().as_millis(),
                        "clipboard read failed while waiting for requested content"
                    );
                }
            }

            if started.elapsed() >= timeout {
                return (false, last_observed, probe_count);
            }

            let sleep_ms = Self::next_clipboard_ready_sleep_ms(probe_count);
            std::thread::sleep(Duration::from_millis(sleep_ms));
        }
    }

    fn next_clipboard_ready_sleep_ms(probe_count: u64) -> u64 {
        let idx = probe_count.saturating_sub(1) as usize;
        Self::CLIPBOARD_READY_SCHEDULE_MS
            .get(idx)
            .copied()
            .unwrap_or_else(|| *Self::CLIPBOARD_READY_SCHEDULE_MS.last().unwrap_or(&70))
    }

    fn sender_name(sender: &PasteKeySender) -> &'static str {
        match sender {
            PasteKeySender::Uinput { .. } => "uinput",
            PasteKeySender::Disabled => "disabled",
        }
    }

    fn stage_start(trace_id: u64, stage: InjectionStage, detail: &str) -> Instant {
        debug!(
            trace_id,
            stage = stage.as_str(),
            status = "start",
            "{detail}"
        );
        Instant::now()
    }

    fn stage_success(trace_id: u64, stage: InjectionStage, started: Instant, detail: &str) {
        let duration_ms = started.elapsed().as_millis() as u64;
        injector_metrics().note_stage_success(stage, duration_ms);
        debug!(
            trace_id,
            stage = stage.as_str(),
            status = "ok",
            duration_ms,
            "{detail}"
        );
    }

    fn stage_failure(
        trace_id: u64,
        stage: InjectionStage,
        started: Instant,
        error: &str,
        detail: &str,
    ) {
        let duration_ms = started.elapsed().as_millis() as u64;
        injector_metrics().note_stage_failure(stage, duration_ms);
        warn!(
            trace_id,
            stage = stage.as_str(),
            status = "fail",
            duration_ms,
            error,
            "{detail}"
        );
    }

    fn run_shortcut_with_sender(
        trace_id: u64,
        shortcut: PasteShortcut,
        sender: &PasteKeySender,
        attempt: ShortcutAttemptContext,
        backend_attempts: &mut Vec<BackendAttemptReport>,
    ) -> Result<()> {
        let backend_name = Self::sender_name(sender);
        let shortcut_name = Self::shortcut_name(shortcut);
        let stage_started = Self::stage_start(
            trace_id,
            InjectionStage::Backend,
            "starting backend shortcut emission",
        );
        let backend_attempt_started = Instant::now();
        match sender {
            PasteKeySender::Uinput { sender, metadata } => {
                debug!(
                    trace_id,
                    stage = STAGE_BACKEND,
                    route_attempt_name = attempt.route_attempt_name,
                    route_attempt_index = attempt.route_attempt_index,
                    route_attempt_total = attempt.route_attempt_total,
                    backend_attempt_index = attempt.backend_attempt_index,
                    backend_attempt_total = attempt.backend_attempt_total,
                    shortcut = ?shortcut,
                    backend = "uinput",
                    backend_config = sender.backend_config().as_deref().unwrap_or("unknown"),
                    uinput_sender_generation = metadata.as_ref().map(|value| value.generation),
                    uinput_fresh_device = metadata.as_ref().map(|value| value.fresh_device),
                    uinput_device_age_ms_at_attempt = metadata
                        .as_ref()
                        .map(|value| value.device_age_ms_at_attempt),
                    uinput_use_count_before_attempt = metadata
                        .as_ref()
                        .map(|value| value.use_count_before_attempt),
                    "sending paste chord"
                );
                if let Err(err) = sender.send_shortcut(shortcut).with_context(|| {
                    format!(
                        "stage=backend failed to emit paste key chord {:?} via uinput",
                        shortcut
                    )
                }) {
                    let message = format!("{err:#}");
                    backend_attempts.push(Self::backend_attempt_report(
                        attempt,
                        backend_name,
                        &shortcut_name,
                        BackendAttemptOutcome {
                            status: "error",
                            duration_ms: backend_attempt_started.elapsed().as_millis() as u64,
                            exit_status: None,
                            stderr_excerpt: None,
                            warning_tags: Vec::new(),
                            backend_config: sender.backend_config(),
                            error: Some(message.clone()),
                            uinput_metadata: metadata.clone(),
                        },
                    ));
                    Self::stage_failure(
                        trace_id,
                        InjectionStage::Backend,
                        stage_started,
                        &message,
                        "backend shortcut emission failed",
                    );
                    return Err(err);
                }
                debug!(
                    trace_id,
                    stage = STAGE_BACKEND,
                    backend = "uinput",
                    "paste chord command finished"
                );
                Self::stage_success(
                    trace_id,
                    InjectionStage::Backend,
                    stage_started,
                    "backend shortcut emission succeeded",
                );
                backend_attempts.push(Self::backend_attempt_report(
                    attempt,
                    backend_name,
                    &shortcut_name,
                    BackendAttemptOutcome {
                        status: "ok",
                        duration_ms: backend_attempt_started.elapsed().as_millis() as u64,
                        exit_status: None,
                        stderr_excerpt: None,
                        warning_tags: Vec::new(),
                        backend_config: sender.backend_config(),
                        error: None,
                        uinput_metadata: metadata.clone(),
                    },
                ));
                Result::<()>::Ok(())
            }
            PasteKeySender::Disabled => {
                let message = "stage=backend paste key sender is disabled".to_string();
                backend_attempts.push(Self::backend_attempt_report(
                    attempt,
                    backend_name,
                    &shortcut_name,
                    BackendAttemptOutcome {
                        status: "error",
                        duration_ms: backend_attempt_started.elapsed().as_millis() as u64,
                        exit_status: None,
                        stderr_excerpt: None,
                        warning_tags: Vec::new(),
                        backend_config: None,
                        error: Some(message.clone()),
                        uinput_metadata: None,
                    },
                ));
                Self::stage_failure(
                    trace_id,
                    InjectionStage::Backend,
                    stage_started,
                    &message,
                    "backend shortcut emission failed",
                );
                anyhow::bail!("{message}")
            }
        }
        .with_context(|| {
            format!(
                "stage=backend backend={} route_attempt={}[{}/{}] backend_attempt={}/{}",
                backend_name,
                attempt.route_attempt_name,
                attempt.route_attempt_index,
                attempt.route_attempt_total,
                attempt.backend_attempt_index,
                attempt.backend_attempt_total,
            )
        })
    }

    fn run_shortcut_for_route(
        &self,
        trace_id: u64,
        route_attempt_name: &'static str,
        route_attempt_index: usize,
        route_attempt_total: usize,
        shortcut: PasteShortcut,
        backend_attempts: &mut Vec<BackendAttemptReport>,
    ) -> Result<()> {
        Self::run_shortcut_with_sender(
            trace_id,
            shortcut,
            &self.sender,
            ShortcutAttemptContext {
                route_attempt_name,
                route_attempt_index,
                route_attempt_total,
                backend_attempt_index: 1,
                backend_attempt_total: 1,
            },
            backend_attempts,
        )
    }

    #[cfg(test)]
    fn run_shortcut(&self, trace_id: u64, shortcut: PasteShortcut) -> Result<()> {
        self.run_shortcut_for_route(trace_id, "primary", 1, 1, shortcut, &mut Vec::new())
    }

    fn run_route_shortcuts(
        &self,
        trace_id: u64,
        primary: PasteShortcut,
        adaptive_fallback: Option<PasteShortcut>,
        backend_attempts: &mut Vec<BackendAttemptReport>,
    ) -> Result<()> {
        let mut attempts = vec![("primary", primary)];
        if let Some(fallback) = adaptive_fallback {
            attempts.push(("adaptive_fallback", fallback));
        }

        let stage_started = Self::stage_start(
            trace_id,
            InjectionStage::RouteShortcut,
            "starting routed shortcut stage",
        );
        let mut errors = Vec::new();
        let total = attempts.len();
        for (index, (attempt_name, shortcut)) in attempts.iter().enumerate() {
            let route_attempt_index = index + 1;
            if index > 0 {
                info!(
                    trace_id,
                    stage = STAGE_ROUTE_SHORTCUT,
                    route_attempt = *attempt_name,
                    route_attempt_index,
                    route_attempt_total = total,
                    route_shortcut = ?shortcut,
                    "attempting adaptive route fallback shortcut"
                );
            }

            match self.run_shortcut_for_route(
                trace_id,
                attempt_name,
                route_attempt_index,
                total,
                *shortcut,
                backend_attempts,
            ) {
                Ok(()) => {
                    debug!(
                        trace_id,
                        stage = STAGE_ROUTE_SHORTCUT,
                        route_attempt = *attempt_name,
                        route_attempt_index,
                        route_attempt_total = total,
                        route_shortcut = ?shortcut,
                        "route shortcut attempt succeeded"
                    );
                    Self::stage_success(
                        trace_id,
                        InjectionStage::RouteShortcut,
                        stage_started,
                        "routed shortcut stage succeeded",
                    );
                    return Ok(());
                }
                Err(err) => {
                    let err_text = format!("{err:#}");
                    warn!(
                        trace_id,
                        stage = STAGE_ROUTE_SHORTCUT,
                        route_attempt = *attempt_name,
                        route_attempt_index,
                        route_attempt_total = total,
                        route_shortcut = ?shortcut,
                        error = %err_text,
                        "route shortcut attempt failed"
                    );
                    errors.push(format!("{attempt_name}({shortcut:?}): {err_text}"));
                }
            }
        }

        let message = format!(
            "stage=route_shortcut all route shortcut attempts failed: {}",
            errors.join(" | ")
        );
        Self::stage_failure(
            trace_id,
            InjectionStage::RouteShortcut,
            stage_started,
            &message,
            "routed shortcut stage failed",
        );
        anyhow::bail!("{message}")
    }

    fn stop_foreground_source(source: &mut Option<Child>, trace_id: u64, label: &'static str) {
        let Some(mut child) = source.take() else {
            return;
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                debug!(
                    trace_id,
                    ?status,
                    source = label,
                    "wl-copy foreground source already exited"
                );
                return;
            }
            Ok(None) => {}
            Err(err) => {
                warn!(
                    trace_id,
                    error = %err,
                    source = label,
                    "failed to query wl-copy foreground source state"
                );
            }
        }

        if let Err(err) = child.kill() {
            warn!(
                trace_id,
                error = %err,
                source = label,
                "failed to stop wl-copy foreground source"
            );
        }
        if let Err(err) = child.wait() {
            warn!(
                trace_id,
                error = %err,
                source = label,
                "failed to wait for wl-copy foreground source"
            );
        } else {
            debug!(
                trace_id,
                source = label,
                "wl-copy foreground source stopped"
            );
        }
    }

    fn transfer_to_background_if_needed(
        &self,
        text: &str,
        clipboard_source: &mut Option<Child>,
        primary_source: &mut Option<Child>,
        trace_id: u64,
    ) {
        if clipboard_source.is_none() && primary_source.is_none() {
            return;
        }

        debug!(
            trace_id,
            "transferring clipboard ownership to background source"
        );
        if clipboard_source.is_some()
            && Self::set_clipboard(text, &self.options, false, false).is_err()
        {
            warn!(
                trace_id,
                "failed to transfer clipboard ownership to background source"
            );
        }
        if primary_source.is_some()
            && Self::set_clipboard(text, &self.options, false, true).is_err()
        {
            warn!(
                trace_id,
                "failed to transfer primary selection ownership to background source"
            );
        }

        Self::stop_foreground_source(clipboard_source, trace_id, "clipboard");
        Self::stop_foreground_source(primary_source, trace_id, "primary");
    }

    fn resolve_focus_metadata(&self, _trace_id: u64) -> FocusResolutionOutcome {
        let Some(cache) = self.wayland_focus_cache.as_ref() else {
            return FocusResolutionOutcome {
                snapshot: None,
                source_selected: "wayland_unavailable",
                wayland_cache_age_ms: None,
                wayland_fallback_reason: Some("wayland_cache_not_initialized"),
            };
        };
        match cache.observe(Self::WAYLAND_STALE_MS, Self::WAYLAND_TRANSITION_GRACE_MS) {
            WaylandFocusObservation::Fresh {
                snapshot,
                cache_age_ms,
            } => FocusResolutionOutcome {
                snapshot: Some(snapshot),
                source_selected: "wayland_cache",
                wayland_cache_age_ms: Some(cache_age_ms),
                wayland_fallback_reason: None,
            },
            WaylandFocusObservation::LowConfidence {
                snapshot,
                cache_age_ms,
                reason,
            } => FocusResolutionOutcome {
                snapshot: Some(snapshot),
                source_selected: "wayland_cache_low_confidence",
                wayland_cache_age_ms: Some(cache_age_ms),
                wayland_fallback_reason: Some(reason),
            },
            WaylandFocusObservation::Unavailable {
                reason,
                cache_age_ms,
            } => FocusResolutionOutcome {
                snapshot: None,
                source_selected: "wayland_unavailable",
                wayland_cache_age_ms: cache_age_ms,
                wayland_fallback_reason: Some(reason),
            },
        }
    }
}

impl TextInjector for ClipboardInjector {
    fn inject(&self, text: &str) -> Result<()> {
        let trace_id = INJECTION_TRACE_ID.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let requested_fingerprint = fingerprint(text);
        let session_id = self.context.as_ref().map(|context| context.session_id);
        let origin = self.context.as_ref().map(|context| context.origin.clone());
        let parent_focus = self
            .context
            .as_ref()
            .and_then(|context| context.parent_focus.clone());
        let mut backend_attempts = Vec::new();
        let mut child_focus_after = None;
        let mut child_focus_source_selected = "not_resolved".to_string();
        let mut child_focus_wayland_cache_age_ms = None;
        let mut child_focus_wayland_fallback_reason = None;
        let mut route_focus_source = "not_selected".to_string();
        let mut route_class = "Unknown".to_string();
        let mut route_primary = Self::shortcut_name(PasteShortcut::CtrlShiftV);
        let mut route_adaptive_fallback = Some(Self::shortcut_name(PasteShortcut::CtrlV));
        let mut route_reason = "not_routed".to_string();
        let mut post_clipboard_matches = None;
        let emit_report = |outcome: InjectionOutcome,
                           clipboard_ready: bool,
                           clipboard_probe_count: u64,
                           post_clipboard_matches: Option<bool>,
                           child_focus_before: Option<FocusSnapshot>,
                           child_focus_after: Option<FocusSnapshot>,
                           child_focus_source_selected: &str,
                           child_focus_wayland_cache_age_ms: Option<u64>,
                           child_focus_wayland_fallback_reason: Option<String>,
                           route_focus_source: &str,
                           route_class: &str,
                           route_primary: &str,
                           route_adaptive_fallback: Option<String>,
                           route_reason: &str,
                           backend_attempts: Vec<BackendAttemptReport>| {
            Self::emit_report(&InjectorChildReport {
                session_id,
                origin: origin.clone(),
                trace_id,
                outcome: outcome.as_str().to_string(),
                requested_len: text.len(),
                requested_fingerprint: requested_fingerprint.clone(),
                clipboard_ready,
                clipboard_probe_count,
                post_clipboard_matches,
                parent_focus: parent_focus.clone(),
                child_focus_before,
                child_focus_after,
                child_focus_source_selected: child_focus_source_selected.to_string(),
                child_focus_wayland_cache_age_ms,
                child_focus_wayland_fallback_reason,
                route_focus_source: route_focus_source.to_string(),
                route_class: route_class.to_string(),
                route_primary: route_primary.to_string(),
                route_adaptive_fallback,
                route_reason: route_reason.to_string(),
                backend_attempts,
                elapsed_ms_total: started.elapsed().as_millis() as u64,
            });
        };

        info!(
            trace_id,
            mode = if self.copy_only { "copy-only" } else { "paste" },
            key_backend = ?self.options.key_backend,
            post_chord_hold_ms = self.options.post_chord_hold_ms,
            seat = ?self.options.seat,
            write_primary = self.options.write_primary,
            len = text.len(),
            fingerprint = %requested_fingerprint,
            preview = %preview(text),
            "starting clipboard injection"
        );

        // 1. Save existing clipboard(s) — kept for diagnostic logging only.
        let _original_clipboard = match Self::get_clipboard(&self.options, false) {
            Ok(value) => {
                debug!(
                    trace_id,
                    elapsed_ms = started.elapsed().as_millis(),
                    len = value.len(),
                    fingerprint = %fingerprint(&value),
                    "captured existing clipboard"
                );
                Some(value)
            }
            Err(err) => {
                warn!(
                    trace_id,
                    error = %err,
                    "failed to read current clipboard before injection; restore may be skipped"
                );
                None
            }
        };

        let _original_primary = if self.options.write_primary {
            match Self::get_clipboard(&self.options, true) {
                Ok(value) => {
                    debug!(
                        trace_id,
                        elapsed_ms = started.elapsed().as_millis(),
                        len = value.len(),
                        fingerprint = %fingerprint(&value),
                        "captured existing primary selection"
                    );
                    Some(value)
                }
                Err(err) => {
                    warn!(
                        trace_id,
                        error = %err,
                        "failed to read current primary selection before injection"
                    );
                    None
                }
            }
        } else {
            None
        };

        // 2. Write transcript into clipboard (always foreground).
        debug!(
                trace_id,
                elapsed_ms = started.elapsed().as_millis(),
                requested_len = text.len(),
                requested_fingerprint = %requested_fingerprint,
                "writing transcript to clipboard"
        );
        let (mut foreground_clipboard_source, mut foreground_primary_source) = self
            .write_clipboards(text, true)
            .context("failed to set clipboard contents")?;

        // 2b. Wait briefly for wl-copy ownership to become readable.
        let clipboard_ready_started = Self::stage_start(
            trace_id,
            InjectionStage::ClipboardReady,
            "starting clipboard readiness probe",
        );
        let (ready, observed, probe_count) = Self::wait_for_clipboard_value(
            &self.options,
            text,
            Duration::from_millis(Self::CLIPBOARD_READY_TIMEOUT_MS),
            trace_id,
        );

        let mut outcome = if ready {
            Self::stage_success(
                trace_id,
                InjectionStage::ClipboardReady,
                clipboard_ready_started,
                "clipboard readiness probe succeeded",
            );
            debug!(
                trace_id,
                stage = STAGE_CLIPBOARD_READY,
                probes = probe_count,
                elapsed_ms = started.elapsed().as_millis(),
                stored_len = observed.as_ref().map_or(0, |value| value.len()),
                stored_fingerprint = %observed
                    .as_ref()
                    .map(|value| fingerprint(value))
                    .unwrap_or_else(|| "none".to_string()),
                "clipboard became ready with requested text"
            );
            InjectionOutcome::SuccessAssumed
        } else {
            let stage_error = format!(
                "clipboard did not match requested text before timeout (timeout_ms={})",
                Self::CLIPBOARD_READY_TIMEOUT_MS
            );
            Self::stage_failure(
                trace_id,
                InjectionStage::ClipboardReady,
                clipboard_ready_started,
                &stage_error,
                "clipboard readiness probe failed",
            );
            warn!(
                trace_id,
                stage = STAGE_CLIPBOARD_READY,
                probes = probe_count,
                elapsed_ms = started.elapsed().as_millis(),
                requested_len = text.len(),
                requested_fingerprint = %requested_fingerprint,
                stored_len = observed.as_ref().map_or(0, |value| value.len()),
                stored_fingerprint = %observed
                    .as_ref()
                    .map(|value| fingerprint(value))
                    .unwrap_or_else(|| "none".to_string()),
                timeout_ms = Self::CLIPBOARD_READY_TIMEOUT_MS,
                "clipboard did not match requested text before timeout; continuing in degraded mode"
            );
            InjectionOutcome::ClipboardNotReady
        };

        if self.copy_only {
            self.transfer_to_background_if_needed(
                text,
                &mut foreground_clipboard_source,
                &mut foreground_primary_source,
                trace_id,
            );
            emit_report(
                InjectionOutcome::CopyOnly,
                ready,
                probe_count,
                None,
                None,
                None,
                &child_focus_source_selected,
                child_focus_wayland_cache_age_ms,
                child_focus_wayland_fallback_reason.clone(),
                &route_focus_source,
                &route_class,
                &route_primary,
                route_adaptive_fallback.clone(),
                &route_reason,
                backend_attempts,
            );
            info!(
                trace_id,
                elapsed_ms = started.elapsed().as_millis(),
                stage = STAGE_CLIPBOARD_READY,
                outcome = InjectionOutcome::CopyOnly.as_str(),
                "clipboard copy-only injection finished"
            );
            return Ok(());
        }

        let child_focus = self.resolve_focus_metadata(trace_id);
        let child_focus_before = child_focus.snapshot.clone();
        child_focus_source_selected = child_focus.source_selected.to_string();
        child_focus_wayland_cache_age_ms = child_focus.wayland_cache_age_ms;
        child_focus_wayland_fallback_reason =
            child_focus.wayland_fallback_reason.map(str::to_string);

        let (route_primary_shortcut, route_adaptive_fallback_shortcut) =
            if let Some(forced_shortcut) = self.forced_shortcut {
                route_focus_source = "forced_shortcut".to_string();
                route_class = "Forced".to_string();
                route_primary = Self::shortcut_name(forced_shortcut);
                route_adaptive_fallback = None;
                route_reason = "forced_diagnostic_shortcut".to_string();
                info!(
                    trace_id,
                    forced_shortcut = ?forced_shortcut,
                    focus_source_selected = child_focus_source_selected,
                    focus_wayland_cache_age_ms = ?child_focus_wayland_cache_age_ms,
                    focus_wayland_fallback_reason = ?child_focus_wayland_fallback_reason,
                    "using forced diagnostic route shortcut override"
                );
                (forced_shortcut, None)
            } else {
                route_focus_source = child_focus_source_selected.clone();
                let route = decide_route(child_focus.snapshot.as_ref());
                route_class = Self::route_class_name(&route);
                route_primary = Self::shortcut_name(route.primary);
                route_adaptive_fallback = route.adaptive_fallback.map(Self::shortcut_name);
                route_reason = route.reason.to_string();

                if let Some(snapshot) = child_focus.snapshot.as_ref() {
                    info!(
                        trace_id,
                        focus_source_selected = route_focus_source,
                        focus_wayland_cache_age_ms = ?child_focus_wayland_cache_age_ms,
                        focus_wayland_fallback_reason = ?child_focus_wayland_fallback_reason,
                        resolver = snapshot.resolver,
                        focus_app = snapshot.app_name.as_deref().unwrap_or("<unknown>"),
                        focus_object = snapshot.object_name.as_deref().unwrap_or("<unknown>"),
                        focus_active = snapshot.active,
                        focus_focused = snapshot.focused,
                        route_class = ?route.class,
                        route_primary = ?route.primary,
                        route_adaptive_fallback = ?route.adaptive_fallback,
                        route_low_confidence = route.low_confidence,
                        route_reason = route.reason,
                        "resolved focused surface for adaptive routing"
                    );
                } else {
                    info!(
                        trace_id,
                        focus_source_selected = route_focus_source,
                        focus_wayland_cache_age_ms = ?child_focus_wayland_cache_age_ms,
                        focus_wayland_fallback_reason = ?child_focus_wayland_fallback_reason,
                        route_class = ?route.class,
                        route_primary = ?route.primary,
                        route_adaptive_fallback = ?route.adaptive_fallback,
                        route_low_confidence = route.low_confidence,
                        route_reason = route.reason,
                        "no focused surface metadata; using unknown routing fallback"
                    );
                }
                (route.primary, route.adaptive_fallback)
            };

        // 3. Send routed paste shortcut(s).
        if let Err(err) = self.run_route_shortcuts(
            trace_id,
            route_primary_shortcut,
            route_adaptive_fallback_shortcut,
            &mut backend_attempts,
        ) {
            outcome = InjectionOutcome::ChordFailed;
            let err_text = format!("{err:#}");
            let stage = if err_text.contains("stage=backend") {
                STAGE_BACKEND
            } else {
                STAGE_ROUTE_SHORTCUT
            };
            warn!(
                trace_id,
                stage,
                error = %err_text,
                elapsed_ms = started.elapsed().as_millis(),
                outcome = outcome.as_str(),
                "all routed paste shortcut attempts failed"
            );
            self.transfer_to_background_if_needed(
                text,
                &mut foreground_clipboard_source,
                &mut foreground_primary_source,
                trace_id,
            );
            emit_report(
                outcome,
                ready,
                probe_count,
                post_clipboard_matches,
                child_focus_before,
                child_focus_after,
                &child_focus_source_selected,
                child_focus_wayland_cache_age_ms,
                child_focus_wayland_fallback_reason.clone(),
                &route_focus_source,
                &route_class,
                &route_primary,
                route_adaptive_fallback.clone(),
                &route_reason,
                backend_attempts,
            );
            return Err(anyhow::anyhow!("{err_text}"));
        }

        if self.options.post_chord_hold_ms > 0 {
            debug!(
                trace_id,
                elapsed_ms = started.elapsed().as_millis(),
                hold_ms = self.options.post_chord_hold_ms,
                "holding foreground clipboard source after paste chord"
            );
            std::thread::sleep(Duration::from_millis(self.options.post_chord_hold_ms));
        }

        // 3b. Probe clipboard right after chord and hold.
        match Self::get_clipboard(&self.options, false) {
            Ok(value) => {
                if value != text {
                    post_clipboard_matches = Some(false);
                    warn!(
                        trace_id,
                        elapsed_ms = started.elapsed().as_millis(),
                        expected_len = text.len(),
                        expected_fingerprint = %requested_fingerprint,
                        observed_len = value.len(),
                        observed_fingerprint = %fingerprint(&value),
                        "post-paste clipboard probe differs from requested text"
                    );
                    outcome = InjectionOutcome::NoEffectSuspected;
                } else {
                    post_clipboard_matches = Some(true);
                    debug!(
                        trace_id,
                        elapsed_ms = started.elapsed().as_millis(),
                        observed_len = value.len(),
                        observed_fingerprint = %fingerprint(&value),
                        "post-paste clipboard probe matches requested text"
                    );
                }
            }
            Err(err) => {
                warn!(
                    trace_id,
                    error = %err,
                    elapsed_ms = started.elapsed().as_millis(),
                    "failed to read clipboard during post-paste probe"
                );
            }
        }

        let child_focus_after_outcome = self.resolve_focus_metadata(trace_id);
        child_focus_after = child_focus_after_outcome.snapshot.clone();

        // Restore policy is Never — transfer to background and keep transcript in clipboard.
        self.transfer_to_background_if_needed(
            text,
            &mut foreground_clipboard_source,
            &mut foreground_primary_source,
            trace_id,
        );

        info!(
            trace_id,
            elapsed_ms = started.elapsed().as_millis(),
            stage = STAGE_BACKEND,
            outcome = outcome.as_str(),
            clipboard_ready_success_total = injector_metrics()
                .clipboard_ready_success_total
                .load(Ordering::Relaxed),
            clipboard_ready_failure_total = injector_metrics()
                .clipboard_ready_failure_total
                .load(Ordering::Relaxed),
            route_shortcut_success_total = injector_metrics()
                .route_shortcut_success_total
                .load(Ordering::Relaxed),
            route_shortcut_failure_total = injector_metrics()
                .route_shortcut_failure_total
                .load(Ordering::Relaxed),
            backend_success_total = injector_metrics()
                .backend_success_total
                .load(Ordering::Relaxed),
            backend_failure_total = injector_metrics()
                .backend_failure_total
                .load(Ordering::Relaxed),
            wl_copy_spawn_total = injector_metrics()
                .wl_copy_spawn_total
                .load(Ordering::Relaxed),
            wl_paste_spawn_total = injector_metrics()
                .wl_paste_spawn_total
                .load(Ordering::Relaxed),
            "clipboard injection flow finished"
        );
        emit_report(
            outcome,
            ready,
            probe_count,
            post_clipboard_matches,
            child_focus_before,
            child_focus_after,
            &child_focus_source_selected,
            child_focus_wayland_cache_age_ms,
            child_focus_wayland_fallback_reason,
            &route_focus_source,
            &route_class,
            &route_primary,
            route_adaptive_fallback,
            &route_reason,
            backend_attempts,
        );

        Ok(())
    }

    fn inject_with_context(&self, text: &str, context: Option<InjectorContext>) -> Result<()> {
        let mut scoped = self.clone();
        scoped.context = context;
        scoped.inject(text)
    }
}

fn fingerprint(text: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn preview(text: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut chars = text.chars();
    let mut out = String::new();
    for _ in 0..MAX_CHARS {
        let Some(ch) = chars.next() else {
            return out;
        };
        out.push(ch);
    }

    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        configure_subprocess_process_group, BackendAttemptOutcome, ClipboardInjector,
        InjectorChildReport, ParentFocusCapture, PasteKeySender, ShortcutAttemptContext,
        UinputAttemptMetadata, UinputChordSender,
    };
    use crate::config::{
        ClipboardOptions, PasteBackendFailurePolicy, PasteKeyBackend, PasteShortcut,
    };
    use crate::surface_focus::FocusSnapshot;
    use evdev::Key;
    #[cfg(unix)]
    use std::io::Read;
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    use uuid::Uuid;

    fn test_options() -> ClipboardOptions {
        ClipboardOptions {
            key_backend: PasteKeyBackend::Uinput,
            backend_failure_policy: PasteBackendFailurePolicy::CopyOnly,
            post_chord_hold_ms: 700,
            seat: None,
            write_primary: false,
        }
    }

    #[test]
    fn uinput_shortcut_plan_ctrl_shift_v() {
        let (modifiers, key) = UinputChordSender::shortcut_plan(PasteShortcut::CtrlShiftV);
        assert_eq!(modifiers, [Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT]);
        assert_eq!(key, Key::KEY_V);
    }

    #[test]
    fn route_fallback_failure_reports_attempt_details() {
        let injector = ClipboardInjector {
            sender: PasteKeySender::Disabled,
            options: test_options(),
            copy_only: false,
            wayland_focus_cache: None,
            context: None,
            forced_shortcut: None,
        };

        let err = injector
            .run_route_shortcuts(
                1,
                PasteShortcut::CtrlShiftV,
                Some(PasteShortcut::CtrlV),
                &mut Vec::new(),
            )
            .expect_err("expected route fallback failure");
        let message = format!("{err:#}");
        assert!(message.contains("all route shortcut attempts failed"));
        assert!(message.contains("primary"));
        assert!(message.contains("adaptive_fallback"));
    }

    #[test]
    fn backend_failures_are_stage_tagged() {
        let injector = ClipboardInjector {
            sender: PasteKeySender::Disabled,
            options: test_options(),
            copy_only: false,
            wayland_focus_cache: None,
            context: None,
            forced_shortcut: None,
        };

        let err = injector
            .run_shortcut(1, PasteShortcut::CtrlV)
            .expect_err("disabled backend should fail");
        let message = format!("{err:#}");
        assert!(message.contains("stage=backend"));
    }

    #[test]
    fn route_failures_are_stage_tagged() {
        let injector = ClipboardInjector {
            sender: PasteKeySender::Disabled,
            options: test_options(),
            copy_only: false,
            wayland_focus_cache: None,
            context: None,
            forced_shortcut: None,
        };

        let err = injector
            .run_route_shortcuts(
                1,
                PasteShortcut::CtrlShiftV,
                Some(PasteShortcut::CtrlV),
                &mut Vec::new(),
            )
            .expect_err("disabled backend should fail route stage");
        let message = format!("{err:#}");
        assert!(message.contains("stage=route_shortcut"));
    }

    #[test]
    fn clipboard_ready_schedule_uses_progressive_backoff() {
        assert_eq!(ClipboardInjector::next_clipboard_ready_sleep_ms(1), 5);
        assert_eq!(ClipboardInjector::next_clipboard_ready_sleep_ms(2), 10);
        assert_eq!(ClipboardInjector::next_clipboard_ready_sleep_ms(3), 15);
        assert_eq!(ClipboardInjector::next_clipboard_ready_sleep_ms(4), 20);
        assert_eq!(ClipboardInjector::next_clipboard_ready_sleep_ms(7), 50);
        assert_eq!(ClipboardInjector::next_clipboard_ready_sleep_ms(8), 70);
        assert_eq!(ClipboardInjector::next_clipboard_ready_sleep_ms(9), 70);
        assert_eq!(ClipboardInjector::next_clipboard_ready_sleep_ms(128), 70);
    }

    #[cfg(unix)]
    #[test]
    fn configure_subprocess_process_group_moves_child_into_own_group() {
        let mut command = Command::new("bash");
        command.arg("-lc").arg(
            r#"pid=$$; pgid=$(ps -o pgid= -p "$pid" | tr -d ' '); printf '%s %s\n' "$pid" "$pgid""#,
        );
        configure_subprocess_process_group(&mut command);
        let mut child = command
            .stdout(Stdio::piped())
            .spawn()
            .expect("test helper should spawn");
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .expect("stdout pipe should exist")
            .read_to_string(&mut stdout)
            .expect("stdout should be readable");
        let status = child.wait().expect("child should exit");
        assert!(status.success(), "helper should exit successfully");

        let mut parts = stdout.split_whitespace();
        let pid = parts.next().expect("helper should report pid");
        let pgid = parts.next().expect("helper should report pgid");
        assert_eq!(
            parts.next(),
            None,
            "helper output should contain two fields"
        );
        assert_eq!(
            pid, pgid,
            "child should become leader of its own process group"
        );
    }

    fn test_focus_snapshot(resolver: &str) -> FocusSnapshot {
        FocusSnapshot {
            app_name: Some("Ghostty".to_string()),
            object_name: Some("terminal".to_string()),
            object_path: Some("/com/example/terminal".to_string()),
            service_name: Some("wayland".to_string()),
            output_name: Some("DP-1".to_string()),
            focused: true,
            active: true,
            resolver: resolver.to_string(),
        }
    }

    #[test]
    fn parent_focus_capture_round_trips_resolver_through_serde() {
        let parent_focus = ParentFocusCapture {
            snapshot: Some(test_focus_snapshot("wayland")),
            source_selected: "wayland".to_string(),
            wayland_cache_age_ms: Some(42),
            wayland_fallback_reason: None,
            captured_elapsed_ms: Some(7),
        };

        let encoded = serde_json::to_string(&parent_focus).expect("parent focus should serialize");
        let decoded: ParentFocusCapture =
            serde_json::from_str(&encoded).expect("parent focus should deserialize");

        assert_eq!(
            decoded
                .snapshot
                .as_ref()
                .expect("snapshot should round-trip")
                .resolver,
            "wayland"
        );
    }

    #[test]
    fn injector_child_report_round_trips_resolver_through_serde() {
        let report = InjectorChildReport {
            session_id: Some(Uuid::nil()),
            origin: Some("test".to_string()),
            trace_id: 9,
            outcome: "success".to_string(),
            requested_len: 12,
            requested_fingerprint: "abc123".to_string(),
            clipboard_ready: true,
            clipboard_probe_count: 2,
            post_clipboard_matches: Some(true),
            parent_focus: Some(ParentFocusCapture {
                snapshot: Some(test_focus_snapshot("parent")),
                source_selected: "wayland".to_string(),
                wayland_cache_age_ms: Some(3),
                wayland_fallback_reason: None,
                captured_elapsed_ms: Some(4),
            }),
            child_focus_before: Some(test_focus_snapshot("before")),
            child_focus_after: Some(test_focus_snapshot("after")),
            child_focus_source_selected: "wayland".to_string(),
            child_focus_wayland_cache_age_ms: Some(5),
            child_focus_wayland_fallback_reason: None,
            route_focus_source: "wayland".to_string(),
            route_class: "Terminal".to_string(),
            route_primary: "CtrlShiftV".to_string(),
            route_adaptive_fallback: Some("CtrlV".to_string()),
            route_reason: "focused terminal".to_string(),
            backend_attempts: Vec::new(),
            elapsed_ms_total: 25,
        };

        let encoded = serde_json::to_string(&report).expect("report should serialize");
        let decoded: InjectorChildReport =
            serde_json::from_str(&encoded).expect("report should deserialize");

        assert_eq!(
            decoded
                .parent_focus
                .as_ref()
                .and_then(|focus| focus.snapshot.as_ref())
                .expect("parent snapshot should round-trip")
                .resolver,
            "parent"
        );
        assert_eq!(
            decoded
                .child_focus_before
                .as_ref()
                .expect("child focus before should round-trip")
                .resolver,
            "before"
        );
        assert_eq!(
            decoded
                .child_focus_after
                .as_ref()
                .expect("child focus after should round-trip")
                .resolver,
            "after"
        );
    }

    #[test]
    fn backend_attempt_report_includes_uinput_lifecycle_metadata() {
        let report = ClipboardInjector::backend_attempt_report(
            ShortcutAttemptContext {
                route_attempt_name: "primary",
                route_attempt_index: 1,
                route_attempt_total: 1,
                backend_attempt_index: 1,
                backend_attempt_total: 1,
            },
            "uinput",
            "CtrlShiftV",
            BackendAttemptOutcome {
                status: "ok",
                duration_ms: 12,
                exit_status: None,
                stderr_excerpt: None,
                warning_tags: Vec::new(),
                backend_config: Some("dwell_ms=18".to_string()),
                error: None,
                uinput_metadata: Some(UinputAttemptMetadata {
                    generation: 4,
                    fresh_device: true,
                    device_age_ms_at_attempt: 203,
                    use_count_before_attempt: 0,
                    created_this_job: true,
                    create_elapsed_ms: Some(7),
                    last_create_error: None,
                    reused_after_failure: true,
                }),
            },
        );

        assert_eq!(report.uinput_sender_generation, Some(4));
        assert_eq!(report.uinput_fresh_device, Some(true));
        assert_eq!(report.uinput_device_age_ms_at_attempt, Some(203));
        assert_eq!(report.uinput_use_count_before_attempt, Some(0));
        assert_eq!(report.uinput_created_this_job, Some(true));
        assert_eq!(report.uinput_create_elapsed_ms, Some(7));
        assert_eq!(report.uinput_last_create_error, None);
        assert_eq!(report.uinput_reused_after_failure, Some(true));
    }
}
