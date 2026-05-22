//! Client Session dispatch policy for Daemon messages.
//!
//! This Module owns per-Session Client runtime state and how Daemon
//! `ServerMessage` values affect Client PTT state, Overlay routing, Injection
//! enqueueing, LLM progress, and parent-focus handoff.

use std::collections::HashMap;

use anyhow::Result;
use tokio::time::Instant as TokioInstant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::injector::ParentFocusCapture;
use crate::injector_runtime::{
    EnqueueFailure, InjectionJob, InjectionOrigin, InjectorWorkerHandle,
    INJECTION_ENQUEUE_TIMEOUT_MS, INJECTION_QUEUE_CAPACITY,
};
use crate::overlay_router::{OverlayRouter, OverlaySink};
use crate::protocol::ServerMessage;
use crate::state::PttState;

#[derive(Debug, Clone)]
pub(crate) struct CapturedParentFocus {
    pub(crate) focus: ParentFocusCapture,
    pub(crate) captured_at: TokioInstant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionIntent {
    Dictate,
    LlmQuery,
}

#[derive(Debug)]
pub(crate) struct ClientSessionRuntime {
    state: PttState,
    active_intent: Option<SessionIntent>,
    llm: LlmSessionRuntime,
    parent_focus_by_session: HashMap<Uuid, CapturedParentFocus>,
    last_hotkey_up_at: Option<TokioInstant>,
    last_stop_message: Option<(Uuid, TokioInstant)>,
}

#[derive(Debug, Default)]
struct LlmSessionRuntime {
    busy: bool,
    in_flight_session: Option<Uuid>,
    seq: HashMap<Uuid, u64>,
    overlay_text: HashMap<Uuid, String>,
    deferred_session_end: HashMap<Uuid, Option<String>>,
    busy_overlay_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LlmDeltaOverlay {
    pub(crate) seq: u64,
    pub(crate) text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LlmCompletionContext {
    pub(crate) session_end_reason: Option<String>,
    pub(crate) session_end_was_deferred: bool,
    pub(crate) state_label: &'static str,
    pub(crate) hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
    pub(crate) stop_message_elapsed_ms_at_enqueue: Option<u64>,
    pub(crate) parent_focus: Option<ParentFocusCapture>,
}

impl ClientSessionRuntime {
    pub(crate) fn new() -> Self {
        Self {
            state: PttState::new(),
            active_intent: None,
            llm: LlmSessionRuntime::default(),
            parent_focus_by_session: HashMap::new(),
            last_hotkey_up_at: None,
            last_stop_message: None,
        }
    }

    pub(crate) fn begin_listening(&mut self, intent: SessionIntent) -> Option<Uuid> {
        let session_id = self.state.begin_listening()?;
        self.active_intent = Some(intent);
        Some(session_id)
    }

    pub(crate) fn stop_listening(&mut self) -> Option<Uuid> {
        self.state.stop_listening()
    }

    pub(crate) fn record_stop_message_sent(
        &mut self,
        session_id: Uuid,
        parent_focus: Option<ParentFocusCapture>,
        sent_at: TokioInstant,
    ) {
        self.last_hotkey_up_at = Some(sent_at);
        self.last_stop_message = Some((session_id, sent_at));
        if let Some(focus) = parent_focus {
            self.parent_focus_by_session.insert(
                session_id,
                CapturedParentFocus {
                    focus,
                    captured_at: sent_at,
                },
            );
        }
    }

    pub(crate) fn reset(&mut self) {
        self.state.reset();
        self.active_intent = None;
    }

    pub(crate) fn reset_for_connection_drop(&mut self) {
        self.reset();
        self.llm.clear();
        self.parent_focus_by_session.clear();
        self.last_hotkey_up_at = None;
        self.last_stop_message = None;
    }

    pub(crate) fn active_intent(&self) -> Option<SessionIntent> {
        self.active_intent
    }

    pub(crate) fn active_session_id(&self) -> Option<Uuid> {
        session_id_from_state(&self.state)
    }

    pub(crate) fn state_label(&self) -> &'static str {
        state_label(&self.state)
    }

    pub(crate) fn is_llm_busy(&self) -> bool {
        self.llm.busy
    }

    pub(crate) fn note_llm_busy_overlay_rejection(&mut self) -> (Uuid, u64) {
        self.llm.busy_overlay_seq = self.llm.busy_overlay_seq.saturating_add(1);
        (Uuid::nil(), self.llm.busy_overlay_seq)
    }

    pub(crate) fn defer_session_end_if_needed(&mut self, message: &ServerMessage) -> Option<Uuid> {
        let ServerMessage::SessionEnded { session_id, reason } = message else {
            return None;
        };

        let waiting_for_llm_final = self.active_intent == Some(SessionIntent::LlmQuery)
            && self.active_session_id() == Some(*session_id);
        let llm_generation_running = self.llm.in_flight_session == Some(*session_id);

        if waiting_for_llm_final || llm_generation_running {
            self.llm
                .deferred_session_end
                .insert(*session_id, reason.clone());
            Some(*session_id)
        } else {
            None
        }
    }

    pub(crate) fn start_llm_answer(&mut self, session_id: Uuid) -> u64 {
        self.llm.busy = true;
        self.llm.in_flight_session = Some(session_id);
        let seq = self.llm.next_seq(session_id);
        self.state.reset();
        self.active_intent = None;
        seq
    }

    pub(crate) fn record_llm_delta(
        &mut self,
        session_id: Uuid,
        delta: String,
    ) -> Option<LlmDeltaOverlay> {
        if self.llm.in_flight_session != Some(session_id) {
            debug!(
                session = %session_id,
                in_flight_session = ?self.llm.in_flight_session,
                "ignoring stale llm delta for non-active session"
            );
            return None;
        }

        let text = {
            let entry = self.llm.overlay_text.entry(session_id).or_default();
            entry.push_str(&delta);
            entry.clone()
        };
        let seq = self.llm.next_seq(session_id);
        Some(LlmDeltaOverlay { seq, text })
    }

    pub(crate) fn finish_llm_answer(&mut self, session_id: Uuid) -> Option<LlmCompletionContext> {
        if self.llm.in_flight_session != Some(session_id) {
            warn!(
                session = %session_id,
                in_flight_session = ?self.llm.in_flight_session,
                "ignoring stale llm completion for non-active session"
            );
            self.llm.clear_session(session_id);
            return None;
        }

        self.llm.busy = false;
        self.llm.in_flight_session = None;
        self.llm.seq.remove(&session_id);
        self.llm.overlay_text.remove(&session_id);
        let session_end_reason = self.llm.deferred_session_end.remove(&session_id).flatten();
        let session_end_was_deferred = session_end_reason.is_some();
        Some(LlmCompletionContext {
            session_end_reason,
            session_end_was_deferred,
            state_label: self.state_label(),
            hotkey_up_elapsed_ms_at_enqueue: elapsed_ms_since(self.last_hotkey_up_at),
            stop_message_elapsed_ms_at_enqueue: self.stop_message_elapsed_ms_at_enqueue(session_id),
            parent_focus: self.take_parent_focus_for_enqueue(session_id),
        })
    }

    pub(crate) fn final_result_belongs_to_active_session(&self, session_id: Uuid) -> bool {
        self.active_session_id() == Some(session_id)
    }

    pub(crate) fn log_rejected_final_result(&self, session_id: Uuid, origin: InjectionOrigin) {
        warn!(
            session = %session_id,
            active_session = ?self.active_session_id(),
            origin = origin.as_str(),
            state_at_receive = self.state_label(),
            "ignoring final result for non-active session"
        );
    }

    fn take_parent_focus_for_enqueue(&mut self, session_id: Uuid) -> Option<ParentFocusCapture> {
        self.parent_focus_by_session
            .remove(&session_id)
            .map(|captured| {
                let mut focus = captured.focus;
                focus.captured_elapsed_ms = Some(captured.captured_at.elapsed().as_millis() as u64);
                focus
            })
    }

    fn remove_parent_focus(&mut self, session_id: Uuid) {
        self.parent_focus_by_session.remove(&session_id);
    }

    fn stop_message_elapsed_ms_at_enqueue(&self, session_id: Uuid) -> Option<u64> {
        self.last_stop_message
            .and_then(|(stopped_session_id, instant)| {
                (stopped_session_id == session_id).then(|| instant.elapsed().as_millis() as u64)
            })
    }
}

impl Default for ClientSessionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmSessionRuntime {
    fn next_seq(&mut self, session_id: Uuid) -> u64 {
        let seq = self.seq.entry(session_id).or_insert(0);
        *seq = seq.saturating_add(1);
        *seq
    }

