use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::OverlayMode;

use parakeet_ptt::overlay_ipc::OverlayIpcMessage;

#[derive(Debug, Default)]
pub struct OverlayProcessMetrics {
    pub launch_success_total: AtomicU64,
    pub launch_failure_total: AtomicU64,
    pub events_enqueued_total: AtomicU64,
    pub events_dropped_total: AtomicU64,
    pub writer_disconnect_total: AtomicU64,
}

impl OverlayProcessMetrics {
    fn note_launch_success(&self) {
        self.launch_success_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_launch_failure(&self) {
        self.launch_failure_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_enqueued(&self) {
        self.events_enqueued_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_dropped(&self) {
        self.events_dropped_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_writer_disconnect(&self) {
        self.writer_disconnect_total.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct OverlayProcessSink {
    sender: mpsc::UnboundedSender<OverlayIpcMessage>,
    metrics: Arc<OverlayProcessMetrics>,
}

impl OverlayProcessSink {
    pub fn spawn(mode: OverlayMode) -> Result<Self> {
        let backend = match mode {
            OverlayMode::LayerShell => "layer-shell",
            OverlayMode::FallbackWindow => "fallback-window",
            OverlayMode::Disabled => {
                return Err(anyhow!(
                    "overlay process should not be spawned when overlay mode is disabled"
                ));
            }
        };

        let overlay_binary = resolve_overlay_binary_path()?;
        let metrics = Arc::new(OverlayProcessMetrics::default());
        let mut command = Command::new(&overlay_binary);
        command
            .arg("--backend")
            .arg(backend)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                metrics.note_launch_failure();
                return Err(err).with_context(|| {
                    format!(
                        "failed to spawn overlay process '{}'",
                        overlay_binary.display()
                    )
                });
            }
        };

        let child_id = child.id();
        let Some(mut child_stdin) = child.stdin.take() else {
            metrics.note_launch_failure();
            return Err(anyhow!("spawned overlay process did not expose stdin"));
        };

        let (sender, mut receiver) = mpsc::unbounded_channel::<OverlayIpcMessage>();
        let writer_metrics = Arc::clone(&metrics);
        tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                let payload = match serde_json::to_vec(&message) {
                    Ok(payload) => payload,
                    Err(err) => {
                        writer_metrics.note_dropped();
                        warn!(error = %err, "failed to serialize overlay message for child process");
                        continue;
                    }
                };
                if let Err(err) = child_stdin.write_all(&payload).await {
                    writer_metrics.note_dropped();
                    writer_metrics.note_writer_disconnect();
                    warn!(error = %err, "overlay child stdin write failed; disabling overlay routing");
                    break;
                }
                if let Err(err) = child_stdin.write_all(b"\n").await {
                    writer_metrics.note_dropped();
                    writer_metrics.note_writer_disconnect();
                    warn!(error = %err, "overlay child stdin newline write failed; disabling overlay routing");
                    break;
                }
            }

            drop(child_stdin);
            match child.wait().await {
                Ok(status) => {
                    if status.success() {
                        info!(?child_id, ?status, "overlay process exited cleanly");
                    } else {
                        warn!(
                            ?child_id,
                            ?status,
                            "overlay process exited with failure status"
                        );
                    }
                }
                Err(err) => {
                    warn!(error = %err, ?child_id, "failed waiting on overlay process");
                }
            }
        });

        metrics.note_launch_success();
        info!(
            binary = %overlay_binary.display(),
            backend,
            ?child_id,
            "overlay process spawned"
        );

        Ok(Self { sender, metrics })
    }

    pub fn send(&self, message: OverlayIpcMessage) {
        if self.sender.send(message).is_ok() {
            self.metrics.note_enqueued();
            return;
        }

        self.metrics.note_dropped();
        self.metrics.note_writer_disconnect();
        debug!("overlay process channel disconnected; dropping overlay event");
    }

    pub fn metrics(&self) -> &Arc<OverlayProcessMetrics> {
        &self.metrics
    }

    #[cfg(test)]
    pub fn from_sender_for_tests(
        sender: mpsc::UnboundedSender<OverlayIpcMessage>,
        metrics: Arc<OverlayProcessMetrics>,
    ) -> Self {
        Self { sender, metrics }
    }
}

fn resolve_overlay_binary_path() -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("failed to locate current executable")?;
    let binary = current_exe.with_file_name("parakeet-overlay");
    Ok(binary)
}
