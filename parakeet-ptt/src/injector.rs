use std::hash::{Hash, Hasher};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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
// The parent worker waits slightly longer than individual helper commands so it
// can reap the subprocess tree and drain stderr without timing out first.
#[cfg(not(test))]
pub(crate) const INJECTOR_CHILD_COMMAND_TIMEOUT_MS: u64 = 1_000;
#[cfg(test)]
pub(crate) const INJECTOR_CHILD_COMMAND_TIMEOUT_MS: u64 = 150;
#[cfg(not(test))]
pub(crate) const INJECTOR_JOB_TIMEOUT_SLACK_MS: u64 = 500;
#[cfg(test)]
pub(crate) const INJECTOR_JOB_TIMEOUT_SLACK_MS: u64 = 0;
pub(crate) const INJECTOR_JOB_TIMEOUT_MS: u64 =
    INJECTOR_CHILD_COMMAND_TIMEOUT_MS + INJECTOR_JOB_TIMEOUT_SLACK_MS;
pub(crate) const INJECTOR_SUBPROCESS_POLL_INTERVAL_MS: u64 = 5;
pub(crate) const INJECTOR_PIPE_DRAIN_TIMEOUT_MS: u64 = 50;
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
    stderr: Vec<u8>,
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

    Ok(TimedCommandOutput {
        status,
        stdout,
        stderr,
    })
}

