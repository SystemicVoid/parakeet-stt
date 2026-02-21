use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId, Key};
use tracing::{debug, info, warn};

use crate::config::{ClipboardOptions, PasteShortcut};
use crate::routing::decide_route;
use crate::surface_focus::{WaylandFocusCache, WaylandFocusObservation};

static INJECTION_TRACE_ID: AtomicU64 = AtomicU64::new(1);

/// MIME type used for all wl-copy clipboard writes.
const CLIPBOARD_MIME_TYPE: &str = "text/plain;charset=utf-8";

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
}

#[derive(Debug, Clone)]
struct FocusResolutionOutcome {
    snapshot: Option<crate::surface_focus::FocusSnapshot>,
    source_selected: &'static str,
    wayland_cache_age_ms: Option<u64>,
    wayland_fallback_reason: Option<&'static str>,
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
    const CLIPBOARD_READY_POLL_MS: u64 = 10;
    const WAYLAND_STALE_MS: u64 = 30_000;
    const WAYLAND_TRANSITION_GRACE_MS: u64 = 500;

    pub fn new(sender: PasteKeySender, options: ClipboardOptions, copy_only: bool) -> Self {
        Self {
            sender,
            options,
            copy_only,
            wayland_focus_cache: Some(WaylandFocusCache::new()),
        }
    }

