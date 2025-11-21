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
