//! Client-side Overlay routing for Daemon Session events.
//!
//! This Module owns the policy for filtering stale or mismatched Session
//! events before they reach the Overlay Implementation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::{debug, info};
use uuid::Uuid;

use crate::config::OverlayMode;
use crate::overlay_process::OverlayProcessManager;
use crate::surface_focus::WaylandFocusCache;
use parakeet_ptt::overlay_ipc::OverlayIpcMessage;

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
        session_id: Uuid,
        seq: u64,
        state: String,
    },
    InterimText {
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
            session_id,
            seq,
            state,
        } => OverlayIpcMessage::InterimState {
            session_id,
            seq,
            state,
        },
        OverlayEvent::InterimText {
            session_id,
            seq,
            text,
        } => OverlayIpcMessage::InterimText {
            session_id,
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
        OverlayEvent::SessionWarning { session_id } => {
            OverlayIpcMessage::SessionWarning { session_id }
        }
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
    last_seq: Option<u64>,
    focus_cache: Option<WaylandFocusCache>,
    last_output_name: Option<String>,
}

impl<S: OverlaySink> OverlayRouter<S> {
    pub(crate) fn new(sink: S, focus_cache: Option<WaylandFocusCache>) -> Self {
        Self {
            sink,
            metrics: Arc::new(OverlayRoutingMetrics::default()),
            active_session_id: None,
            last_seq: None,
            focus_cache,
            last_output_name: None,
        }
    }

    #[cfg(test)]
    fn metrics(&self) -> &Arc<OverlayRoutingMetrics> {
        &self.metrics
    }

    pub(crate) fn note_session_started(&mut self, session_id: Uuid) {
        if self.active_session_id != Some(session_id) {
            self.active_session_id = Some(session_id);
            self.last_seq = None;
            self.last_output_name = None;
        }
    }

    pub(crate) fn route_interim_state(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        state: String,
    ) {
        if !self.allow_session(expected_session_id, session_id) || !self.accept_seq(session_id, seq)
        {
            return;
        }

        self.maybe_emit_output_hint();
        self.sink.on_overlay_event(OverlayEvent::InterimState {
            session_id,
            seq,
            state,
        });
        self.metrics.note_interim_state();
    }

    pub(crate) fn route_interim_text(
        &mut self,
        expected_session_id: Option<Uuid>,
        session_id: Uuid,
        seq: u64,
        text: String,
    ) {
        if !self.allow_session(expected_session_id, session_id) || !self.accept_seq(session_id, seq)
        {
            return;
        }

        self.maybe_emit_output_hint();
        self.sink.on_overlay_event(OverlayEvent::InterimText {
            session_id,
            seq,
            text,
        });
        self.metrics.note_interim_text();
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
            self.last_seq = None;
            self.last_output_name = None;
        }
    }

    pub(crate) fn route_injection_complete(&mut self, session_id: Uuid, success: bool) {
        self.sink.on_overlay_event(OverlayEvent::InjectionComplete {
            session_id,
            success,
        });
    }

    pub(crate) fn route_session_warning(&mut self, session_id: Uuid) {
        if self.active_session_id != Some(session_id) {
            return;
        }
        self.sink
            .on_overlay_event(OverlayEvent::SessionWarning { session_id });
    }

    fn maybe_emit_output_hint(&mut self) {
        let Some(focus_cache) = self.focus_cache.as_ref() else {
            return;
        };

        let Some(output_name) = focus_cache.current_output_name() else {
            return;
        };

        if self.last_output_name.as_deref() == Some(output_name.as_str()) {
            return;
        }

        self.last_output_name = Some(output_name.clone());
        self.sink
            .on_overlay_event(OverlayEvent::OutputHint { output_name });
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

    fn accept_seq(&mut self, incoming_session_id: Uuid, seq: u64) -> bool {
        if self.active_session_id != Some(incoming_session_id) {
            self.active_session_id = Some(incoming_session_id);
            self.last_seq = None;
        }

        if let Some(last_seq) = self.last_seq {
            if seq <= last_seq {
                self.metrics.note_stale_seq_drop();
                debug!(
                    session = %incoming_session_id,
                    seq,
                    last_seq,
                    "dropping stale overlay event sequence"
                );
                return false;
            }
        }

        self.last_seq = Some(seq);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    use uuid::Uuid;

    use super::{OverlayEvent, OverlayRouter, OverlaySink};

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
        let mut router = OverlayRouter::new(sink, None);

        router.route_interim_text(None, session_id, 10, "newest".to_string());
        router.route_interim_text(None, session_id, 9, "stale".to_string());

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
                session_id,
                seq: 10,
                text: "newest".to_string(),
            }]
        );
    }

    #[test]
    fn mismatched_session_events_are_dropped_near_router() {
        let active_session_id = Uuid::new_v4();
        let stale_session_id = Uuid::new_v4();
        let (sink, seen) = RecordingOverlaySink::new();
        let mut router = OverlayRouter::new(sink, None);
        router.note_session_started(active_session_id);

        router.route_interim_text(
            Some(active_session_id),
            stale_session_id,
            1,
            "stale session".to_string(),
        );
        router.route_interim_text(
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
                session_id: active_session_id,
                seq: 1,
                text: "active session".to_string(),
            }]
        );
    }

    #[test]
    fn session_warning_requires_active_session() {
        let active_session_id = Uuid::new_v4();
        let inactive_session_id = Uuid::new_v4();
        let (sink, seen) = RecordingOverlaySink::new();
        let mut router = OverlayRouter::new(sink, None);
        router.note_session_started(active_session_id);

        router.route_session_warning(inactive_session_id);
        router.route_session_warning(active_session_id);

        assert_eq!(
            seen.lock()
                .expect("overlay recording lock should be available")
                .clone(),
            vec![OverlayEvent::SessionWarning {
                session_id: active_session_id,
            }]
        );
    }
}
