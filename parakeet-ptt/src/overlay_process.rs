use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::OverlayMode;
use crate::surface_focus::WaylandFocusCache;

use parakeet_ptt::overlay_ipc::OverlayIpcMessage;

const OVERLAY_RESPAWN_BACKOFF_MS: u64 = 750;

type OverlayLauncher =
    dyn Fn(OverlayMode, Option<String>) -> Result<OverlayProcessSink> + Send + Sync;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlaySendError {
    Disconnected,
}

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

#[derive(Debug, Default)]
pub struct OverlayManagerMetrics {
    pub spawn_attempt_total: AtomicU64,
    pub spawn_success_total: AtomicU64,
    pub spawn_failure_total: AtomicU64,
    pub send_disconnect_total: AtomicU64,
    pub replay_sent_total: AtomicU64,
    pub replay_dropped_total: AtomicU64,
}

impl OverlayManagerMetrics {
    fn note_spawn_attempt(&self) {
        self.spawn_attempt_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_spawn_success(&self) {
        self.spawn_success_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_spawn_failure(&self) {
        self.spawn_failure_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_send_disconnect(&self) {
        self.send_disconnect_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_replay_sent(&self) {
        self.replay_sent_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_replay_dropped(&self) {
        self.replay_dropped_total.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct OverlayProcessSink {
    sender: mpsc::UnboundedSender<OverlayIpcMessage>,
    metrics: Arc<OverlayProcessMetrics>,
}

impl OverlayProcessSink {
    pub fn spawn(mode: OverlayMode, output_name: Option<&str>) -> Result<Self> {
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
        if let Some(output_name) = output_name {
            command.arg("--output-name").arg(output_name);
        }

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

    pub fn send(&self, message: OverlayIpcMessage) -> std::result::Result<(), OverlaySendError> {
        if self.sender.send(message).is_ok() {
            self.metrics.note_enqueued();
            return Ok(());
        }

        self.metrics.note_dropped();
        self.metrics.note_writer_disconnect();
        debug!("overlay process channel disconnected; dropping overlay event");
        Err(OverlaySendError::Disconnected)
    }

    #[cfg(test)]
    pub fn from_sender_for_tests(
        sender: mpsc::UnboundedSender<OverlayIpcMessage>,
        metrics: Arc<OverlayProcessMetrics>,
    ) -> Self {
        Self { sender, metrics }
    }
}

pub struct OverlayProcessManager {
    mode: OverlayMode,
    sink: Option<OverlayProcessSink>,
    latest_message: Option<OverlayIpcMessage>,
    focus_cache: Option<WaylandFocusCache>,
    pending_output_name: Option<String>,
    require_output_name_on_spawn: bool,
    launcher: Arc<OverlayLauncher>,
    metrics: Arc<OverlayManagerMetrics>,
    respawn_backoff: Duration,
    next_spawn_allowed_at: Option<Instant>,
}

impl OverlayProcessManager {
    pub fn new(mode: OverlayMode, focus_cache: Option<WaylandFocusCache>) -> Self {
        let require_output_name_on_spawn = focus_cache.is_some();
        Self::new_with_launcher_and_backoff(
            mode,
            focus_cache,
            require_output_name_on_spawn,
            Arc::new(|mode, output_name| OverlayProcessSink::spawn(mode, output_name.as_deref())),
            Duration::from_millis(OVERLAY_RESPAWN_BACKOFF_MS),
        )
    }

    pub fn send(&mut self, message: OverlayIpcMessage) {
        let is_output_hint = matches!(&message, OverlayIpcMessage::OutputHint { .. });
        if let OverlayIpcMessage::OutputHint { output_name } = &message {
            self.pending_output_name = Some(output_name.clone());
        } else {
            self.latest_message = Some(message.clone());
        }

        if self.mode == OverlayMode::Disabled {
            return;
        }

        let sink_was_missing = self.sink.is_none();
        if sink_was_missing {
            self.try_spawn_sink();
        }

        match self.try_send_to_active(message.clone()) {
            Ok(()) => {
                if sink_was_missing && is_output_hint && self.latest_message.is_some() {
                    if self.replay_latest_message().is_err() {
                        self.metrics.note_replay_dropped();
                    }
                }
                return;
            }
            Err(OverlaySendError::Disconnected) => {
                self.metrics.note_send_disconnect();
                self.sink = None;
            }
        }

        self.try_spawn_sink();
        if self.replay_latest_message().is_err() {
            self.metrics.note_replay_dropped();
        }
    }

    pub fn metrics(&self) -> &Arc<OverlayManagerMetrics> {
        &self.metrics
    }

    pub fn has_active_sink(&self) -> bool {
        self.sink.is_some()
    }

    fn try_spawn_sink(&mut self) {
        if self.mode == OverlayMode::Disabled || self.sink.is_some() {
            return;
        }

        let now = Instant::now();
        if let Some(next_allowed) = self.next_spawn_allowed_at {
            if now < next_allowed {
                return;
            }
        }

        let output_name = self.pending_output_name.clone().or_else(|| {
            self.focus_cache
                .as_ref()
                .and_then(WaylandFocusCache::current_output_name)
        });
        if self.pending_output_name.is_none() {
            self.pending_output_name = output_name.clone();
        }

        if self.require_output_name_on_spawn && output_name.is_none() {
            debug!("deferring overlay process spawn until focused output is available");
            return;
        }

        self.metrics.note_spawn_attempt();
        match (self.launcher)(self.mode, output_name) {
            Ok(sink) => {
                self.metrics.note_spawn_success();
                self.sink = Some(sink);
                self.next_spawn_allowed_at = None;
            }
            Err(err) => {
                self.metrics.note_spawn_failure();
                self.next_spawn_allowed_at = Some(now + self.respawn_backoff);
                warn!(
                    error = %err,
                    backoff_ms = self.respawn_backoff.as_millis(),
                    "overlay process spawn failed; overlay routing remains non-fatal"
                );
            }
        }
    }

    fn replay_latest_message(&mut self) -> std::result::Result<(), OverlaySendError> {
        let Some(message) = self.latest_message.clone() else {
            return Ok(());
        };

        match self.try_send_to_active(message) {
            Ok(()) => {
                self.metrics.note_replay_sent();
                Ok(())
            }
            Err(err) => {
                self.sink = None;
                Err(err)
            }
        }
    }

    fn try_send_to_active(
        &self,
        message: OverlayIpcMessage,
    ) -> std::result::Result<(), OverlaySendError> {
        let Some(sink) = self.sink.as_ref() else {
            return Err(OverlaySendError::Disconnected);
        };

        sink.send(message)
    }

    fn new_with_launcher_and_backoff(
        mode: OverlayMode,
        focus_cache: Option<WaylandFocusCache>,
        require_output_name_on_spawn: bool,
        launcher: Arc<OverlayLauncher>,
        respawn_backoff: Duration,
    ) -> Self {
        let mut manager = Self {
            mode,
            sink: None,
            latest_message: None,
            focus_cache,
            pending_output_name: None,
            require_output_name_on_spawn,
            launcher,
            metrics: Arc::new(OverlayManagerMetrics::default()),
            respawn_backoff,
            next_spawn_allowed_at: None,
        };
        if !manager.require_output_name_on_spawn {
            manager.try_spawn_sink();
        }
        manager
    }

    #[cfg(test)]
    pub fn new_for_tests(
        mode: OverlayMode,
        launcher: Arc<OverlayLauncher>,
        respawn_backoff: Duration,
    ) -> Self {
        Self::new_with_launcher_and_backoff(mode, None, false, launcher, respawn_backoff)
    }

    #[cfg(test)]
    pub fn new_for_tests_with_output_targeting(
        mode: OverlayMode,
        launcher: Arc<OverlayLauncher>,
        respawn_backoff: Duration,
    ) -> Self {
        Self::new_with_launcher_and_backoff(mode, None, true, launcher, respawn_backoff)
    }
}

fn resolve_overlay_binary_path() -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("failed to locate current executable")?;
    let binary = current_exe.with_file_name("parakeet-overlay");
    Ok(binary)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::anyhow;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crate::config::OverlayMode;

    use super::{
        OverlayLauncher, OverlayProcessManager, OverlayProcessMetrics, OverlayProcessSink,
    };
    use parakeet_ptt::overlay_ipc::OverlayIpcMessage;

    fn queued_launcher(
        queue: Arc<Mutex<VecDeque<std::result::Result<OverlayProcessSink, anyhow::Error>>>>,
    ) -> Arc<OverlayLauncher> {
        Arc::new(move |_mode, _output_name| {
            queue
                .lock()
                .expect("spawn queue lock should be available")
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("no spawn outcome configured")))
        })
    }

    fn recording_launcher(
        seen_output_names: Arc<Mutex<Vec<Option<String>>>>,
        queue: Arc<Mutex<VecDeque<std::result::Result<OverlayProcessSink, anyhow::Error>>>>,
    ) -> Arc<OverlayLauncher> {
        Arc::new(move |_mode, output_name| {
            seen_output_names
                .lock()
                .expect("recorded output names lock should be available")
                .push(output_name);
            queue
                .lock()
                .expect("spawn queue lock should be available")
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("no spawn outcome configured")))
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manager_replays_latest_message_after_disconnect() {
        let (tx_first, rx_first) = mpsc::unbounded_channel();
        drop(rx_first);
        let first_sink = OverlayProcessSink::from_sender_for_tests(
            tx_first,
            Arc::new(OverlayProcessMetrics::default()),
        );

        let (tx_second, mut rx_second) = mpsc::unbounded_channel();
        let second_sink = OverlayProcessSink::from_sender_for_tests(
            tx_second,
            Arc::new(OverlayProcessMetrics::default()),
        );

        let queue = Arc::new(Mutex::new(VecDeque::from([
            Ok(first_sink),
            Ok(second_sink),
        ])));
        let launcher = queued_launcher(queue);
        let mut manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            launcher,
            Duration::from_millis(0),
        );

        let message = OverlayIpcMessage::InterimText {
            session_id: Uuid::new_v4(),
            seq: 10,
            text: "current".to_string(),
        };
        manager.send(message.clone());

        let replayed = timeout(Duration::from_millis(100), rx_second.recv())
            .await
            .expect("replayed message should arrive")
            .expect("receiver should remain open");
        assert_eq!(replayed, message);
        assert!(timeout(Duration::from_millis(50), rx_second.recv())
            .await
            .is_err());

        assert_eq!(
            manager.metrics().replay_sent_total.load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            manager
                .metrics()
                .spawn_success_total
                .load(Ordering::Relaxed),
            2
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manager_reconnect_sends_only_current_state() {
        let (tx_first, mut rx_first) = mpsc::unbounded_channel();
        let first_sink = OverlayProcessSink::from_sender_for_tests(
            tx_first,
            Arc::new(OverlayProcessMetrics::default()),
        );

        let (tx_second, mut rx_second) = mpsc::unbounded_channel();
        let second_sink = OverlayProcessSink::from_sender_for_tests(
            tx_second,
            Arc::new(OverlayProcessMetrics::default()),
        );

        let queue = Arc::new(Mutex::new(VecDeque::from([
            Ok(first_sink),
            Ok(second_sink),
        ])));
        let launcher = queued_launcher(queue);
        let mut manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            launcher,
            Duration::from_millis(0),
        );

        let session_id = Uuid::new_v4();
        let old_state = OverlayIpcMessage::InterimText {
            session_id,
            seq: 1,
            text: "old-state".to_string(),
        };
        manager.send(old_state.clone());
        let first_seen = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive old state")
            .expect("first sink should remain open");
        assert_eq!(first_seen, old_state);

        drop(rx_first);

        let current_state = OverlayIpcMessage::InterimText {
            session_id,
            seq: 2,
            text: "current-state".to_string(),
        };
        manager.send(current_state.clone());

        let second_seen = timeout(Duration::from_millis(100), rx_second.recv())
            .await
            .expect("second sink should receive current state")
            .expect("second sink should remain open");
        assert_eq!(second_seen, current_state);
        assert!(timeout(Duration::from_millis(50), rx_second.recv())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_targeted_manager_waits_for_hint_and_replays_latest_state() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = OverlayProcessSink::from_sender_for_tests(
            tx,
            Arc::new(OverlayProcessMetrics::default()),
        );
        let queue = Arc::new(Mutex::new(VecDeque::from([Ok(sink)])));
        let seen_output_names = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let launcher = recording_launcher(Arc::clone(&seen_output_names), queue);
        let mut manager = OverlayProcessManager::new_for_tests_with_output_targeting(
            OverlayMode::LayerShell,
            launcher,
            Duration::ZERO,
        );

        let session_id = Uuid::new_v4();
        let latest_state = OverlayIpcMessage::InterimText {
            session_id,
            seq: 7,
            text: "latest-state".to_string(),
        };
        manager.send(latest_state.clone());
        assert!(!manager.has_active_sink());
        assert_eq!(
            manager
                .metrics()
                .spawn_attempt_total
                .load(Ordering::Relaxed),
            0,
            "spawn should be deferred until an output hint is available"
        );

        manager.send(OverlayIpcMessage::OutputHint {
            output_name: "DP-1".to_string(),
        });
        assert!(manager.has_active_sink());
        assert_eq!(
            seen_output_names
                .lock()
                .expect("recorded output names lock should be available")
                .clone(),
            vec![Some("DP-1".to_string())]
        );

        let first_seen = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("output hint should be sent after spawn")
            .expect("sink should remain open");
        assert_eq!(
            first_seen,
            OverlayIpcMessage::OutputHint {
                output_name: "DP-1".to_string()
            }
        );

        let replayed = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("latest state should be replayed after output-targeted spawn")
            .expect("sink should remain open");
        assert_eq!(replayed, latest_state);
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());
    }

    #[test]
    fn missing_overlay_binary_spawn_failures_remain_non_fatal() {
        let launcher: Arc<OverlayLauncher> = Arc::new(|_mode, _output_name| {
            Err(anyhow!(
                "failed to spawn overlay process '/tmp/parakeet-overlay': No such file or directory"
            ))
        });
        let mut manager =
            OverlayProcessManager::new_for_tests(OverlayMode::LayerShell, launcher, Duration::ZERO);

        manager.send(OverlayIpcMessage::InterimText {
            session_id: Uuid::new_v4(),
            seq: 1,
            text: "state survives missing binary".to_string(),
        });

        assert!(!manager.has_active_sink());
        assert!(
            manager
                .metrics()
                .spawn_failure_total
                .load(Ordering::Relaxed)
                >= 1,
            "missing binary should be counted as a non-fatal spawn failure"
        );
    }
}