    fn get_clipboard(options: &ClipboardOptions, primary: bool) -> Result<String> {
        let mut command = Command::new("wl-paste");
        command.arg("--no-newline"); // Don't add newline if not present.
        if let Some(seat) = options.seat.as_ref() {
            command.arg("--seat").arg(seat);
        }
        if primary {
            command.arg("--primary");
        }

        let output = command.output().context("failed to spawn wl-paste")?;

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
            .stdin(Stdio::piped());
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
        let status = child.wait().context("failed to wait for wl-copy")?;
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
        poll: Duration,
        trace_id: u64,
    ) -> (bool, Option<String>) {
        let started = Instant::now();
        let mut last_observed = None;

        loop {
            match Self::get_clipboard(options, false) {
                Ok(value) => {
                    let matches = value == expected;
                    last_observed = Some(value);
                    if matches {
                        return (true, last_observed);
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
                return (false, last_observed);
            }

            std::thread::sleep(poll);
        }
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

    fn run_shortcut_with_sender(
        trace_id: u64,
        shortcut: PasteShortcut,
        sender: &PasteKeySender,
    ) -> Result<()> {
        match sender {
            PasteKeySender::Ydotool(binary) => {
                debug!(
                    trace_id,
                    shortcut = ?shortcut,
                    backend = "ydotool",
                    binary = %binary.display(),
                    args = ?Self::ydotool_shortcut_args(shortcut),
                    "sending paste chord"
                );
                let status = Command::new(binary)
                    .arg("key")
                    .args(Self::ydotool_shortcut_args(shortcut))
                    .status()
                    .context("failed to spawn ydotool for paste chord")?;

                debug!(
                    trace_id,
                    ?status,
                    backend = "ydotool",
                    "paste chord command finished"
                );
                if !status.success() {
                    anyhow::bail!(
                        "paste key chord {:?} via ydotool exited with status {}",
                        shortcut,
                        status
                    );
                }
                Ok(())
            }
            PasteKeySender::Uinput(sender) => {
                debug!(
                    trace_id,
                    shortcut = ?shortcut,
                    backend = "uinput",
                    dwell_ms = sender.dwell_ms(),
                    "sending paste chord"
                );
                sender.send_shortcut(shortcut).with_context(|| {
                    format!("failed to emit paste key chord {:?} via uinput", shortcut)
                })?;
                debug!(trace_id, backend = "uinput", "paste chord command finished");
                Ok(())
            }
            PasteKeySender::Chain(_) => anyhow::bail!("nested sender chain is not supported"),
            PasteKeySender::Disabled => anyhow::bail!("paste key sender is disabled"),
        }
    }

    fn run_shortcut(&self, trace_id: u64, shortcut: PasteShortcut) -> Result<()> {
        match &self.sender {
            PasteKeySender::Chain(backends) => {
                let mut errors = Vec::new();
                for (idx, backend) in backends.iter().enumerate() {
                    if idx > 0 {
                        info!(
                            trace_id,
                            shortcut = ?shortcut,
                            backend = Self::sender_name(backend),
                            attempt = idx + 1,
                            total_attempts = backends.len(),
                            "attempting paste backend fallback"
                        );
                    }
                    match Self::run_shortcut_with_sender(trace_id, shortcut, backend) {
                        Ok(()) => return Ok(()),
                        Err(err) => {
                            warn!(
                                trace_id,
                                shortcut = ?shortcut,
                                backend = Self::sender_name(backend),
                                attempt = idx + 1,
                                total_attempts = backends.len(),
                                error = %err,
                                "paste backend attempt failed"
                            );
                            errors.push(format!("{}: {}", Self::sender_name(backend), err));
                        }
                    }
                }

                anyhow::bail!(
                    "all paste backend attempts failed for shortcut {:?}: {}",
                    shortcut,
                    errors.join(" | ")
                )
            }
            sender => Self::run_shortcut_with_sender(trace_id, shortcut, sender),
        }
    }

    fn run_route_shortcuts(
        &self,
        trace_id: u64,
        primary: PasteShortcut,
        adaptive_fallback: Option<PasteShortcut>,
    ) -> Result<()> {
        let mut attempts = vec![("primary", primary)];
        if let Some(fallback) = adaptive_fallback {
            attempts.push(("adaptive_fallback", fallback));
        }

        let mut errors = Vec::new();
        for (index, (attempt_name, shortcut)) in attempts.iter().enumerate() {
            if index > 0 {
                info!(
                    trace_id,
                    route_attempt = *attempt_name,
                    route_shortcut = ?shortcut,
                    "attempting adaptive route fallback shortcut"
                );
            }

            match self.run_shortcut(trace_id, *shortcut) {
                Ok(()) => {
                    debug!(
                        trace_id,
                        route_attempt = *attempt_name,
                        route_shortcut = ?shortcut,
                        "route shortcut attempt succeeded"
                    );
                    return Ok(());
                }
                Err(err) => {
                    warn!(
                        trace_id,
                        route_attempt = *attempt_name,
                        route_shortcut = ?shortcut,
                        error = %err,
                        "route shortcut attempt failed"
                    );
                    errors.push(format!("{attempt_name}({shortcut:?}): {err}"));
                }
            }
        }

        anyhow::bail!("all route shortcut attempts failed: {}", errors.join(" | "))
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

        info!(
            trace_id,
            mode = if self.copy_only { "copy-only" } else { "paste" },
            key_backend = ?self.options.key_backend,
            post_chord_hold_ms = self.options.post_chord_hold_ms,
            seat = ?self.options.seat,
            write_primary = self.options.write_primary,
            len = text.len(),
            fingerprint = %fingerprint(text),
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
            requested_fingerprint = %fingerprint(text),
            "writing transcript to clipboard"
        );
        let (mut foreground_clipboard_source, mut foreground_primary_source) = self
            .write_clipboards(text, true)
            .context("failed to set clipboard contents")?;

        // 2b. Wait briefly for wl-copy ownership to become readable.
        let (ready, observed) = Self::wait_for_clipboard_value(
            &self.options,
            text,
            Duration::from_millis(Self::CLIPBOARD_READY_TIMEOUT_MS),
            Duration::from_millis(Self::CLIPBOARD_READY_POLL_MS),
            trace_id,
        );

        let mut outcome = if ready {
            debug!(
                trace_id,
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
            warn!(
                trace_id,
                elapsed_ms = started.elapsed().as_millis(),
                requested_len = text.len(),
                requested_fingerprint = %fingerprint(text),
                stored_len = observed.as_ref().map_or(0, |value| value.len()),
                stored_fingerprint = %observed
                    .as_ref()
                    .map(|value| fingerprint(value))
                    .unwrap_or_else(|| "none".to_string()),
                timeout_ms = Self::CLIPBOARD_READY_TIMEOUT_MS,
                "clipboard did not match requested text before timeout; continuing"
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
            info!(
                trace_id,
                elapsed_ms = started.elapsed().as_millis(),
                outcome = InjectionOutcome::CopyOnly.as_str(),
                "clipboard copy-only injection finished"
            );
            return Ok(());
        }

        let FocusResolutionOutcome {
            snapshot: focus_snapshot,
            source_selected,
            wayland_cache_age_ms,
            wayland_fallback_reason,
        } = self.resolve_focus_metadata(trace_id);

        let route = decide_route(focus_snapshot.as_ref());
        if let Some(snapshot) = focus_snapshot.as_ref() {
            info!(
                trace_id,
                focus_source_selected = source_selected,
                focus_wayland_cache_age_ms = ?wayland_cache_age_ms,
                focus_wayland_fallback_reason = ?wayland_fallback_reason,
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
                focus_source_selected = source_selected,
                focus_wayland_cache_age_ms = ?wayland_cache_age_ms,
                focus_wayland_fallback_reason = ?wayland_fallback_reason,
                route_class = ?route.class,
                route_primary = ?route.primary,
                route_adaptive_fallback = ?route.adaptive_fallback,
                route_low_confidence = route.low_confidence,
                route_reason = route.reason,
                "no focused surface metadata; using unknown routing fallback"
            );
        }

        // 3. Send routed paste shortcut(s).
        if let Err(err) = self.run_route_shortcuts(trace_id, route.primary, route.adaptive_fallback)
        {
            outcome = InjectionOutcome::ChordFailed;
            warn!(
                trace_id,
                error = %err,
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
            return Err(err);
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
                    warn!(
                        trace_id,
                        elapsed_ms = started.elapsed().as_millis(),
                        expected_len = text.len(),
                        expected_fingerprint = %fingerprint(text),
                        observed_len = value.len(),
                        observed_fingerprint = %fingerprint(&value),
                        "post-paste clipboard probe differs from requested text"
                    );
                    outcome = InjectionOutcome::NoEffectSuspected;
                } else {
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
            outcome = outcome.as_str(),
            "clipboard injection flow finished"
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
    use super::{ClipboardInjector, PasteKeySender, UinputChordSender};
    use crate::config::{
        ClipboardOptions, PasteBackendFailurePolicy, PasteKeyBackend, PasteShortcut,
    };
    use evdev::Key;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

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
        };

        let result =
            injector.run_route_shortcuts(1, PasteShortcut::CtrlShiftV, Some(PasteShortcut::CtrlV));
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
        };

        let err = injector
            .run_route_shortcuts(1, PasteShortcut::CtrlShiftV, Some(PasteShortcut::CtrlV))
            .expect_err("expected route fallback failure");
        let message = format!("{err:#}");
        assert!(message.contains("all route shortcut attempts failed"));
        assert!(message.contains("primary"));
        assert!(message.contains("adaptive_fallback"));
    }
}
