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

use crate::config::{ClipboardOptions, PasteRestorePolicy, PasteShortcut, PasteStrategy};

static INJECTION_TRACE_ID: AtomicU64 = AtomicU64::new(1);

pub trait TextInjector: Send + Sync {
    fn inject(&self, text: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct NoopInjector;

impl TextInjector for NoopInjector {
    fn inject(&self, _text: &str) -> Result<()> {
        Ok(())
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
pub struct WtypeInjector {
    binary: PathBuf,
    delay_ms: u64,
}

impl WtypeInjector {
    pub fn new(binary: PathBuf, delay_ms: u64) -> Self {
        Self { binary, delay_ms }
    }
}

impl TextInjector for WtypeInjector {
    fn inject(&self, text: &str) -> Result<()> {
        debug!(
            mode = "type",
            len = text.len(),
            preview = %preview(text),
            fingerprint = %fingerprint(text),
            "injecting via wtype"
        );

        let status = Command::new(&self.binary)
            .arg("-d")
            .arg(self.delay_ms.to_string())
            .arg(text)
            .status()
            .context("failed to spawn wtype")?;

        if !status.success() {
            anyhow::bail!("wtype exited with status {status}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum PasteKeySender {
    Wtype(PathBuf),
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
        keys.insert(Key::KEY_INSERT);

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
        const SHIFT: [Key; 1] = [Key::KEY_LEFTSHIFT];
        const CTRL_SHIFT: [Key; 2] = [Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT];

        match shortcut {
            PasteShortcut::CtrlV => (&CTRL, Key::KEY_V),
            PasteShortcut::CtrlShiftV => (&CTRL_SHIFT, Key::KEY_V),
            PasteShortcut::ShiftInsert => (&SHIFT, Key::KEY_INSERT),
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

    pub fn new(sender: PasteKeySender, options: ClipboardOptions, copy_only: bool) -> Self {
        Self {
            sender,
            options,
            copy_only,
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
            mime_type = %options.mime_type,
            seat = ?options.seat,
            "setting clipboard via wl-copy"
        );
        let mut command = Command::new("wl-copy");
        command
            .arg("--type")
            .arg(&options.mime_type)
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

    fn wtype_shortcut_args(shortcut: PasteShortcut) -> &'static [&'static str] {
        match shortcut {
            PasteShortcut::CtrlV => &["-M", "ctrl", "-k", "v", "-m", "ctrl"],
            PasteShortcut::CtrlShiftV => &[
                "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
            ],
            PasteShortcut::ShiftInsert => &["-M", "shift", "-k", "Insert", "-m", "shift"],
        }
    }

    fn ydotool_shortcut_args(shortcut: PasteShortcut) -> &'static [&'static str] {
        match shortcut {
            PasteShortcut::CtrlV => &["29:1", "47:1", "47:0", "29:0"],
            PasteShortcut::CtrlShiftV => &["29:1", "42:1", "47:1", "47:0", "42:0", "29:0"],
            PasteShortcut::ShiftInsert => &["42:1", "110:1", "110:0", "42:0"],
        }
    }

    fn sender_name(sender: &PasteKeySender) -> &'static str {
        match sender {
            PasteKeySender::Wtype(_) => "wtype",
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
            PasteKeySender::Wtype(binary) => {
                debug!(
                    trace_id,
                    shortcut = ?shortcut,
                    backend = "wtype",
                    binary = %binary.display(),
                    args = ?Self::wtype_shortcut_args(shortcut),
                    "sending paste chord"
                );
                let status = Command::new(binary)
                    .args(Self::wtype_shortcut_args(shortcut))
                    .status()
                    .context("failed to spawn wtype for paste chord")?;

                debug!(
                    trace_id,
                    ?status,
                    backend = "wtype",
                    "paste chord command finished"
                );
                if !status.success() {
                    anyhow::bail!(
                        "paste key chord {:?} via wtype exited with status {}",
                        shortcut,
                        status
                    );
                }
                Ok(())
            }
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

    fn shortcut_attempt_order(&self) -> Vec<PasteShortcut> {
        let mut attempts = vec![self.options.paste_shortcut];
        if matches!(self.options.paste_strategy, PasteStrategy::Single) {
            return attempts;
        }

        if let Some(fallback) = self.options.shortcut_fallback {
            if !attempts.contains(&fallback) {
                attempts.push(fallback);
            }
        }

        if !attempts.contains(&PasteShortcut::CtrlV)
            && matches!(
                self.options.paste_strategy,
                PasteStrategy::OnError | PasteStrategy::AlwaysChain
            )
        {
            attempts.push(PasteShortcut::CtrlV);
        }

        attempts
    }

    fn run_paste_shortcuts(&self, trace_id: u64) -> Result<()> {
        let attempts = self.shortcut_attempt_order();
        let mut errors = Vec::new();

        match self.options.paste_strategy {
            PasteStrategy::Single => {
                return self.run_shortcut(trace_id, self.options.paste_shortcut);
            }
            PasteStrategy::OnError => {
                for (idx, shortcut) in attempts.iter().enumerate() {
                    match self.run_shortcut(trace_id, *shortcut) {
                        Ok(()) => {
                            if idx > 0 {
                                info!(
                                    trace_id,
                                    strategy = ?self.options.paste_strategy,
                                    shortcut = ?shortcut,
                                    "fallback paste shortcut succeeded"
                                );
                            }
                            return Ok(());
                        }
                        Err(err) => {
                            warn!(
                                trace_id,
                                strategy = ?self.options.paste_strategy,
                                attempt = idx + 1,
                                total_attempts = attempts.len(),
                                shortcut = ?shortcut,
                                error = %err,
                                "paste shortcut attempt failed"
                            );
                            errors.push(format!("{shortcut:?}: {err}"));
                            if idx + 1 < attempts.len() {
                                std::thread::sleep(Duration::from_millis(
                                    self.options.chain_delay_ms,
                                ));
                            }
                        }
                    }
                }
            }
            PasteStrategy::AlwaysChain => {
                let mut succeeded = false;
                for (idx, shortcut) in attempts.iter().enumerate() {
                    match self.run_shortcut(trace_id, *shortcut) {
                        Ok(()) => {
                            succeeded = true;
                            info!(
                                trace_id,
                                strategy = ?self.options.paste_strategy,
                                attempt = idx + 1,
                                total_attempts = attempts.len(),
                                shortcut = ?shortcut,
                                "paste shortcut executed"
                            );
                        }
                        Err(err) => {
                            warn!(
                                trace_id,
                                strategy = ?self.options.paste_strategy,
                                attempt = idx + 1,
                                total_attempts = attempts.len(),
                                shortcut = ?shortcut,
                                error = %err,
                                "paste shortcut attempt failed"
                            );
                            errors.push(format!("{shortcut:?}: {err}"));
                        }
                    }
                    if idx + 1 < attempts.len() {
                        std::thread::sleep(Duration::from_millis(self.options.chain_delay_ms));
                    }
                }
                if succeeded {
                    return Ok(());
                }
            }
        }

        anyhow::bail!(
            "all paste shortcut attempts failed (strategy={:?}): {}",
            self.options.paste_strategy,
            errors.join(" | ")
        )
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

    fn restore_clipboards(
        &self,
        original_clipboard: &Option<String>,
        original_primary: &Option<String>,
        clipboard_source: &mut Option<Child>,
        primary_source: &mut Option<Child>,
        trace_id: u64,
    ) {
        Self::stop_foreground_source(clipboard_source, trace_id, "clipboard");
        Self::stop_foreground_source(primary_source, trace_id, "primary");

        let Some(clipboard) = original_clipboard else {
            debug!(
                trace_id,
                "no original clipboard captured; skipping clipboard restore"
            );
            return;
        };

        debug!(
            trace_id,
            len = clipboard.len(),
            "restoring original clipboard"
        );
        if let Err(err) = Self::set_clipboard(clipboard, &self.options, false, false) {
            warn!(trace_id, error = %err, "failed to restore original clipboard");
        } else {
            debug!(trace_id, "original clipboard restored");
        }

        if self.options.write_primary {
            if let Some(primary) = original_primary {
                debug!(
                    trace_id,
                    len = primary.len(),
                    "restoring original primary selection"
                );
                if let Err(err) = Self::set_clipboard(primary, &self.options, false, true) {
                    warn!(trace_id, error = %err, "failed to restore original primary selection");
                }
            } else {
                debug!(
                    trace_id,
                    "no original primary selection captured; skipping restore"
                );
            }
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
            shortcut = ?self.options.paste_shortcut,
            shortcut_fallback = ?self.options.shortcut_fallback,
            strategy = ?self.options.paste_strategy,
            chain_delay_ms = self.options.chain_delay_ms,
            restore_policy = ?self.options.restore_policy,
            restore_delay_ms = self.options.restore_delay_ms,
            post_chord_hold_ms = self.options.post_chord_hold_ms,
            copy_foreground = self.options.copy_foreground,
            key_backend = ?self.options.key_backend,
            seat = ?self.options.seat,
            write_primary = self.options.write_primary,
            mime_type = %self.options.mime_type,
            len = text.len(),
            fingerprint = %fingerprint(text),
            preview = %preview(text),
            "starting clipboard injection"
        );

        // 1. Save existing clipboard(s).
        let original_clipboard = match Self::get_clipboard(&self.options, false) {
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

        let original_primary = if self.options.write_primary {
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

        // 2. Write transcript into clipboard.
        debug!(
            trace_id,
            elapsed_ms = started.elapsed().as_millis(),
            requested_len = text.len(),
            requested_fingerprint = %fingerprint(text),
            "writing transcript to clipboard"
        );
        let (mut foreground_clipboard_source, mut foreground_primary_source) = self
            .write_clipboards(text, self.options.copy_foreground)
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

        // 3. Send paste shortcut(s).
        if let Err(err) = self.run_paste_shortcuts(trace_id) {
            outcome = InjectionOutcome::ChordFailed;
            warn!(
                trace_id,
                error = %err,
                elapsed_ms = started.elapsed().as_millis(),
                outcome = outcome.as_str(),
                "paste shortcut stage failed"
            );
            if matches!(self.options.restore_policy, PasteRestorePolicy::Delayed) {
                self.restore_clipboards(
                    &original_clipboard,
                    &original_primary,
                    &mut foreground_clipboard_source,
                    &mut foreground_primary_source,
                    trace_id,
                );
            } else {
                self.transfer_to_background_if_needed(
                    text,
                    &mut foreground_clipboard_source,
                    &mut foreground_primary_source,
                    trace_id,
                );
            }
            return Err(err);
        }

        if self.options.post_chord_hold_ms > 0 {
            debug!(
                trace_id,
                elapsed_ms = started.elapsed().as_millis(),
                hold_ms = self.options.post_chord_hold_ms,
                "holding foreground clipboard source after paste chords"
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

        match self.options.restore_policy {
            PasteRestorePolicy::Never => {
                self.transfer_to_background_if_needed(
                    text,
                    &mut foreground_clipboard_source,
                    &mut foreground_primary_source,
                    trace_id,
                );
                debug!(
                    trace_id,
                    elapsed_ms = started.elapsed().as_millis(),
                    "restore policy is never; leaving transcript in clipboard"
                );
            }
            PasteRestorePolicy::Delayed => {
                debug!(
                    trace_id,
                    elapsed_ms = started.elapsed().as_millis(),
                    restore_delay_ms = self.options.restore_delay_ms,
                    "sleeping before clipboard restore"
                );
                std::thread::sleep(Duration::from_millis(self.options.restore_delay_ms));
                self.restore_clipboards(
                    &original_clipboard,
                    &original_primary,
                    &mut foreground_clipboard_source,
                    &mut foreground_primary_source,
                    trace_id,
                );
            }
        }

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
        ClipboardOptions, PasteBackendFailurePolicy, PasteKeyBackend, PasteRestorePolicy,
        PasteShortcut, PasteStrategy,
    };
    use evdev::Key;

    fn options(
        strategy: PasteStrategy,
        primary: PasteShortcut,
        fallback: Option<PasteShortcut>,
    ) -> ClipboardOptions {
        ClipboardOptions {
            paste_shortcut: primary,
            shortcut_fallback: fallback,
            paste_strategy: strategy,
            chain_delay_ms: 45,
            restore_policy: PasteRestorePolicy::Never,
            restore_delay_ms: 250,
            post_chord_hold_ms: 700,
            copy_foreground: true,
            mime_type: "text/plain;charset=utf-8".to_string(),
            key_backend: PasteKeyBackend::Wtype,
            backend_failure_policy: PasteBackendFailurePolicy::CopyOnly,
            seat: None,
            write_primary: false,
        }
    }

    #[test]
    fn single_strategy_uses_primary_only() {
        let injector = ClipboardInjector::new(
            PasteKeySender::Disabled,
            options(
                PasteStrategy::Single,
                PasteShortcut::CtrlShiftV,
                Some(PasteShortcut::CtrlV),
            ),
            false,
        );
        assert_eq!(
            injector.shortcut_attempt_order(),
            vec![PasteShortcut::CtrlShiftV]
        );
    }

    #[test]
    fn on_error_strategy_adds_ctrl_v_tail() {
        let injector = ClipboardInjector::new(
            PasteKeySender::Disabled,
            options(
                PasteStrategy::OnError,
                PasteShortcut::ShiftInsert,
                Some(PasteShortcut::CtrlShiftV),
            ),
            false,
        );
        assert_eq!(
            injector.shortcut_attempt_order(),
            vec![
                PasteShortcut::ShiftInsert,
                PasteShortcut::CtrlShiftV,
                PasteShortcut::CtrlV,
            ]
        );
    }

    #[test]
    fn always_chain_deduplicates_ctrl_v() {
        let injector = ClipboardInjector::new(
            PasteKeySender::Disabled,
            options(
                PasteStrategy::AlwaysChain,
                PasteShortcut::CtrlV,
                Some(PasteShortcut::CtrlV),
            ),
            false,
        );
        assert_eq!(
            injector.shortcut_attempt_order(),
            vec![PasteShortcut::CtrlV]
        );
    }

    #[test]
    fn uinput_shortcut_plan_ctrl_shift_v() {
        let (modifiers, key) = UinputChordSender::shortcut_plan(PasteShortcut::CtrlShiftV);
        assert_eq!(modifiers, [Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT]);
        assert_eq!(key, Key::KEY_V);
    }
}
