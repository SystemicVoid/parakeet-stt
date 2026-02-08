use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::PasteShortcut;

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
    paste_shortcut: PasteShortcut,
    restore_delay_ms: u64,
}

impl ClipboardInjector {
    pub fn new(
        wtype_binary: PathBuf,
        paste_shortcut: PasteShortcut,
        restore_delay_ms: u64,
    ) -> Self {
        Self {
            wtype_binary,
            paste_shortcut,
            restore_delay_ms,
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

    fn set_clipboard(text: &str) -> Result<()> {
        debug!(len = text.len(), "setting clipboard via wl-copy");
        let mut child = Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn wl-copy")?;

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin
                .write_all(text.as_bytes())
                .context("failed to write to wl-copy stdin")?;
        }

        // wl-copy forks a background helper by default. Piping stderr and reading
        // with wait_with_output can hang because the helper keeps the pipe open.
        let status = child.wait().context("failed to wait for wl-copy")?;
        debug!(?status, "wl-copy finished");
        if !status.success() {
            anyhow::bail!("wl-copy exited with status {}", status);
        }
        Ok(())
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

    fn run_paste_shortcut(&self) -> Result<()> {
        debug!(
            shortcut = ?self.paste_shortcut,
            args = ?Self::shortcut_args(self.paste_shortcut),
            "sending paste chord via wtype"
        );
        let status = Command::new(&self.wtype_binary)
            .args(Self::shortcut_args(self.paste_shortcut))
            .status()
            .context("failed to spawn wtype for paste chord")?;

        debug!(?status, "paste chord command finished");
        if !status.success() {
            anyhow::bail!(
                "paste key chord {:?} exited with status {}",
                self.paste_shortcut,
                status
            );
        }

        Ok(())
    }

    fn restore_clipboard(original: &Option<String>) {
        let Some(original_clipboard) = original else {
            debug!("no original clipboard captured; skipping restore");
            return;
        };

        debug!(
            len = original_clipboard.len(),
            "restoring original clipboard"
        );
        if let Err(err) = Self::set_clipboard(original_clipboard) {
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
            shortcut = ?self.paste_shortcut,
            restore_delay_ms = self.restore_delay_ms,
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
        Self::set_clipboard(text)?;
        debug!(
            elapsed_ms = start.elapsed().as_millis(),
            "clipboard write completed"
        );

        // 2b. Round-trip verification is informative only: some clipboard managers
        // transform/normalize content, so mismatch should not block paste.
        match Self::get_clipboard() {
            Ok(roundtrip) if roundtrip != text => {
                warn!(
                    mode = "paste",
                    elapsed_ms = start.elapsed().as_millis(),
                    requested_len = text.len(),
                    stored_len = roundtrip.len(),
                    "clipboard roundtrip mismatch; continuing paste attempt"
                );
            }
            Ok(roundtrip) => {
                debug!(
                    elapsed_ms = start.elapsed().as_millis(),
                    stored_len = roundtrip.len(),
                    "clipboard roundtrip matched requested text"
                );
            }
            Err(err) => {
                warn!(
                    error = %err,
                    elapsed_ms = start.elapsed().as_millis(),
                    "failed to read clipboard after wl-copy; continuing paste attempt"
                );
            }
        }

        // 3. Simulate the configured paste shortcut (e.g. Ctrl+Shift+V in Ghostty).
        if let Err(err) = self.run_paste_shortcut() {
            warn!(
                error = %err,
                elapsed_ms = start.elapsed().as_millis(),
                "paste chord failed; attempting clipboard restore"
            );
            Self::restore_clipboard(&original_clipboard);
            return Err(err);
        }
        debug!(
            elapsed_ms = start.elapsed().as_millis(),
            "paste chord completed"
        );

        // 4. Restore original clipboard (optional, but good UX)
        // A delay avoids racing the target application's clipboard read.
        debug!(
            elapsed_ms = start.elapsed().as_millis(),
            restore_delay_ms = self.restore_delay_ms,
            "sleeping before clipboard restore"
        );
        std::thread::sleep(std::time::Duration::from_millis(self.restore_delay_ms));
        Self::restore_clipboard(&original_clipboard);
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
