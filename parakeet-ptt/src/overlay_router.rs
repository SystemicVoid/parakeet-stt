//! Client-side Overlay routing for Daemon Session events.
//!
//! This Module owns the policy for filtering stale or mismatched Session
//! events before they reach the Overlay Implementation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::{debug, info};
use uuid::Uuid;

use crate::config::OverlayMode;
use crate::overlay_process::OverlayProcessManager;
use crate::surface_focus::WaylandFocusCache;
use parakeet_ptt::overlay_ipc::OverlayIpcMessage;
pub(crate) use parakeet_ptt::overlay_ipc::OverlayTextProducer;

#[derive(Debug, Default)]
struct OverlayRoutingMetrics {
    routed_interim_state_total: AtomicU64,
    routed_interim_text_total: AtomicU64,
    routed_session_ended_total: AtomicU64,
    dropped_stale_seq_total: AtomicU64,
    dropped_session_mismatch_total: AtomicU64,
}

impl OverlayRoutingMetrics {
    fn note_interim_state(&self) {
        self.routed_interim_state_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_interim_text(&self) {
        self.routed_interim_text_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_session_ended(&self) {
        self.routed_session_ended_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_stale_seq_drop(&self) {
        self.dropped_stale_seq_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_session_mismatch_drop(&self) {
        self.dropped_session_mismatch_total
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OverlayEvent {
    OutputHint {
        output_name: String,
    },
    InterimState {
        producer: OverlayTextProducer,
        session_id: Uuid,
        seq: u64,
        state: String,
    },
    InterimText {
        producer: OverlayTextProducer,
        session_id: Uuid,
        seq: u64,
        text: String,
    },
    AudioLevel {
        session_id: Uuid,
        level_db: f32,
    },
    SessionEnded {
        session_id: Uuid,
        reason: Option<String>,
    },
    InjectionComplete {
        session_id: Uuid,
        success: bool,
    },
    SessionWarning {
        session_id: Uuid,
        remaining_seconds: f32,
        limit_seconds: f32,
    },
}

pub trait OverlaySink: Send {
    fn on_overlay_event(&mut self, event: OverlayEvent);
}

impl<T: OverlaySink + ?Sized> OverlaySink for Box<T> {
    fn on_overlay_event(&mut self, event: OverlayEvent) {
        self.as_mut().on_overlay_event(event);
    }
}

#[derive(Debug, Default)]
pub struct NoopOverlaySink;

impl OverlaySink for NoopOverlaySink {
    fn on_overlay_event(&mut self, event: OverlayEvent) {
        debug!(?event, "overlay event dropped by noop sink");
    }
}

pub(crate) enum RuntimeOverlaySink {
    Noop(NoopOverlaySink),
    Process(Box<OverlayProcessManager>),
}

impl OverlaySink for RuntimeOverlaySink {
    fn on_overlay_event(&mut self, event: OverlayEvent) {
        match self {
            Self::Noop(sink) => sink.on_overlay_event(event),
            Self::Process(manager) => manager.send(overlay_event_to_ipc(event)),
        }
    }
}

fn overlay_event_to_ipc(event: OverlayEvent) -> OverlayIpcMessage {
    match event {
        OverlayEvent::OutputHint { output_name } => OverlayIpcMessage::OutputHint { output_name },
        OverlayEvent::InterimState {
            producer,
            session_id,
            seq,
            state,
        } => OverlayIpcMessage::InterimState {
            session_id,
            producer,
            seq,
            state,
        },
        OverlayEvent::InterimText {
            producer,
            session_id,
            seq,
            text,
        } => OverlayIpcMessage::InterimText {
            session_id,
            producer,
            seq,
            text,
        },
        OverlayEvent::AudioLevel {
            session_id,
            level_db,
        } => OverlayIpcMessage::AudioLevel {
            session_id,
            level_db,
        },
        OverlayEvent::SessionEnded { session_id, reason } => {
            OverlayIpcMessage::SessionEnded { session_id, reason }
        }
        OverlayEvent::InjectionComplete {
            session_id,
            success,
        } => OverlayIpcMessage::InjectionComplete {
            session_id,
            success,
        },
        OverlayEvent::SessionWarning {
            session_id,
            remaining_seconds,
            limit_seconds,
        } => OverlayIpcMessage::SessionWarning {
            session_id,
            remaining_seconds: Some(remaining_seconds),
            limit_seconds: Some(limit_seconds),
        },
    }
}

pub(crate) fn build_runtime_overlay_sink(
    mode: OverlayMode,
    overlay_adaptive_width: bool,
    focus_cache: Option<WaylandFocusCache>,
) -> Box<dyn OverlaySink> {
    match mode {
        OverlayMode::Disabled => Box::new(RuntimeOverlaySink::Noop(NoopOverlaySink)),
        OverlayMode::LayerShell | OverlayMode::FallbackWindow => {
            let manager = OverlayProcessManager::new(mode, overlay_adaptive_width, focus_cache);
            let metrics = manager.metrics();
            info!(
                overlay_spawn_attempt_total = metrics.spawn_attempt_total.load(Ordering::Relaxed),
                overlay_spawn_success_total = metrics.spawn_success_total.load(Ordering::Relaxed),
                overlay_spawn_failure_total = metrics.spawn_failure_total.load(Ordering::Relaxed),
                overlay_active_sink = manager.has_active_sink(),
                overlay_adaptive_width,
                "overlay process routing enabled with respawn manager"
            );
            Box::new(RuntimeOverlaySink::Process(Box::new(manager)))
        }
    }
}

pub(crate) struct OverlayRouter<S: OverlaySink> {
    sink: S,
    metrics: Arc<OverlayRoutingMetrics>,
    active_session_id: Option<Uuid>,
    last_seq_by_producer: HashMap<OverlayTextProducer, u64>,
}

impl<S: OverlaySink> OverlayRouter<S> {
    pub(crate) fn new(sink: S) -> Self {
        Self {
            sink,
            metrics: Arc::new(OverlayRoutingMetrics::default()),
            active_session_id: None,
            last_seq_by_producer: HashMap::new(),
        }
    }

    #[cfg(test)]
    fn metrics(&self) -> &Arc<OverlayRoutingMetrics> {
        &self.metrics
    }

    pub(crate) fn note_session_started(&mut self, session_id: Uuid) {
        if self.active_session_id != Some(session_id) {
            self.active_session_id = Some(session_id);
            self.last_seq_by_producer.clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn route_output_hint(&mut self, output_name: String) {
        self.route_optional_output_hint(Some(output_name));
    }

    fn route_optional_output_hint(&mut self, output_name: Option<String>) {
        let Some(output_name) = output_name else {
            return;
        };

        self.sink
            .on_overlay_event(OverlayEvent::OutputHint { output_name });
    }

    #[cfg(test)]
    pub(crate) fn route_daemon_interim_state(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        state: String,
    ) {
        self.route_daemon_interim_state_with_output_hint(
            expected_session_id,
            session_id,
            seq,
            state,
            || None,
        );
    }

    pub(crate) fn route_daemon_interim_state_with_output_hint(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        state: String,
        output_hint: impl FnOnce() -> Option<String>,
    ) {
        let Some(expected_session_id) = expected_session_id else {
            self.metrics.note_session_mismatch_drop();
            debug!(
                incoming_session = %session_id,
                "dropping daemon interim state without an active session"
            );
            return;
        };

        self.route_interim_state(
            Some(expected_session_id),
            session_id,
            seq,
            state,
            OverlayTextProducer::DaemonSttInterim,
            output_hint,
        );
    }

    #[cfg(test)]
    pub(crate) fn route_daemon_interim_text(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        text: String,
    ) {
        self.route_daemon_interim_text_with_output_hint(
            expected_session_id,
            session_id,
            seq,
            text,
            || None,
        );
    }

    pub(crate) fn route_daemon_interim_text_with_output_hint(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        text: String,
        output_hint: impl FnOnce() -> Option<String>,
    ) {
        let Some(expected_session_id) = expected_session_id else {
            self.metrics.note_session_mismatch_drop();
            debug!(
                incoming_session = %session_id,
                "dropping daemon interim text without an active session"
            );
            return;
        };

        self.route_interim_text(
            Some(expected_session_id),
            session_id,
            seq,
            text,
            OverlayTextProducer::DaemonSttInterim,
            output_hint,
        );
    }

    pub(crate) fn route_llm_answer_state_with_output_hint(
        &mut self,
        session_id: Uuid,
        seq: u64,
        state: String,
        output_hint: impl FnOnce() -> Option<String>,
    ) {
        self.route_interim_state(
            None,
            session_id,
            seq,
            state,
            OverlayTextProducer::LlmAnswerDelta,
            output_hint,
        );
    }

    #[cfg(test)]
    pub(crate) fn route_llm_answer_delta(&mut self, session_id: Uuid, seq: u64, text: String) {
        self.route_llm_answer_delta_with_output_hint(session_id, seq, text, || None);
    }

    pub(crate) fn route_llm_answer_delta_with_output_hint(
        &mut self,
        session_id: Uuid,
        seq: u64,
        text: String,
        output_hint: impl FnOnce() -> Option<String>,
    ) {
        self.route_interim_text(
            None,
            session_id,
            seq,
            text,
            OverlayTextProducer::LlmAnswerDelta,
            output_hint,
        );
    }

    pub(crate) fn route_audio_level(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        level_db: f32,
    ) {
        if !self.allow_session(expected_session_id, session_id) {
            return;
        }

        self.sink.on_overlay_event(OverlayEvent::AudioLevel {
            session_id,
            level_db,
        });
    }

    pub(crate) fn route_session_ended(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        reason: Option<String>,
    ) {
        if !self.allow_session(expected_session_id, session_id) {
            return;
        }

        self.sink
            .on_overlay_event(OverlayEvent::SessionEnded { session_id, reason });
        self.metrics.note_session_ended();

        if self.active_session_id == Some(session_id) {
            self.active_session_id = None;
            self.last_seq_by_producer.clear();
        }
    }

    fn route_interim_state(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        state: String,
        producer: OverlayTextProducer,
        output_hint: impl FnOnce() -> Option<String>,
    ) {
        if !self.allow_session(expected_session_id, session_id)
            || !self.accept_seq(session_id, seq, &producer)
        {
            return;
        }

        debug!(
            session = %session_id,
            seq,
            overlay_text_producer = producer.as_str(),
            "routing overlay interim state"
        );
        self.route_optional_output_hint(output_hint());
        self.sink.on_overlay_event(OverlayEvent::InterimState {
            producer,
            session_id,
            seq,
            state,
        });
        self.metrics.note_interim_state();
    }

    fn route_interim_text(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        text: String,
        producer: OverlayTextProducer,
        output_hint: impl FnOnce() -> Option<String>,
    ) {
        if !self.allow_session(expected_session_id, session_id)
            || !self.accept_seq(session_id, seq, &producer)
        {
            return;
        }

        debug!(
            session = %session_id,
            seq,
            overlay_text_producer = producer.as_str(),
            text_chars = text.chars().count(),
            "routing overlay interim text"
        );
        self.route_optional_output_hint(output_hint());
        self.sink.on_overlay_event(OverlayEvent::InterimText {
            producer,
            session_id,
            seq,
            text,
        });
        self.metrics.note_interim_text();
    }

    pub(crate) fn route_injection_complete(&mut self, session_id: Uuid, success: bool) {
        self.sink.on_overlay_event(OverlayEvent::InjectionComplete {
            session_id,
            success,
        });
    }

    pub(crate) fn route_session_warning(
        &mut self,
        session_id: Uuid,
        remaining_seconds: f32,
        limit_seconds: f32,
    ) {
        if self.active_session_id != Some(session_id) {
            return;
        }
        self.sink.on_overlay_event(OverlayEvent::SessionWarning {
            session_id,
            remaining_seconds,
            limit_seconds,
        });
    }

    fn allow_session(&self, expected_session_id: Option<Uuid>, incoming_session_id: Uuid) -> bool {
        match expected_session_id {
            Some(expected) if expected != incoming_session_id => {
                self.metrics.note_session_mismatch_drop();
                debug!(
                    expected_session = %expected,
                    incoming_session = %incoming_session_id,
                    "dropping overlay event for mismatched active session"
                );
                false
            }
            _ => true,
        }
    }

    fn accept_seq(
        &mut self,
        incoming_session_id: Uuid,
        seq: u64,
        producer: &OverlayTextProducer,
    ) -> bool {
        if self.active_session_id != Some(incoming_session_id) {
            self.active_session_id = Some(incoming_session_id);
            self.last_seq_by_producer.clear();
        }

        if let Some(last_seq) = self.last_seq_by_producer.get(producer) {
            if seq <= *last_seq {
                self.metrics.note_stale_seq_drop();
                debug!(
                    session = %incoming_session_id,
                    seq,
                    last_seq,
                    overlay_text_producer = producer.as_str(),
                    "dropping stale overlay event sequence"
                );
                return false;
            }
        }

        self.last_seq_by_producer.insert(*producer, seq);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    use uuid::Uuid;

    use super::{OverlayEvent, OverlayRouter, OverlaySink, OverlayTextProducer};

    struct RecordingOverlaySink {
        seen: Arc<Mutex<Vec<OverlayEvent>>>,
    }

    impl RecordingOverlaySink {
        fn new() -> (Self, Arc<Mutex<Vec<OverlayEvent>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    seen: Arc::clone(&seen),
                },
                seen,
            )
        }
    }

    impl OverlaySink for RecordingOverlaySink {
        fn on_overlay_event(&mut self, event: OverlayEvent) {
            self.seen
                .lock()
                .expect("overlay recording lock should be available")
                .push(event);
        }
    }

    #[test]
    fn stale_interim_sequences_are_dropped_near_router() {
        let session_id = Uuid::new_v4();
        let (sink, seen) = RecordingOverlaySink::new();
        let mut router = OverlayRouter::new(sink);
        router.note_session_started(session_id);

        router.route_daemon_interim_text(Some(session_id), session_id, 10, "newest".to_string());
        router.route_daemon_interim_text(Some(session_id), session_id, 9, "stale".to_string());

        assert_eq!(
            router
                .metrics()
                .dropped_stale_seq_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            seen.lock()
                .expect("overlay recording lock should be available")
                .clone(),
            vec![OverlayEvent::InterimText {
                producer: OverlayTextProducer::DaemonSttInterim,
                session_id,
                seq: 10,
                text: "newest".to_string(),
            }]
        );
    }

    #[test]
    fn daemon_interim_requires_active_client_session() {
        let session_id = Uuid::new_v4();
        let (sink, seen) = RecordingOverlaySink::new();
        let mut router = OverlayRouter::new(sink);

        router.route_daemon_interim_state(None, session_id, 1, "listening".to_string());
        router.route_daemon_interim_text(None, session_id, 2, "late daemon".to_string());

        assert_eq!(
            router
                .metrics()
                .dropped_session_mismatch_total
                .load(Ordering::Relaxed),
            2
        );
        assert!(seen
            .lock()
            .expect("overlay recording lock should be available")
            .is_empty());
    }

    #[test]
    fn mismatched_session_events_are_dropped_near_router() {
        let active_session_id = Uuid::new_v4();
        let stale_session_id = Uuid::new_v4();
        let (sink, seen) = RecordingOverlaySink::new();
        let mut router = OverlayRouter::new(sink);
        router.note_session_started(active_session_id);

        router.route_daemon_interim_text(
            Some(active_session_id),
            stale_session_id,
            1,
            "stale session".to_string(),
        );
        router.route_daemon_interim_text(
            Some(active_session_id),
            active_session_id,
            1,
            "active session".to_string(),
        );

        assert_eq!(
            router
                .metrics()
                .dropped_session_mismatch_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            seen.lock()
                .expect("overlay recording lock should be available")
                .clone(),
            vec![OverlayEvent::InterimText {
                producer: OverlayTextProducer::DaemonSttInterim,
                session_id: active_session_id,
                seq: 1,
                text: "active session".to_string(),
            }]
        );
    }

    #[test]
    fn daemon_and_llm_overlay_text_sequences_are_independent() {
        let session_id = Uuid::new_v4();
        let (sink, seen) = RecordingOverlaySink::new();
        let mut router = OverlayRouter::new(sink);
        router.note_session_started(session_id);

        router.route_daemon_interim_text(
            Some(session_id),
            session_id,
            10,
            "daemon interim".to_string(),
        );
        router.route_llm_answer_delta(session_id, 1, "answer".to_string());
        router.route_llm_answer_delta(session_id, 2, "answer delta".to_string());
        router.route_daemon_interim_text(
            Some(session_id),
            session_id,
            9,
            "stale daemon".to_string(),
        );
        router.route_llm_answer_delta(session_id, 2, "stale llm".to_string());

        assert_eq!(
            router
                .metrics()
                .dropped_stale_seq_total
                .load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            seen.lock()
                .expect("overlay recording lock should be available")
                .clone(),
            vec![
                OverlayEvent::InterimText {
                    producer: OverlayTextProducer::DaemonSttInterim,
                    session_id,
                    seq: 10,
                    text: "daemon interim".to_string(),
                },
                OverlayEvent::InterimText {
                    producer: OverlayTextProducer::LlmAnswerDelta,
                    session_id,
                    seq: 1,
                    text: "answer".to_string(),
                },
                OverlayEvent::InterimText {
                    producer: OverlayTextProducer::LlmAnswerDelta,
                    session_id,
                    seq: 2,
                    text: "answer delta".to_string(),
                },
            ]
        );
    }

    #[test]
    fn session_warning_requires_active_session() {
        let active_session_id = Uuid::new_v4();
        let inactive_session_id = Uuid::new_v4();
        let (sink, seen) = RecordingOverlaySink::new();
        let mut router = OverlayRouter::new(sink);
        router.note_session_started(active_session_id);

        router.route_session_warning(inactive_session_id, 120.0, 600.0);
        router.route_session_warning(active_session_id, 120.0, 600.0);

        assert_eq!(
            seen.lock()
                .expect("overlay recording lock should be available")
                .clone(),
            vec![OverlayEvent::SessionWarning {
                session_id: active_session_id,
                remaining_seconds: 120.0,
                limit_seconds: 600.0,
            }]
        );
    }
}