    fn clear_session(&mut self, session_id: Uuid) {
        self.seq.remove(&session_id);
        self.overlay_text.remove(&session_id);
        self.deferred_session_end.remove(&session_id);
    }

    fn clear(&mut self) {
        self.busy = false;
        self.in_flight_session = None;
        self.seq.clear();
        self.overlay_text.clear();
        self.deferred_session_end.clear();
    }
}

pub(crate) async fn handle_server_message<S: OverlaySink>(
    message: ServerMessage,
    runtime: &mut ClientSessionRuntime,
    overlay_router: &mut OverlayRouter<S>,
    injector_worker: &InjectorWorkerHandle,
) -> Result<()> {
    match message {
        ServerMessage::SessionStarted { session_id, .. } => {
            info!(session = %session_id, "session started ack");
            overlay_router.note_session_started(session_id);
        }
        ServerMessage::FinalResult {
            session_id,
            text,
            latency_ms,
            audio_ms,
            ..
        } => {
            if !runtime.final_result_belongs_to_active_session(session_id) {
                runtime.log_rejected_final_result(session_id, InjectionOrigin::RawFinalResult);
                return Ok(());
            }

            let hotkey_up_elapsed_ms_at_enqueue = elapsed_ms_since(runtime.last_hotkey_up_at);
            let stop_message_elapsed_ms_at_enqueue =
                runtime.stop_message_elapsed_ms_at_enqueue(session_id);
            info!(
                session = %session_id,
                origin = InjectionOrigin::RawFinalResult.as_str(),
                daemon_latency_ms = latency_ms,
                audio_ms,
                state_at_enqueue = runtime.state_label(),
                hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue,
                "final result received"
            );
            match injector_worker
                .enqueue(
                    InjectionJob::new(session_id, text, latency_ms, audio_ms)
                        .with_origin(InjectionOrigin::RawFinalResult)
                        .with_enqueue_timing(
                            hotkey_up_elapsed_ms_at_enqueue,
                            stop_message_elapsed_ms_at_enqueue,
                        )
                        .with_parent_focus(runtime.take_parent_focus_for_enqueue(session_id)),
                )
                .await
            {
                Ok(()) => {
                    debug!(session = %session_id, "final result queued for injector worker");
                }
                Err(EnqueueFailure::Timeout) => {
                    warn!(
                        session = %session_id,
                        queue_capacity = INJECTION_QUEUE_CAPACITY,
                        enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                        "injector queue remained full; dropping final result injection job"
                    );
                }
                Err(EnqueueFailure::WorkerGone) => {
                    warn!(
                        session = %session_id,
                        "injector worker unavailable; dropping final result injection job"
                    );
                }
            }
            runtime.reset();
        }
        ServerMessage::Error {
            session_id,
            code,
            message,
        } => {
            let error_kind = classify_error_code(&code);
            warn!(
                session = ?session_id,
                error_code = %code,
                error_kind,
                "daemon error: {}",
                message
            );
            if let Some(session_id) = session_id {
                runtime.remove_parent_focus(session_id);
            }
            runtime.reset();
        }
        ServerMessage::InterimState {
            session_id,
            seq,
            state: interim_state,
        } => {
            overlay_router.route_daemon_interim_state(
                runtime.active_session_id(),
                session_id,
                seq,
                interim_state,
            );
        }
        ServerMessage::InterimText {
            session_id,
            seq,
            text,
        } => {
            overlay_router.route_daemon_interim_text(
                runtime.active_session_id(),
                session_id,
                seq,
                text,
            );
        }
        ServerMessage::AudioLevel {
            session_id,
            level_db,
        } => {
            overlay_router.route_audio_level(runtime.active_session_id(), session_id, level_db);
        }
        ServerMessage::SessionEnded { session_id, reason } => {
            runtime.remove_parent_focus(session_id);
            overlay_router.route_session_ended(runtime.active_session_id(), session_id, reason);
        }
        ServerMessage::SessionWarning {
            session_id,
            warning: _,
            remaining_seconds,
            limit_seconds,
        } => {
            info!(
                session = %session_id,
                remaining_seconds,
                limit_seconds,
                "session warning: approaching limit"
            );
            overlay_router.route_session_warning(session_id);
        }
        ServerMessage::Status(_) => {}
    }
    Ok(())
}

pub(crate) fn session_id_from_state(state: &PttState) -> Option<Uuid> {
    match *state {
        PttState::Idle => None,
        PttState::Listening { session_id } | PttState::WaitingResult { session_id } => {
            Some(session_id)
        }
    }
}

pub(crate) fn state_label(state: &PttState) -> &'static str {
    match state {
        PttState::Idle => "idle",
        PttState::Listening { .. } => "listening",
        PttState::WaitingResult { .. } => "waiting_result",
    }
}

pub(crate) fn elapsed_ms_since(instant: Option<TokioInstant>) -> Option<u64> {
    instant.map(|value| value.elapsed().as_millis() as u64)
}

pub(crate) fn classify_error_code(code: &str) -> &'static str {
    match code {
        "SESSION_BUSY" => "session_busy",
        "SESSION_NOT_FOUND" => "session_not_found",
        "SESSION_ABORTED" => "session_aborted",
        "AUDIO_DEVICE" => "audio_device",
        "MODEL" => "model",
        "INVALID_REQUEST" => "invalid_request",
        "UNEXPECTED" => "unexpected",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::config::OverlayMode;
    use crate::injector::ParentFocusCapture;
    use crate::injector_runtime::{
        spawn_injector_worker_with_capacity, InjectionJob, InjectionJobRunner, InjectionRunError,
        InjectionRunOutput, InjectorWorkerHandle,
    };
    use crate::overlay_process::{
        OverlayProcessManager, OverlayProcessMetrics, OverlayProcessSink,
    };
    use crate::overlay_router::{
        NoopOverlaySink, OverlayEvent, OverlayRouter, OverlaySink, OverlayTextProducer,
        RuntimeOverlaySink,
    };
    use crate::protocol::ServerMessage;
    use anyhow::anyhow;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Instant as TokioInstant};
    use uuid::Uuid;

