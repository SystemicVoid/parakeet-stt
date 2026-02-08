use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::{ClipboardOptions, PasteRestorePolicy, PasteShortcut};

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
pub struct ClipboardInjector {
    wtype_binary: PathBuf,
    options: ClipboardOptions,
}

impl ClipboardInjector {
    const CLIPBOARD_READY_TIMEOUT_MS: u64 = 250;
    const CLIPBOARD_READY_POLL_MS: u64 = 10;

    pub fn new(wtype_binary: PathBuf, options: ClipboardOptions) -> Self {
        Self {
            wtype_binary,
            options,
        }
    }

    fn get_clipboard() -> Result<String> {
        let output = Command::new("wl-paste")
            .arg("--no-newline") // Don't add newline if not present
            .output()
            .context("failed to spawn wl-paste")?;

        // It's okay if wl-paste fails (e.g. empty clipboard), we just return empty string
        if !output.status.success() {
            return Ok(String::new());
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn set_clipboard(text: &str, mime_type: &str, foreground: bool) -> Result<Option<Child>> {
        debug!(
            len = text.len(),
            foreground,
            mime_type = %mime_type,
            "setting clipboard via wl-copy"
        );
        let mut command = Command::new("wl-copy");
        command.arg("--type").arg(mime_type).stdin(Stdio::piped());
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
            debug!("wl-copy foreground source started");
            return Ok(Some(child));
        }

        // wl-copy forks a background helper by default. Piping stderr and reading
        // with wait_with_output can hang because the helper keeps the pipe open.
        let status = child.wait().context("failed to wait for wl-copy")?;
        debug!(?status, "wl-copy finished");
        if !status.success() {
            anyhow::bail!("wl-copy exited with status {}", status);
        }
        Ok(None)
    }

    fn wait_for_clipboard_value(
        expected: &str,
        timeout: Duration,
        poll: Duration,
    ) -> (bool, Option<String>) {
        let started = Instant::now();
        let mut last_observed = None;

        loop {
            match Self::get_clipboard() {
                Ok(value) => {
                    let matches = value == expected;
                    last_observed = Some(value);
                    if matches {
                        return (true, last_observed);
                    }
                }
                Err(err) => {
                    debug!(
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

    fn shortcut_args(shortcut: PasteShortcut) -> &'static [&'static str] {
        match shortcut {
            PasteShortcut::CtrlV => &["-M", "ctrl", "-k", "v", "-m", "ctrl"],
            PasteShortcut::CtrlShiftV => &[
                "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
            ],
            PasteShortcut::ShiftInsert => &["-M", "shift", "-k", "Insert", "-m", "shift"],
        }
    }

    fn run_shortcut(&self, shortcut: PasteShortcut) -> Result<()> {
        debug!(
            shortcut = ?shortcut,
            args = ?Self::shortcut_args(shortcut),
            "sending paste chord via wtype"
        );
        let status = Command::new(&self.wtype_binary)
            .args(Self::shortcut_args(shortcut))
            .status()
            .context("failed to spawn wtype for paste chord")?;

        debug!(?status, "paste chord command finished");
        if !status.success() {
            anyhow::bail!(
                "paste key chord {:?} exited with status {}",
                shortcut,
                status
            );
        }

        Ok(())
    }

    fn run_paste_shortcut(&self) -> Result<()> {
        if let Err(primary_err) = self.run_shortcut(self.options.paste_shortcut) {
            let Some(fallback) = self.options.shortcut_fallback else {
                return Err(primary_err);
            };

            if fallback == self.options.paste_shortcut {
                warn!(
                    shortcut = ?fallback,
                    "fallback shortcut matches primary shortcut; skipping retry"
                );
                return Err(primary_err);
            }

            warn!(
                primary = ?self.options.paste_shortcut,
                fallback = ?fallback,
                primary_error = %primary_err,
                "primary paste chord failed; trying fallback shortcut"
            );
            std::thread::sleep(Duration::from_millis(40));

            return self.run_shortcut(fallback).with_context(|| {
                format!(
                    "primary paste chord {:?} failed and fallback {:?} also failed",
                    self.options.paste_shortcut, fallback
                )
            });
        }

        Ok(())
    }

    fn stop_foreground_source(source: &mut Option<Child>) {
        let Some(mut child) = source.take() else {
            return;
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                debug!(?status, "wl-copy foreground source already exited");
                return;
            }
            Ok(None) => {}
            Err(err) => {
                warn!(
                    error = %err,
                    "failed to query wl-copy foreground source state"
                );
            }
        }

        if let Err(err) = child.kill() {
            warn!(error = %err, "failed to stop wl-copy foreground source");
        }
        if let Err(err) = child.wait() {
            warn!(error = %err, "failed to wait for wl-copy foreground source");
        } else {
            debug!("wl-copy foreground source stopped");
        }
    }

    fn transfer_to_background_if_needed(&self, text: &str, source: &mut Option<Child>) {
        if source.is_none() {
            return;
        }

        debug!("transferring clipboard ownership from foreground to background source");
        if let Err(err) = Self::set_clipboard(text, &self.options.mime_type, false) {
            warn!(
                error = %err,
                "failed to transfer clipboard ownership to background source"
            );
        }
        Self::stop_foreground_source(source);
    }

    fn restore_clipboard(&self, original: &Option<String>, source: &mut Option<Child>) {
        Self::stop_foreground_source(source);

        let Some(original_clipboard) = original else {
            debug!("no original clipboard captured; skipping restore");
            return;
        };

        debug!(
            len = original_clipboard.len(),
            "restoring original clipboard"
        );
        if let Err(err) = Self::set_clipboard(original_clipboard, &self.options.mime_type, false) {
            warn!(error = %err, "failed to restore original clipboard");
        } else {
            debug!("original clipboard restored");
        }
    }
}

impl TextInjector for ClipboardInjector {
    fn inject(&self, text: &str) -> Result<()> {
        let start = Instant::now();
        info!(
            mode = "paste",
            shortcut = ?self.options.paste_shortcut,
            restore_policy = ?self.options.restore_policy,
            restore_delay_ms = self.options.restore_delay_ms,
            copy_foreground = self.options.copy_foreground,
            mime_type = %self.options.mime_type,
            len = text.len(),
            preview = %preview(text),
            "injecting via clipboard"
        );

        // 1. Save current clipboard
        let original_clipboard = match Self::get_clipboard() {
            Ok(value) => {
                debug!(
                    elapsed_ms = start.elapsed().as_millis(),
                    captured_len = value.len(),
                    "captured existing clipboard"
                );
                Some(value)
            }
            Err(err) => {
                warn!(error = %err, "failed to read current clipboard before paste; restore will be skipped");
                None
            }
        };

        // 2. Set new text to clipboard
        debug!(
            elapsed_ms = start.elapsed().as_millis(),
            requested_len = text.len(),
            "writing transcript to clipboard"
        );
        let mut foreground_source =
            Self::set_clipboard(text, &self.options.mime_type, self.options.copy_foreground)?;
        debug!(
            elapsed_ms = start.elapsed().as_millis(),
            "clipboard write completed"
        );

        // 2b. Wait briefly for wl-copy ownership to become readable before firing
        // the paste chord. This reduces stale-paste races.
        let (ready, observed) = Self::wait_for_clipboard_value(
            text,
            Duration::from_millis(Self::CLIPBOARD_READY_TIMEOUT_MS),
            Duration::from_millis(Self::CLIPBOARD_READY_POLL_MS),
        );
        if ready {
            debug!(
                elapsed_ms = start.elapsed().as_millis(),
                stored_len = observed.as_ref().map_or(0, |value| value.len()),
                "clipboard became ready with requested text"
            );
        } else {
            warn!(
                mode = "paste",
                elapsed_ms = start.elapsed().as_millis(),
                requested_len = text.len(),
                stored_len = observed.as_ref().map_or(0, |value| value.len()),
                timeout_ms = Self::CLIPBOARD_READY_TIMEOUT_MS,
                "clipboard did not match requested text before timeout; continuing paste attempt"
            );
        }

        // 3. Simulate the configured paste shortcut (e.g. Ctrl+Shift+V in Ghostty).
        if let Err(err) = self.run_paste_shortcut() {
            warn!(
                error = %err,
                elapsed_ms = start.elapsed().as_millis(),
                "paste chord failed; attempting clipboard restore"
            );
            if matches!(self.options.restore_policy, PasteRestorePolicy::Delayed) {
                self.restore_clipboard(&original_clipboard, &mut foreground_source);
            } else {
                self.transfer_to_background_if_needed(text, &mut foreground_source);
            }
            return Err(err);
        }
        debug!(
            elapsed_ms = start.elapsed().as_millis(),
            "paste chord completed"
        );

        match self.options.restore_policy {
            PasteRestorePolicy::Never => {
                self.transfer_to_background_if_needed(text, &mut foreground_source);
                debug!(
                    elapsed_ms = start.elapsed().as_millis(),
                    "restore policy is never; leaving transcript in clipboard"
                );
            }
            PasteRestorePolicy::Delayed => {
                // A delay avoids racing the target application's clipboard read.
                debug!(
                    elapsed_ms = start.elapsed().as_millis(),
                    restore_delay_ms = self.options.restore_delay_ms,
                    "sleeping before clipboard restore"
                );
                std::thread::sleep(Duration::from_millis(self.options.restore_delay_ms));
                self.restore_clipboard(&original_clipboard, &mut foreground_source);
            }
        }
        debug!(
            elapsed_ms = start.elapsed().as_millis(),
            "clipboard injection flow finished"
        );

        Ok(())
    }
}

fn preview(text: &str) -> String {
    const MAX: usize = 80;
    if text.len() <= MAX {
        text.to_string()
    } else {
        format!("{}…", &text[..MAX])
    }
}
