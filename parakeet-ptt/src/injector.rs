use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

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
}

impl ClipboardInjector {
    pub fn new(wtype_binary: PathBuf) -> Self {
        Self { wtype_binary }
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
        // wl-copy reads from stdin if no arguments are provided, or we can use echo | wl-copy
        // But safer to write to stdin of the process
        let mut child = Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn wl-copy")?;

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(text.as_bytes()).context("failed to write to wl-copy stdin")?;
        }

        let status = child.wait().context("failed to wait for wl-copy")?;
        if !status.success() {
            anyhow::bail!("wl-copy exited with status {}", status);
        }
        Ok(())
    }
}

impl TextInjector for ClipboardInjector {
    fn inject(&self, text: &str) -> Result<()> {
        // 1. Save current clipboard
        let original_clipboard = Self::get_clipboard().unwrap_or_default();

        // 2. Set new text to clipboard
        Self::set_clipboard(text)?;

        // 3. Simulate Ctrl+V
        // wtype -M ctrl -k v -m ctrl
        let status = Command::new(&self.wtype_binary)
            .arg("-M")
            .arg("ctrl")
            .arg("-k")
            .arg("v")
            .arg("-m")
            .arg("ctrl")
            .status()
            .context("failed to spawn wtype for paste")?;

        if !status.success() {
            anyhow::bail!("wtype paste exited with status {}", status);
        }

        // 4. Restore original clipboard (optional, but good UX)
        // We need a small delay to ensure the paste has happened before we restore
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = Self::set_clipboard(&original_clipboard);

        Ok(())
    }
}