    use super::{handle_server_message, ClientSessionRuntime, LlmDeltaOverlay, SessionIntent};

    struct SlowRunner {
        calls: Arc<AtomicU64>,
        sleep_ms: u64,
    }

    impl InjectionJobRunner for SlowRunner {
        fn run(
            &self,
            _job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(self.sleep_ms));
            Ok(InjectionRunOutput::default())
        }
    }

    struct RecordingRunner {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl InjectionJobRunner for RecordingRunner {
        fn run(
            &self,
            job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            self.seen
                .lock()
                .expect("recording lock should be available")
                .push(job.text.to_string());
            Ok(InjectionRunOutput::default())
        }
    }

    struct RecordingOverlaySink {
        seen: Arc<Mutex<Vec<OverlayEvent>>>,
    }

    impl OverlaySink for RecordingOverlaySink {
        fn on_overlay_event(&mut self, event: OverlayEvent) {
            self.seen
                .lock()
                .expect("overlay recording lock should be available")
                .push(event);
        }
    }

    async fn handle_server_message_for_tests<S: OverlaySink>(
        message: ServerMessage,
        runtime: &mut ClientSessionRuntime,
        overlay_router: &mut OverlayRouter<S>,
        injector_worker: &InjectorWorkerHandle,
    ) -> anyhow::Result<()> {
        handle_server_message(message, runtime, overlay_router, injector_worker).await
    }

    fn runtime_waiting_for_result() -> (ClientSessionRuntime, Uuid) {
        runtime_waiting_for_result_with_timing(TokioInstant::now())
    }

    fn runtime_waiting_for_result_with_timing(
        stopped_at: TokioInstant,
    ) -> (ClientSessionRuntime, Uuid) {
        let mut runtime = ClientSessionRuntime::new();
        let session_id = runtime
            .begin_listening(SessionIntent::Dictate)
            .expect("state should begin listening");
        runtime
            .stop_listening()
            .expect("state should stop listening");
        runtime.record_stop_message_sent(session_id, None, stopped_at);
        (runtime, session_id)
    }

    fn test_parent_focus() -> ParentFocusCapture {
        ParentFocusCapture {
            snapshot: None,
            source_selected: "test".to_string(),
            wayland_cache_age_ms: None,
            wayland_fallback_reason: None,
            captured_elapsed_ms: None,
        }
    }

    #[test]
    fn client_session_runtime_resets_reconnect_caches() {
        let mut runtime = ClientSessionRuntime::new();
        let session_id = runtime
            .begin_listening(SessionIntent::LlmQuery)
            .expect("llm session should start");
        runtime.stop_listening().expect("llm session should stop");
        runtime.record_stop_message_sent(
            session_id,
            Some(test_parent_focus()),
            TokioInstant::now(),
        );
        assert_eq!(
            runtime.defer_session_end_if_needed(&ServerMessage::SessionEnded {
                session_id,
                reason: Some("connection_drop".to_string()),
            }),
            Some(session_id)
        );
        assert_eq!(runtime.start_llm_answer(session_id), 1);
        assert_eq!(
            runtime.record_llm_delta(session_id, "partial".to_string()),
            Some(LlmDeltaOverlay {
                seq: 2,
                text: "partial".to_string(),
            })
        );
        assert!(runtime.is_llm_busy());
        assert_eq!(runtime.note_llm_busy_overlay_rejection(), (Uuid::nil(), 1));

        runtime.reset_for_connection_drop();

        assert_eq!(runtime.state_label(), "idle");
        assert_eq!(runtime.active_session_id(), None);
        assert_eq!(runtime.active_intent(), None);
        assert!(!runtime.is_llm_busy());
        assert_eq!(
            runtime.record_llm_delta(session_id, "late".to_string()),
            None
        );
        assert!(runtime.finish_llm_answer(session_id).is_none());
    }

    #[test]
    fn client_session_runtime_defers_session_end_until_llm_finish() {
        let mut runtime = ClientSessionRuntime::new();
        let session_id = runtime
            .begin_listening(SessionIntent::LlmQuery)
            .expect("llm session should start");
        runtime.stop_listening().expect("llm session should stop");
        runtime.record_stop_message_sent(
            session_id,
            Some(test_parent_focus()),
            TokioInstant::now(),
        );
        assert_eq!(
            runtime.defer_session_end_if_needed(&ServerMessage::SessionEnded {
                session_id,
                reason: Some("normal".to_string()),
            }),
            Some(session_id)
        );
        assert_eq!(runtime.start_llm_answer(session_id), 1);
        assert_eq!(
            runtime.record_llm_delta(session_id, "answer".to_string()),
            Some(LlmDeltaOverlay {
                seq: 2,
                text: "answer".to_string(),
            })
        );

        let completion = runtime
            .finish_llm_answer(session_id)
            .expect("active llm completion should produce injection context");

        assert_eq!(completion.session_end_reason.as_deref(), Some("normal"));
        assert!(completion.session_end_was_deferred);
        assert_eq!(completion.state_label, "idle");
        assert!(completion.hotkey_up_elapsed_ms_at_enqueue.is_some());
        assert!(completion.stop_message_elapsed_ms_at_enqueue.is_some());
        assert!(completion.parent_focus.is_some());
        assert!(!runtime.is_llm_busy());
        assert!(runtime.finish_llm_answer(session_id).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_result_enqueues_injection_job() {
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);
        let (mut runtime, session_id) = runtime_waiting_for_result();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "direct final result".to_string(),
                latency_ms: 44,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.92),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("injection report should arrive")
            .expect("report channel should stay open");
        assert!(report.error.is_none());
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .as_slice(),
            &["direct final result".to_string()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_result_report_includes_user_visible_completion_latency() {
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);
        let stop_message_at = TokioInstant::now() - Duration::from_millis(40);
        let (mut runtime, session_id) = runtime_waiting_for_result_with_timing(stop_message_at);
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);

        handle_server_message(
            ServerMessage::FinalResult {
                session_id,
                text: "timed final result".to_string(),
                latency_ms: 44,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.92),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("injection report should arrive")
            .expect("report channel should stay open");
        assert!(report.error.is_none());
        assert_eq!(report.daemon_latency_ms, 44);
        assert_eq!(
            report.enqueue_to_injection_complete_ms,
            report.total_worker_ms
        );
        assert_eq!(
            report.hotkey_up_elapsed_ms_at_completion,
            report
                .hotkey_up_elapsed_ms_at_enqueue
                .map(|elapsed| elapsed.saturating_add(report.enqueue_to_injection_complete_ms))
        );
        assert_eq!(
            report.stop_message_elapsed_ms_at_completion,
            report
                .stop_message_elapsed_ms_at_enqueue
                .map(|elapsed| elapsed.saturating_add(report.enqueue_to_injection_complete_ms))
        );
        assert!(
            report.hotkey_up_elapsed_ms_at_completion
                >= report.hotkey_up_elapsed_ms_at_worker_start
        );
        assert!(
            report.stop_message_elapsed_ms_at_completion
                >= report.stop_message_elapsed_ms_at_worker_start
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_final_result_does_not_enqueue_injection_job() {
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);
        let (mut runtime, active_session_id) = runtime_waiting_for_result();
        let stale_session_id = Uuid::new_v4();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id: stale_session_id,
                text: "stale private transcript".to_string(),
                latency_ms: 44,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.92),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("stale final result should be ignored without failing dispatch");

        assert_eq!(runtime.state_label(), "waiting_result");
        assert_eq!(runtime.active_session_id(), Some(active_session_id));
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 0);
        assert!(seen_injection
            .lock()
            .expect("recording lock should be available")
            .is_empty());
        assert!(timeout(Duration::from_millis(100), reports.recv())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_result_enqueues_without_waiting_for_injection_completion() {
        let calls = Arc::new(AtomicU64::new(0));
        let slow_injector = Arc::new(SlowRunner {
            calls: Arc::clone(&calls),
            sleep_ms: 120,
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(slow_injector, 8);

        let (mut runtime, session_id) = runtime_waiting_for_result();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);
        let message = ServerMessage::FinalResult {
            session_id,
            text: "hello from daemon".to_string(),
            latency_ms: 60,
            audio_ms: 1900,
            lang: Some("en".to_string()),
            confidence: Some(0.99),
        };

        let started = Instant::now();
        handle_server_message_for_tests(message, &mut runtime, &mut overlay_router, &worker)
            .await
            .expect("server message should enqueue successfully");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "handle_server_message should not wait for blocking injection, elapsed={elapsed:?}"
        );
        assert_eq!(runtime.state_label(), "idle");

        let report = timeout(Duration::from_secs(2), reports.recv())
            .await
            .expect("worker should report")
            .expect("report stream should remain open");
        assert!(report.error.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interim_overlay_messages_route_without_injection_enqueue() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
        let injector_seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&injector_seen),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 4);

        let (mut runtime, session_id) = runtime_waiting_for_result();
        handle_server_message_for_tests(
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "listening".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim state should route to overlay");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "hello".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim text should route to overlay");
        handle_server_message_for_tests(
            ServerMessage::SessionEnded {
                session_id,
                reason: Some("normal".to_string()),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("session ended should route to overlay");

        assert_eq!(runtime.state_label(), "waiting_result");
        assert_eq!(runtime.active_session_id(), Some(session_id));
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 0);
        assert_eq!(
            injector_seen
                .lock()
                .expect("recording lock should be available")
                .len(),
            0
        );

        let overlay_events = seen_overlay_events
            .lock()
            .expect("overlay recording lock should be available")
            .clone();
        assert_eq!(
            overlay_events,
            vec![
                OverlayEvent::InterimState {
                    producer: OverlayTextProducer::DaemonSttInterim,
                    session_id,
                    seq: 1,
                    state: "listening".to_string(),
                },
                OverlayEvent::InterimText {
                    producer: OverlayTextProducer::DaemonSttInterim,
                    session_id,
                    seq: 2,
                    text: "hello".to_string(),
                },
                OverlayEvent::SessionEnded {
                    session_id,
                    reason: Some("normal".to_string()),
                },
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_stream_enqueues_exactly_one_final_result() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);

        let (mut runtime, session_id) = runtime_waiting_for_result();
        handle_server_message_for_tests(
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "processing".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim state should route");
        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "only final injects".to_string(),
                latency_ms: 40,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.9),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should enqueue exactly once");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "post-final overlay".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim text should stay in overlay route");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should produce one report")
            .expect("report channel should remain open");
        assert!(report.error.is_none());

        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["only final injects".to_string()]
        );
        assert!(timeout(Duration::from_millis(150), reports.recv())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overlay_disconnect_does_not_block_final_result_injection() {
        let (overlay_tx, overlay_rx) = mpsc::unbounded_channel();
        drop(overlay_rx);
        let first_sink = OverlayProcessSink::from_sender_for_tests(
            overlay_tx,
            Arc::new(OverlayProcessMetrics::default()),
        );
        let sink_slot = Arc::new(Mutex::new(Some(first_sink)));
        let launcher = {
            let sink_slot = Arc::clone(&sink_slot);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                sink_slot
                    .lock()
                    .expect("sink slot lock should be available")
                    .take()
                    .ok_or_else(|| anyhow!("no overlay sink available"))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let (mut runtime, session_id) = runtime_waiting_for_result();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 1,
                text: "overlay event while disconnected".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("overlay disconnect should be non-fatal");
        assert_eq!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed),
            1
        );

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "final survives overlay disconnect".to_string(),
                latency_ms: 33,
                audio_ms: 777,
                lang: Some("en".to_string()),
                confidence: Some(0.95),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should still enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should report")
            .expect("report stream should remain open");
        assert!(report.error.is_none());
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["final survives overlay disconnect".to_string()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_overlay_failures_remain_non_fatal_to_final_injection() {
        fn disconnected_test_sink() -> OverlayProcessSink {
            let (overlay_tx, overlay_rx) = mpsc::unbounded_channel();
            drop(overlay_rx);
            OverlayProcessSink::from_sender_for_tests(
                overlay_tx,
                Arc::new(OverlayProcessMetrics::default()),
            )
        }

        let spawn_queue = Arc::new(Mutex::new(VecDeque::from([
            Ok(disconnected_test_sink()),
            Err(anyhow!(
                "failed to spawn overlay process '/tmp/parakeet-overlay': No such file or directory"
            )),
            Ok(disconnected_test_sink()),
            Err(anyhow!(
                "failed to spawn overlay process '/tmp/parakeet-overlay': No such file or directory"
            )),
            Ok(disconnected_test_sink()),
        ])));
        let launcher = {
            let spawn_queue = Arc::clone(&spawn_queue);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                spawn_queue
                    .lock()
                    .expect("spawn queue lock should be available")
                    .pop_front()
                    .unwrap_or_else(|| Err(anyhow!("no overlay sink available")))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let (mut runtime, session_id) = runtime_waiting_for_result();
        for seq in 1..=4 {
            handle_server_message_for_tests(
                ServerMessage::InterimText {
                    session_id,
                    seq,
                    text: format!("overlay seq {seq}"),
                },
                &mut runtime,
                &mut overlay_router,
                &worker,
            )
            .await
            .expect("overlay failures should remain non-fatal");
        }

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "final survives repeated overlay failures".to_string(),
                latency_ms: 12,
                audio_ms: 345,
                lang: Some("en".to_string()),
                confidence: Some(0.99),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should still enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should report")
            .expect("report stream should remain open");
        assert!(report.error.is_none());
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["final survives repeated overlay failures".to_string()]
        );
        assert!(
            manager_metrics.spawn_failure_total.load(Ordering::Relaxed) >= 1,
            "at least one spawn failure should be recorded"
        );
        assert!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed)
                >= 1,
            "at least one disconnect should be recorded"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overlay_crash_restart_replays_current_state_and_preserves_final_injection() {
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

        let spawn_queue = Arc::new(Mutex::new(VecDeque::from([
            Ok(first_sink),
            Ok(second_sink),
        ])));
        let launcher = {
            let spawn_queue = Arc::clone(&spawn_queue);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                spawn_queue
                    .lock()
                    .expect("spawn queue lock should be available")
                    .pop_front()
                    .unwrap_or_else(|| Err(anyhow!("no overlay sink available")))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let (mut runtime, session_id) = runtime_waiting_for_result();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 1,
                text: "old-state".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("first interim text should route");
        let first_seen = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive old state")
            .expect("first sink channel should stay open");
        assert_eq!(
            first_seen,
            parakeet_ptt::overlay_ipc::OverlayIpcMessage::InterimText {
                session_id,
                producer: parakeet_ptt::overlay_ipc::OverlayTextProducer::DaemonSttInterim,
                seq: 1,
                text: "old-state".to_string(),
            }
        );

        drop(rx_first);

        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "current-state".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim text after crash should remain non-fatal");

        let second_seen = timeout(Duration::from_millis(100), rx_second.recv())
            .await
            .expect("second sink should receive replayed current state")
            .expect("second sink channel should stay open");
        assert_eq!(
            second_seen,
            parakeet_ptt::overlay_ipc::OverlayIpcMessage::InterimText {
                session_id,
                producer: parakeet_ptt::overlay_ipc::OverlayTextProducer::DaemonSttInterim,
                seq: 2,
                text: "current-state".to_string(),
            }
        );
        assert!(timeout(Duration::from_millis(50), rx_second.recv())
            .await
            .is_err());

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "final after overlay restart".to_string(),
                latency_ms: 45,
                audio_ms: 1000,
                lang: Some("en".to_string()),
                confidence: Some(0.98),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should still enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should produce one report")
            .expect("report channel should remain open");
        assert!(report.error.is_none());
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["final after overlay restart".to_string()]
        );
        assert_eq!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(manager_metrics.replay_sent_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            manager_metrics.spawn_success_total.load(Ordering::Relaxed),
            2
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn daemon_interim_text_requires_active_session_and_fresh_sequence() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
        let injector = Arc::new(RecordingRunner {
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 2);

        let (mut runtime, session_id) = runtime_waiting_for_result();
        let stale_session_id = Uuid::new_v4();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id: stale_session_id,
                seq: 1,
                text: "stale session".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("mismatched interim text should be dropped without failure");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 10,
                text: "newest".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("first interim text should route");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 9,
                text: "stale".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("stale interim text should be dropped without failure");
        runtime.reset();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 11,
                text: "late daemon after reset".to_string(),
            },
            &mut runtime,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("late daemon interim text without active session should be dropped");

        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 0);
        let overlay_events = seen_overlay_events
            .lock()
            .expect("overlay recording lock should be available")
            .clone();
        assert_eq!(overlay_events.len(), 1);
        assert_eq!(
            overlay_events[0],
            OverlayEvent::InterimText {
                producer: OverlayTextProducer::DaemonSttInterim,
                session_id,
                seq: 10,
                text: "newest".to_string(),
            }
        );
    }
}
