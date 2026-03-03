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
const OVERLAY_OUTPUT_NAME_WATCHDOG_TIMEOUT_MS: u64 = 1_500;

type OverlayLauncher =
    dyn Fn(OverlayMode, Option<String>, bool) -> Result<OverlayProcessSink> + Send + Sync;

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
    pub fn spawn(
        mode: OverlayMode,
        output_name: Option<&str>,
        adaptive_width: bool,
    ) -> Result<Self> {
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
            .arg("--adaptive-width")
            .arg(if adaptive_width { "true" } else { "false" })
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
            adaptive_width,
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
    adaptive_width: bool,
    sink: Option<OverlayProcessSink>,
    active_output_name: Option<String>,
    latest_message: Option<OverlayIpcMessage>,
    focus_cache: Option<WaylandFocusCache>,
    pending_output_name: Option<String>,
    utterance_active: bool,
    require_output_name_on_spawn: bool,
    launcher: Arc<OverlayLauncher>,
    metrics: Arc<OverlayManagerMetrics>,
    respawn_backoff: Duration,
    next_spawn_allowed_at: Option<Instant>,
    output_wait_started_at: Option<Instant>,
    output_watchdog_timeout: Duration,
    output_watchdog_fallback_used: bool,
}

impl OverlayProcessManager {
    pub fn new(
        mode: OverlayMode,
        adaptive_width: bool,
        focus_cache: Option<WaylandFocusCache>,
    ) -> Self {
        let require_output_name_on_spawn = focus_cache.is_some();
        Self::new_with_launcher_and_backoff_and_watchdog(
            mode,
            adaptive_width,
            focus_cache,
            require_output_name_on_spawn,
            Arc::new(|mode, output_name, adaptive_width| {
                OverlayProcessSink::spawn(mode, output_name.as_deref(), adaptive_width)
            }),
            Duration::from_millis(OVERLAY_RESPAWN_BACKOFF_MS),
            Duration::from_millis(OVERLAY_OUTPUT_NAME_WATCHDOG_TIMEOUT_MS),
        )
    }