pub trait TextInjector: Send + Sync {
    fn inject(&self, text: &str) -> Result<()>;
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
pub enum PasteKeySender {
    Ydotool(PathBuf),
    Uinput(Arc<UinputChordSender>),
    Chain(Vec<PasteKeySender>),
    Disabled,
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
    const BACKEND_STDERR_EXCERPT_MAX_CHARS: usize = 240;
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
        Self {
            sender,
            options,
            copy_only,
            wayland_focus_cache: Some(WaylandFocusCache::new()),
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

    fn stderr_excerpt(stderr: &[u8]) -> Option<String> {
        let trimmed = String::from_utf8_lossy(stderr).trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        let mut excerpt = String::new();
        for ch in trimmed.chars().take(Self::BACKEND_STDERR_EXCERPT_MAX_CHARS) {
            excerpt.push(ch);
        }
        if trimmed.chars().count() > Self::BACKEND_STDERR_EXCERPT_MAX_CHARS {
            excerpt.push_str("...");
        }
        Some(excerpt)
    }

    fn classify_warning_tags(stderr_excerpt: Option<&str>) -> Vec<String> {
        let Some(stderr) = stderr_excerpt else {
            return Vec::new();
        };
        let mut tags = Vec::new();
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("ydotoold backend unavailable") {
            tags.push("ydotool_backend_unavailable".to_string());
        }
        if lower.contains("latency+delay") {
            tags.push("ydotool_latency_delay_warning".to_string());
        }
        tags
    }

    fn backend_attempt_report(
        attempt: ShortcutAttemptContext,
        backend_name: &str,
        shortcut_name: &str,
        status: &str,
        duration_ms: u64,
        exit_status: Option<String>,
        stderr_excerpt: Option<String>,
        warning_tags: Vec<String>,
        backend_config: Option<String>,
        error: Option<String>,
    ) -> BackendAttemptReport {
        BackendAttemptReport {
            route_attempt_name: attempt.route_attempt_name.to_string(),
            route_attempt_index: attempt.route_attempt_index,
            route_attempt_total: attempt.route_attempt_total,
            backend: backend_name.to_string(),
            backend_attempt_index: attempt.backend_attempt_index,
            backend_attempt_total: attempt.backend_attempt_total,
            shortcut: shortcut_name.to_string(),
            status: status.to_string(),
            duration_ms,
            exit_status,
            stderr_excerpt,
            warning_tags,
            backend_config,
            error,
        }
    }

    fn emit_report(report: &InjectorChildReport) {
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

    fn ydotool_shortcut_args(shortcut: PasteShortcut) -> &'static [&'static str] {
        match shortcut {
            PasteShortcut::CtrlV => &["29:1", "47:1", "47:0", "29:0"],
            PasteShortcut::CtrlShiftV => &["29:1", "42:1", "47:1", "47:0", "42:0", "29:0"],
        }
    }

    fn sender_name(sender: &PasteKeySender) -> &'static str {
        match sender {
            PasteKeySender::Ydotool(_) => "ydotool",
            PasteKeySender::Uinput(_) => "uinput",
            PasteKeySender::Chain(_) => "chain",
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
            PasteKeySender::Ydotool(binary) => {
                debug!(
                    trace_id,
                    stage = STAGE_BACKEND,
                    route_attempt_name = attempt.route_attempt_name,
                    route_attempt_index = attempt.route_attempt_index,
                    route_attempt_total = attempt.route_attempt_total,
                    backend_attempt_index = attempt.backend_attempt_index,
                    backend_attempt_total = attempt.backend_attempt_total,
                    shortcut = ?shortcut,
                    backend = "ydotool",
                    binary = %binary.display(),
                    args = ?Self::ydotool_shortcut_args(shortcut),
                    "sending paste chord"
                );
                let mut command = Command::new(binary);
                command
                    .arg("key")
                    .args(Self::ydotool_shortcut_args(shortcut));
                let output = command_output_with_timeout(
                    command,
                    Duration::from_millis(INJECTOR_CHILD_COMMAND_TIMEOUT_MS),
                    trace_id,
                    "ydotool",
                )
                .context("stage=backend failed to run ydotool for paste chord");
                let output = match output {
                    Ok(output) => output,
                    Err(err) => {
                        let message = format!("{err:#}");
                        backend_attempts.push(Self::backend_attempt_report(
                            attempt,
                            backend_name,
                            &shortcut_name,
                            "error",
                            backend_attempt_started.elapsed().as_millis() as u64,
                            None,
                            None,
                            Vec::new(),
                            Some(format!("binary={}", binary.display())),
                            Some(message.clone()),
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
                };

                let stderr_excerpt = Self::stderr_excerpt(&output.stderr);
                let warning_tags = Self::classify_warning_tags(stderr_excerpt.as_deref());
                if !warning_tags.is_empty() {
                    warn!(
                        trace_id,
                        stage = STAGE_BACKEND,
                        backend = "ydotool",
                        warning_tags = ?warning_tags,
                        stderr_excerpt = ?stderr_excerpt,
                        "ydotool backend emitted warning stderr"
                    );
                }

                debug!(
                    trace_id,
                    stage = STAGE_BACKEND,
                    route_attempt_name = attempt.route_attempt_name,
                    route_attempt_index = attempt.route_attempt_index,
                    route_attempt_total = attempt.route_attempt_total,
                    backend_attempt_index = attempt.backend_attempt_index,
                    backend_attempt_total = attempt.backend_attempt_total,
                    status = ?output.status,
                    backend = "ydotool",
                    stderr_excerpt = ?stderr_excerpt,
                    "paste chord command finished"
                );
                if !output.status.success() {
                    let mut message = format!(
                        "stage=backend paste key chord {:?} via ydotool exited with status {}",
                        shortcut, output.status
                    );
                    if let Some(stderr_excerpt) = stderr_excerpt.as_ref() {
                        message.push_str(&format!(": stderr={stderr_excerpt}"));
                    }
                    backend_attempts.push(Self::backend_attempt_report(
                        attempt,
                        backend_name,
                        &shortcut_name,
                        "nonzero_exit",
                        backend_attempt_started.elapsed().as_millis() as u64,
                        Some(output.status.to_string()),
                        stderr_excerpt,
                        warning_tags,
                        Some(format!("binary={}", binary.display())),
                        Some(message.clone()),
                    ));
                    Self::stage_failure(
                        trace_id,
                        InjectionStage::Backend,
                        stage_started,
                        &message,
                        "backend shortcut emission failed",
                    );
                    anyhow::bail!("{message}");
                }
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
                    "ok",
                    backend_attempt_started.elapsed().as_millis() as u64,
                    Some(output.status.to_string()),
                    stderr_excerpt,
                    warning_tags,
                    Some(format!("binary={}", binary.display())),
                    None,
                ));
                Result::<()>::Ok(())
            }
            PasteKeySender::Uinput(sender) => {
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
                    dwell_ms = sender.dwell_ms(),
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
                        "error",
                        backend_attempt_started.elapsed().as_millis() as u64,
                        None,
                        None,
                        Vec::new(),
                        Some(format!("dwell_ms={}", sender.dwell_ms())),
                        Some(message.clone()),
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
                    "ok",
                    backend_attempt_started.elapsed().as_millis() as u64,
                    None,
                    None,
                    Vec::new(),
                    Some(format!("dwell_ms={}", sender.dwell_ms())),
                    None,
                ));
                Result::<()>::Ok(())
            }
            PasteKeySender::Chain(_) => {
                let message = "stage=backend nested sender chain is not supported".to_string();
                backend_attempts.push(Self::backend_attempt_report(
                    attempt,
                    backend_name,
                    &shortcut_name,
                    "error",
                    backend_attempt_started.elapsed().as_millis() as u64,
                    None,
                    None,
                    Vec::new(),
                    None,
                    Some(message.clone()),
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
            PasteKeySender::Disabled => {
                let message = "stage=backend paste key sender is disabled".to_string();
                backend_attempts.push(Self::backend_attempt_report(
                    attempt,
                    backend_name,
                    &shortcut_name,
                    "error",
                    backend_attempt_started.elapsed().as_millis() as u64,
                    None,
                    None,
                    Vec::new(),
                    None,
                    Some(message.clone()),
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
        match &self.sender {
            PasteKeySender::Chain(backends) => {
                let mut errors = Vec::new();
                for (idx, backend) in backends.iter().enumerate() {
                    let backend_attempt_index = idx + 1;
                    let backend_attempt_total = backends.len();
                    let attempt = ShortcutAttemptContext {
                        route_attempt_name,
                        route_attempt_index,
                        route_attempt_total,
                        backend_attempt_index,
                        backend_attempt_total,
                    };
                    if backend_attempt_index > 1 {
                        info!(
                            trace_id,
                            stage = STAGE_BACKEND,
                            route_attempt_name,
                            route_attempt_index,
                            route_attempt_total,
                            route_shortcut = ?shortcut,
                            backend = Self::sender_name(backend),
                            backend_attempt_index,
                            backend_attempt_total,
                            "attempting paste backend fallback"
                        );
                    }
                    match Self::run_shortcut_with_sender(
                        trace_id,
                        shortcut,
                        backend,
                        attempt,
                        backend_attempts,
                    ) {
                        Ok(()) => return Ok(()),
                        Err(err) => {
                            let err_text = format!("{err:#}");
                            warn!(
                                trace_id,
                                stage = STAGE_BACKEND,
                                route_attempt_name,
                                route_attempt_index,
                                route_attempt_total,
                                route_shortcut = ?shortcut,
                                backend = Self::sender_name(backend),
                                backend_attempt_index,
                                backend_attempt_total,
                                error = %err_text,
                                "paste backend attempt failed"
                            );
                            errors.push(format!("{}: {}", Self::sender_name(backend), err_text));
                        }
                    }
                }

                anyhow::bail!(
                    "stage=backend all paste backend attempts failed for shortcut {:?}: {}",
                    shortcut,
                    errors.join(" | ")
                )
            }
            sender => Self::run_shortcut_with_sender(
                trace_id,
                shortcut,
                sender,
                ShortcutAttemptContext {
                    route_attempt_name,
                    route_attempt_index,
                    route_attempt_total,
                    backend_attempt_index: 1,
                    backend_attempt_total: 1,
                },
                backend_attempts,
            ),
        }
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
        let child_focus_before;
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
        child_focus_before = child_focus.snapshot.clone();
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
    use super::{injector_metrics_snapshot, ClipboardInjector, PasteKeySender, UinputChordSender};
    use crate::config::{
        ClipboardOptions, PasteBackendFailurePolicy, PasteKeyBackend, PasteShortcut,
    };
    use evdev::Key;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn test_options() -> ClipboardOptions {
        ClipboardOptions {
            key_backend: PasteKeyBackend::Auto,
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
    fn chain_sender_falls_through_to_next_backend() {
        let injector = ClipboardInjector {
            sender: PasteKeySender::Chain(vec![
                PasteKeySender::Ydotool(PathBuf::from("/bin/false")),
                PasteKeySender::Ydotool(PathBuf::from("/bin/true")),
            ]),
            options: test_options(),
            copy_only: false,
            wayland_focus_cache: None,
            context: None,
            forced_shortcut: None,
        };

        // ydotool /bin/true with "key" arg should succeed (it's just /bin/true ignoring args)
        assert!(injector.run_shortcut(1, PasteShortcut::CtrlV).is_ok());
    }

    #[test]
    fn chain_sender_reports_all_backend_failures() {
        let injector = ClipboardInjector {
            sender: PasteKeySender::Chain(vec![
                PasteKeySender::Disabled,
                PasteKeySender::Ydotool(PathBuf::from("/bin/false")),
            ]),
            options: test_options(),
            copy_only: false,
            wayland_focus_cache: None,
            context: None,
            forced_shortcut: None,
        };

        let err = injector
            .run_shortcut(1, PasteShortcut::CtrlV)
            .expect_err("expected chain failure");
        let message = format!("{err:#}");
        assert!(message.contains("all paste backend attempts failed"));
        assert!(message.contains("disabled"));
        assert!(message.contains("ydotool"));
    }

    fn make_test_ydotool(content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "parakeet-ptt-injector-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        fs::write(&path, content).expect("test helper script should be writable");
        let mut perms = fs::metadata(&path)
            .expect("test helper script should exist")
            .permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&path, perms).expect("test helper script should be executable");
        path
    }

    #[test]
    fn route_fallback_attempt_uses_adaptive_shortcut_after_primary_failure() {
        let script = make_test_ydotool(
            "#!/usr/bin/env bash\nif [ \"$#\" -eq 7 ]; then exit 1; fi\nexit 0\n",
        );
        let injector = ClipboardInjector {
            sender: PasteKeySender::Ydotool(script.clone()),
            options: test_options(),
            copy_only: false,
            wayland_focus_cache: None,
            context: None,
            forced_shortcut: None,
        };

        let result = injector.run_route_shortcuts(
            1,
            PasteShortcut::CtrlShiftV,
            Some(PasteShortcut::CtrlV),
            &mut Vec::new(),
        );
        fs::remove_file(&script).expect("test helper script should be removable");

        assert!(result.is_ok());
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
    fn ydotool_spawn_failures_are_counted_as_backend_stage_failures() {
        let missing_binary = std::env::temp_dir().join(format!(
            "parakeet-ptt-missing-ydotool-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        let injector = ClipboardInjector {
            sender: PasteKeySender::Ydotool(missing_binary),
            options: test_options(),
            copy_only: false,
            wayland_focus_cache: None,
            context: None,
            forced_shortcut: None,
        };
        let attempts = 12;
        let baseline = injector_metrics_snapshot().backend_failure_total;

        for trace_id in 1..=attempts {
            let err = injector
                .run_shortcut(trace_id, PasteShortcut::CtrlV)
                .expect_err("missing ydotool binary should fail to spawn");
            let message = format!("{err:#}");
            assert!(message.contains("failed to spawn ydotool"));
            assert!(message.contains("stage=backend"));
        }

        let observed_delta = injector_metrics_snapshot()
            .backend_failure_total
            .saturating_sub(baseline);
        assert!(
            observed_delta >= attempts,
            "expected at least {attempts} backend failures recorded, observed {observed_delta}",
        );
    }

    #[test]
    fn timed_out_ydotool_backend_fails_fast_and_chain_recovers() {
        let hanging_script = make_test_ydotool("#!/usr/bin/env bash\nsleep 5\n");
        let injector = ClipboardInjector {
            sender: PasteKeySender::Chain(vec![
                PasteKeySender::Ydotool(hanging_script.clone()),
                PasteKeySender::Ydotool(PathBuf::from("/bin/true")),
            ]),
            options: test_options(),
            copy_only: false,
            wayland_focus_cache: None,
            context: None,
            forced_shortcut: None,
        };

        let started = Instant::now();
        let result = injector.run_shortcut(1, PasteShortcut::CtrlV);
        fs::remove_file(&hanging_script).expect("test helper script should be removable");

        assert!(result.is_ok(), "chain backend should recover after timeout");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timed out backend should be killed promptly"
        );
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
}