    pub fn send(&mut self, message: OverlayIpcMessage) {
        let is_output_hint = matches!(&message, OverlayIpcMessage::OutputHint { .. });
        let starts_utterance = matches!(
            &message,
            OverlayIpcMessage::InterimState { .. } | OverlayIpcMessage::InterimText { .. }
        );
        let ends_utterance = matches!(&message, OverlayIpcMessage::SessionEnded { .. });
        if let OverlayIpcMessage::OutputHint { output_name } = &message {
            self.pending_output_name = Some(output_name.clone());
            self.output_wait_started_at = None;
        } else if is_replayable_overlay_message(&message) {
            self.latest_message = Some(message.clone());
        }

        if starts_utterance {
            if !self.utterance_active {
                self.maybe_retarget_for_next_utterance();
            }
            self.utterance_active = true;
        } else if ends_utterance {
            self.utterance_active = false;
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
                if sink_was_missing
                    && is_output_hint
                    && self.latest_message.is_some()
                    && self.replay_latest_message().is_err()
                {
                    self.metrics.note_replay_dropped();
                }
                return;
            }
            Err(OverlaySendError::Disconnected) => {
                self.metrics.note_send_disconnect();
                self.sink = None;
                self.active_output_name = None;
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
        if output_name.is_some() {
            self.output_wait_started_at = None;
        }
        if self.pending_output_name.is_none() {
            self.pending_output_name = output_name.clone();
        }

        if self.require_output_name_on_spawn && output_name.is_none() {
            if self.output_watchdog_fallback_used {
                debug!(
                    "deferring overlay process spawn; output watchdog fallback already used once"
                );
                return;
            }
            let wait_started = self.output_wait_started_at.get_or_insert(now);
            let waited = now.saturating_duration_since(*wait_started);
            if waited < self.output_watchdog_timeout {
                debug!(
                    waited_ms = waited.as_millis(),
                    timeout_ms = self.output_watchdog_timeout.as_millis(),
                    "deferring overlay process spawn until focused output is available"
                );
                return;
            }
            self.output_watchdog_fallback_used = true;
            self.output_wait_started_at = None;
            warn!(
                waited_ms = waited.as_millis(),
                timeout_ms = self.output_watchdog_timeout.as_millis(),
                "focused output unavailable after watchdog timeout; spawning overlay without output targeting once"
            );
        }

        self.metrics.note_spawn_attempt();
        match (self.launcher)(self.mode, output_name.clone(), self.adaptive_width) {
            Ok(sink) => {
                self.metrics.note_spawn_success();
                self.sink = Some(sink);
                self.active_output_name = output_name;
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
                self.active_output_name = None;
                Err(err)
            }
        }
    }

    fn maybe_retarget_for_next_utterance(&mut self) {
        let Some(pending_output_name) = self.pending_output_name.clone() else {
            return;
        };
        if self.active_output_name.as_deref() == Some(pending_output_name.as_str()) {
            return;
        }
        if self.sink.is_none() {
            return;
        }

        match (self.launcher)(
            self.mode,
            Some(pending_output_name.clone()),
            self.adaptive_width,
        ) {
            Ok(sink) => {
                info!(
                    from_output = ?self.active_output_name,
                    to_output = %pending_output_name,
                    "retargeted overlay process for next utterance"
                );
                self.sink = Some(sink);
                self.active_output_name = Some(pending_output_name);
                self.next_spawn_allowed_at = None;
            }
            Err(err) => {
                warn!(
                    error = %err,
                    from_output = ?self.active_output_name,
                    to_output = %pending_output_name,
                    "failed to retarget overlay process; keeping existing overlay sink"
                );
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

    #[cfg(test)]
    fn new_with_launcher_and_backoff(
        mode: OverlayMode,
        adaptive_width: bool,
        focus_cache: Option<WaylandFocusCache>,
        require_output_name_on_spawn: bool,
        launcher: Arc<OverlayLauncher>,
        respawn_backoff: Duration,
    ) -> Self {
        Self::new_with_launcher_and_backoff_and_watchdog(
            mode,
            adaptive_width,
            focus_cache,
            require_output_name_on_spawn,
            launcher,
            respawn_backoff,
            Duration::from_millis(OVERLAY_OUTPUT_NAME_WATCHDOG_TIMEOUT_MS),
        )
    }

    fn new_with_launcher_and_backoff_and_watchdog(
        mode: OverlayMode,
        adaptive_width: bool,
        focus_cache: Option<WaylandFocusCache>,
        require_output_name_on_spawn: bool,
        launcher: Arc<OverlayLauncher>,
        respawn_backoff: Duration,
        output_watchdog_timeout: Duration,
    ) -> Self {
        let mut manager = Self {
            mode,
            adaptive_width,
            sink: None,
            active_output_name: None,
            latest_message: None,
            focus_cache,
            pending_output_name: None,
            utterance_active: false,
            require_output_name_on_spawn,
            launcher,
            metrics: Arc::new(OverlayManagerMetrics::default()),
            respawn_backoff,
            next_spawn_allowed_at: None,
            output_wait_started_at: None,
            output_watchdog_timeout,
            output_watchdog_fallback_used: false,
        };
        if !manager.require_output_name_on_spawn {
            manager.try_spawn_sink();
        }
        manager
    }

    #[cfg(test)]
    pub fn new_for_tests(
        mode: OverlayMode,
        adaptive_width: bool,
        launcher: Arc<OverlayLauncher>,
        respawn_backoff: Duration,
    ) -> Self {
        Self::new_with_launcher_and_backoff(
            mode,
            adaptive_width,
            None,
            false,
            launcher,
            respawn_backoff,
        )
    }

    #[cfg(test)]
    pub fn new_for_tests_with_output_targeting(
        mode: OverlayMode,
        adaptive_width: bool,
        launcher: Arc<OverlayLauncher>,
        respawn_backoff: Duration,
    ) -> Self {
        Self::new_with_launcher_and_backoff(
            mode,
            adaptive_width,
            None,
            true,
            launcher,
            respawn_backoff,
        )
    }

    #[cfg(test)]
    pub fn new_for_tests_with_output_targeting_and_watchdog(
        mode: OverlayMode,
        adaptive_width: bool,
        launcher: Arc<OverlayLauncher>,
        respawn_backoff: Duration,
        output_watchdog_timeout: Duration,
    ) -> Self {
        Self::new_with_launcher_and_backoff_and_watchdog(
            mode,
            adaptive_width,
            None,
            true,
            launcher,
            respawn_backoff,
            output_watchdog_timeout,
        )
    }
}

fn is_replayable_overlay_message(message: &OverlayIpcMessage) -> bool {
    match message {
        OverlayIpcMessage::InterimState { .. }
        | OverlayIpcMessage::InterimText { .. }
        | OverlayIpcMessage::SessionEnded { .. } => true,
        OverlayIpcMessage::InjectionComplete { .. }
        | OverlayIpcMessage::OutputHint { .. }
        | OverlayIpcMessage::AudioLevel { .. } => false,
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
        Arc::new(move |_mode, _output_name, _adaptive_width| {
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
        Arc::new(move |_mode, output_name, _adaptive_width| {
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
            true,
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
            true,
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
    async fn manager_replay_ignores_audio_level_as_latest_state() {
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
            true,
            launcher,
            Duration::from_millis(0),
        );

        let session_id = Uuid::new_v4();
        let state_message = OverlayIpcMessage::InterimText {
            session_id,
            seq: 2,
            text: "current-state".to_string(),
        };
        manager.send(state_message.clone());
        let _ = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive state")
            .expect("first sink should remain open");

        let audio_level_message = OverlayIpcMessage::AudioLevel {
            session_id,
            level_db: -28.0,
        };
        manager.send(audio_level_message.clone());
        let _ = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive audio level")
            .expect("first sink should remain open");

        drop(rx_first);

        manager.send(audio_level_message);

        let replayed = timeout(Duration::from_millis(100), rx_second.recv())
            .await
            .expect("second sink should receive replay")
            .expect("second sink should remain open");
        assert_eq!(replayed, state_message);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manager_replay_ignores_injection_complete_as_latest_state() {
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
            true,
            launcher,
            Duration::from_millis(0),
        );

        let session_id = Uuid::new_v4();
        let state_message = OverlayIpcMessage::InterimText {
            session_id,
            seq: 2,
            text: "current-state".to_string(),
        };
        manager.send(state_message.clone());
        let _ = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive state")
            .expect("first sink should remain open");

        let injection_complete_message = OverlayIpcMessage::InjectionComplete {
            session_id,
            success: true,
        };
        manager.send(injection_complete_message.clone());
        let _ = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive injection complete")
            .expect("first sink should remain open");

        drop(rx_first);

        manager.send(injection_complete_message);

        let replayed = timeout(Duration::from_millis(100), rx_second.recv())
            .await
            .expect("second sink should receive replay")
            .expect("second sink should remain open");
        assert_eq!(replayed, state_message);
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
            true,
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

    #[tokio::test(flavor = "current_thread")]
    async fn output_watchdog_spawns_once_without_output_targeting() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = OverlayProcessSink::from_sender_for_tests(
            tx,
            Arc::new(OverlayProcessMetrics::default()),
        );
        let queue = Arc::new(Mutex::new(VecDeque::from([Ok(sink)])));
        let seen_output_names = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let launcher = recording_launcher(Arc::clone(&seen_output_names), queue);
        let mut manager = OverlayProcessManager::new_for_tests_with_output_targeting_and_watchdog(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
            Duration::ZERO,
        );

        let state = OverlayIpcMessage::InterimText {
            session_id: Uuid::new_v4(),
            seq: 1,
            text: "watchdog-fallback".to_string(),
        };
        manager.send(state.clone());
        assert!(manager.has_active_sink());
        assert_eq!(
            manager
                .metrics()
                .spawn_attempt_total
                .load(Ordering::Relaxed),
            1,
            "watchdog should trigger exactly one fallback spawn attempt"
        );
        assert_eq!(
            seen_output_names
                .lock()
                .expect("recorded output names lock should be available")
                .clone(),
            vec![None],
            "watchdog fallback spawn should omit output targeting exactly once"
        );

        let received = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("state should be delivered to fallback spawn sink")
            .expect("sink should remain open");
        assert_eq!(received, state);
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());
    }

    #[test]
    fn missing_overlay_binary_spawn_failures_remain_non_fatal() {
        let launcher: Arc<OverlayLauncher> = Arc::new(|_mode, _output_name, _adaptive_width| {
            Err(anyhow!(
                "failed to spawn overlay process '/tmp/parakeet-overlay': No such file or directory"
            ))
        });
        let mut manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );

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

    #[tokio::test(flavor = "current_thread")]
    async fn manager_passes_adaptive_width_to_launcher() {
        let seen_adaptive_width = Arc::new(Mutex::new(Vec::<bool>::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = OverlayProcessSink::from_sender_for_tests(
            tx,
            Arc::new(OverlayProcessMetrics::default()),
        );
        let queue = Arc::new(Mutex::new(VecDeque::from([Ok(sink)])));
        let launcher: Arc<OverlayLauncher> = {
            let seen_adaptive_width = Arc::clone(&seen_adaptive_width);
            Arc::new(move |_mode, _output_name, adaptive_width| {
                seen_adaptive_width
                    .lock()
                    .expect("adaptive width lock should be available")
                    .push(adaptive_width);
                queue
                    .lock()
                    .expect("spawn queue lock should be available")
                    .pop_front()
                    .unwrap_or_else(|| Err(anyhow!("no spawn outcome configured")))
            })
        };
        let mut manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            false,
            launcher,
            Duration::ZERO,
        );

        manager.send(OverlayIpcMessage::InterimText {
            session_id: Uuid::new_v4(),
            seq: 1,
            text: "check adaptive width forwarding".to_string(),
        });

        assert_eq!(
            seen_adaptive_width
                .lock()
                .expect("adaptive width lock should be available")
                .as_slice(),
            &[false]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manager_retargets_only_on_next_utterance_boundary() {
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
        let seen_output_names = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let launcher = recording_launcher(Arc::clone(&seen_output_names), queue);
        let mut manager = OverlayProcessManager::new_for_tests_with_output_targeting(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );

        manager.send(OverlayIpcMessage::OutputHint {
            output_name: "DP-1".to_string(),
        });
        let session_a = Uuid::new_v4();
        manager.send(OverlayIpcMessage::InterimState {
            session_id: session_a,
            seq: 1,
            state: "listening".to_string(),
        });
        manager.send(OverlayIpcMessage::OutputHint {
            output_name: "HDMI-A-1".to_string(),
        });
        manager.send(OverlayIpcMessage::InterimText {
            session_id: session_a,
            seq: 2,
            text: "still session a".to_string(),
        });
        manager.send(OverlayIpcMessage::SessionEnded {
            session_id: session_a,
            reason: None,
        });

        let first_hint = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive initial hint")
            .expect("first sink should remain open");
        assert_eq!(
            first_hint,
            OverlayIpcMessage::OutputHint {
                output_name: "DP-1".to_string()
            }
        );
        let first_interim = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive first utterance state")
            .expect("first sink should remain open");
        assert!(matches!(
            first_interim,
            OverlayIpcMessage::InterimState {
                session_id,
                seq: 1,
                ..
            } if session_id == session_a
        ));
        let changed_hint = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive changed output hint during active utterance")
            .expect("first sink should remain open");
        assert_eq!(
            changed_hint,
            OverlayIpcMessage::OutputHint {
                output_name: "HDMI-A-1".to_string()
            }
        );

        let first_text = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive interim text")
            .expect("first sink should remain open");
        assert!(matches!(
            first_text,
            OverlayIpcMessage::InterimText {
                session_id,
                seq: 2,
                ..
            } if session_id == session_a
        ));

        let first_end = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive session end")
            .expect("first sink should remain open");
        assert!(matches!(
            first_end,
            OverlayIpcMessage::SessionEnded { session_id, .. } if session_id == session_a
        ));

        let session_b = Uuid::new_v4();
        manager.send(OverlayIpcMessage::InterimState {
            session_id: session_b,
            seq: 1,
            state: "listening".to_string(),
        });

        let retargeted_interim = timeout(Duration::from_millis(100), rx_second.recv())
            .await
            .expect("second sink should receive first event of next utterance")
            .expect("second sink should remain open");
        assert!(matches!(
            retargeted_interim,
            OverlayIpcMessage::InterimState {
                session_id,
                seq: 1,
                ..
            } if session_id == session_b
        ));

        assert_eq!(
            seen_output_names
                .lock()
                .expect("recorded output names lock should be available")
                .clone(),
            vec![Some("DP-1".to_string()), Some("HDMI-A-1".to_string())]
        );
    }
}
