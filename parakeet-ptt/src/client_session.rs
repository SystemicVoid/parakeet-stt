//! Client Session dispatch policy for Daemon messages.
//!
//! This Module owns per-Session Client runtime state and how Daemon
//! `ServerMessage` values affect Client PTT state, Overlay routing, Injection
//! enqueueing, LLM progress, and focus-routing handoff.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use anyhow::Result;
use tokio::time::Instant as TokioInstant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::audio_feedback::AudioFeedback;
use crate::injector::{injector_metrics_snapshot, ParentFocusCapture};
use crate::injector_runtime::{
    EnqueueFailure, InjectionErrorKind, InjectionJob, InjectionOrigin, InjectionReport,
    InjectorWorkerHandle, INJECTION_ENQUEUE_TIMEOUT_MS, INJECTION_QUEUE_CAPACITY,
};
use crate::overlay_router::{OverlayRouter, OverlaySink};
use crate::protocol::ServerMessage;
use crate::state::PttState;
use crate::surface_focus::{WaylandFocusCache, WaylandFocusObservation};

const PARENT_FOCUS_STALE_MS: u64 = 30_000;
const PARENT_FOCUS_TRANSITION_GRACE_MS: u64 = 500;

#[derive(Debug, Clone)]
struct CapturedParentFocus {
    focus: ParentFocusCapture,
    captured_at: TokioInstant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionIntent {
    Dictate,
    LlmQuery,
}

#[derive(Debug)]
pub(crate) struct ClientFocusRouter {
    focus_cache: Option<WaylandFocusCache>,
    parent_focus_by_session: HashMap<Uuid, CapturedParentFocus>,
    last_overlay_output_name: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ClientInjectionDispatcher {
    worker: InjectorWorkerHandle,
}

#[derive(Debug)]
pub(crate) struct ClientSessionRuntime {
    state: PttState,
    active_intent: Option<SessionIntent>,
    llm: LlmSessionRuntime,
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
}

#[derive(Debug, Clone)]
pub(crate) struct LlmAnswerInjection {
    session_id: Uuid,
    text: String,
    daemon_latency_ms: u64,
    daemon_audio_ms: u64,
    completion: LlmCompletionContext,
}

#[derive(Debug, Clone)]
struct FinalResultInjection {
    session_id: Uuid,
    text: String,
    latency_ms: u64,
    audio_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InjectionDispatchOutcome {
    Queued,
    QueueTimeout,
    WorkerGone,
    SkippedStaleSession,
}

impl ClientFocusRouter {
    pub(crate) fn new(focus_cache: Option<WaylandFocusCache>) -> Self {
        Self {
            focus_cache,
            parent_focus_by_session: HashMap::new(),
            last_overlay_output_name: None,
        }
    }

    pub(crate) fn record_stop_target(&mut self, session_id: Uuid, captured_at: TokioInstant) {
        if let Some(focus) = self.capture_parent_focus() {
            self.parent_focus_by_session
                .insert(session_id, CapturedParentFocus { focus, captured_at });
        }
    }

    #[cfg(test)]
    pub(crate) fn take_parent_focus_for_enqueue(
        &mut self,
        session_id: Uuid,
    ) -> Option<ParentFocusCapture> {
        self.take_captured_parent_focus_for_enqueue(session_id)
            .map(|captured| parent_focus_for_enqueue(&captured))
    }

    pub(crate) fn clear_session(&mut self, session_id: Uuid) {
        self.parent_focus_by_session.remove(&session_id);
    }

    pub(crate) fn reset_for_connection_drop(&mut self) {
        self.parent_focus_by_session.clear();
        self.reset_overlay_target();
    }

    pub(crate) fn reset_overlay_target(&mut self) {
        self.last_overlay_output_name = None;
    }

    pub(crate) fn next_overlay_output_hint(&mut self) -> Option<String> {
        let output_name = self.focus_cache.as_ref()?.current_output_name();
        self.next_overlay_output_hint_from(output_name)
    }

    fn next_overlay_output_hint_from(&mut self, output_name: Option<String>) -> Option<String> {
        let output_name = output_name?;
        if self.last_overlay_output_name.as_deref() == Some(output_name.as_str()) {
            return None;
        }

        self.last_overlay_output_name = Some(output_name.clone());
        Some(output_name)
    }

    fn capture_parent_focus(&self) -> Option<ParentFocusCapture> {
        let cache = self.focus_cache.as_ref()?;
        Some(parent_focus_from_observation(cache.observe(
            PARENT_FOCUS_STALE_MS,
            PARENT_FOCUS_TRANSITION_GRACE_MS,
        )))
    }

    fn take_captured_parent_focus_for_enqueue(
        &mut self,
        session_id: Uuid,
    ) -> Option<CapturedParentFocus> {
        self.parent_focus_by_session.remove(&session_id)
    }

    fn restore_captured_parent_focus_for_enqueue(
        &mut self,
        session_id: Uuid,
        captured: CapturedParentFocus,
    ) {
        self.parent_focus_by_session.insert(session_id, captured);
    }

    #[cfg(test)]
    fn record_parent_focus_for_tests(
        &mut self,
        session_id: Uuid,
        focus: ParentFocusCapture,
        captured_at: TokioInstant,
    ) {
        self.parent_focus_by_session
            .insert(session_id, CapturedParentFocus { focus, captured_at });
    }

    #[cfg(test)]
    fn next_overlay_output_hint_for_tests(&mut self, output_name: Option<&str>) -> Option<String> {
        self.next_overlay_output_hint_from(output_name.map(str::to_string))
    }
}

impl Default for ClientFocusRouter {
    fn default() -> Self {
        Self::new(None)
    }
}

impl LlmAnswerInjection {
    pub(crate) fn new(
        session_id: Uuid,
        text: String,
        daemon_latency_ms: u64,
        daemon_audio_ms: u64,
        completion: LlmCompletionContext,
    ) -> Self {
        Self {
            session_id,
            text,
            daemon_latency_ms,
            daemon_audio_ms,
            completion,
        }
    }
}

impl ClientInjectionDispatcher {
    pub(crate) fn new(worker: InjectorWorkerHandle) -> Self {
        Self { worker }
    }

    async fn dispatch_raw_final_result(
        &self,
        injection: FinalResultInjection,
        runtime: &mut ClientSessionRuntime,
        focus_router: &mut ClientFocusRouter,
    ) -> InjectionDispatchOutcome {
        if !runtime.final_result_belongs_to_active_session(injection.session_id) {
            runtime
                .log_rejected_final_result(injection.session_id, InjectionOrigin::RawFinalResult);
            return InjectionDispatchOutcome::SkippedStaleSession;
        }

        let hotkey_up_elapsed_ms_at_enqueue = elapsed_ms_since(runtime.last_hotkey_up_at);
        let stop_message_elapsed_ms_at_enqueue =
            runtime.stop_message_elapsed_ms_at_enqueue(injection.session_id);
        info!(
            session = %injection.session_id,
            origin = InjectionOrigin::RawFinalResult.as_str(),
            daemon_latency_ms = injection.latency_ms,
            audio_ms = injection.audio_ms,
            state_at_enqueue = runtime.state_label(),
            hotkey_up_elapsed_ms_at_enqueue,
            stop_message_elapsed_ms_at_enqueue,
            "final result received"
        );

        let outcome = self
            .enqueue(
                QueueInjectionRequest {
                    session_id: injection.session_id,
                    text: injection.text,
                    daemon_latency_ms: injection.latency_ms,
                    daemon_audio_ms: injection.audio_ms,
                    origin: InjectionOrigin::RawFinalResult,
                    hotkey_up_elapsed_ms_at_enqueue,
                    stop_message_elapsed_ms_at_enqueue,
                },
                focus_router,
            )
            .await;
        runtime.reset();
        outcome
    }

    pub(crate) async fn dispatch_llm_answer(
        &self,
        injection: LlmAnswerInjection,
        focus_router: &mut ClientFocusRouter,
    ) -> InjectionDispatchOutcome {
        info!(
            session = %injection.session_id,
            origin = InjectionOrigin::LlmAnswer.as_str(),
            state_at_enqueue = injection.completion.state_label,
            session_end_was_deferred = injection.completion.session_end_was_deferred,
            hotkey_up_elapsed_ms_at_enqueue =
                injection.completion.hotkey_up_elapsed_ms_at_enqueue,
            stop_message_elapsed_ms_at_enqueue =
                injection.completion.stop_message_elapsed_ms_at_enqueue,
            response_chars = injection.text.chars().count(),
            "queueing llm answer injection job"
        );

        self.enqueue(
            QueueInjectionRequest {
                session_id: injection.session_id,
                text: injection.text,
                daemon_latency_ms: injection.daemon_latency_ms,
                daemon_audio_ms: injection.daemon_audio_ms,
                origin: InjectionOrigin::LlmAnswer,
                hotkey_up_elapsed_ms_at_enqueue: injection
                    .completion
                    .hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue: injection
                    .completion
                    .stop_message_elapsed_ms_at_enqueue,
            },
            focus_router,
        )
        .await
    }

    async fn enqueue(
        &self,
        request: QueueInjectionRequest,
        focus_router: &mut ClientFocusRouter,
    ) -> InjectionDispatchOutcome {
        let session_id = request.session_id;
        let origin = request.origin;
        let captured_parent_focus =
            focus_router.take_captured_parent_focus_for_enqueue(request.session_id);
        let parent_focus = captured_parent_focus.as_ref().map(parent_focus_for_enqueue);
        let result = self
            .worker
            .enqueue(
                InjectionJob::new(
                    request.session_id,
                    request.text,
                    request.daemon_latency_ms,
                    request.daemon_audio_ms,
                )
                .with_origin(request.origin)
                .with_enqueue_timing(
                    request.hotkey_up_elapsed_ms_at_enqueue,
                    request.stop_message_elapsed_ms_at_enqueue,
                )
                .with_parent_focus(parent_focus),
            )
            .await;

        match result {
            Ok(()) => {
                log_dispatch_queued(session_id, origin);
                InjectionDispatchOutcome::Queued
            }
            Err(EnqueueFailure::Timeout) => {
                if let Some(captured_parent_focus) = captured_parent_focus {
                    focus_router.restore_captured_parent_focus_for_enqueue(
                        session_id,
                        captured_parent_focus,
                    );
                }
                log_dispatch_enqueue_timeout(session_id, origin);
                InjectionDispatchOutcome::QueueTimeout
            }
            Err(EnqueueFailure::WorkerGone) => {
                if let Some(captured_parent_focus) = captured_parent_focus {
                    focus_router.restore_captured_parent_focus_for_enqueue(
                        session_id,
                        captured_parent_focus,
                    );
                }
                log_dispatch_worker_gone(session_id, origin);
                InjectionDispatchOutcome::WorkerGone
            }
        }
    }

    pub(crate) fn handle_report<S: OverlaySink>(
        &self,
        report: InjectionReport,
        overlay_router: &mut OverlayRouter<S>,
        audio_feedback: &AudioFeedback,
    ) {
        self.worker.metrics().note_report(&report);
        let success = report.error_kind.is_none() && report.error.is_none();
        match (report.error_kind, report.error) {
            (Some(error_kind), Some(error)) => {
                warn!(
                    session = %report.session_id,
                    origin = report.origin.as_str(),
                    error_kind = error_kind.as_str(),
                    daemon_latency_ms = report.daemon_latency_ms,
                    daemon_audio_ms = report.daemon_audio_ms,
                    queue_wait_ms = report.queue_wait_ms,
                    run_ms = report.run_ms,
                    total_worker_ms = report.total_worker_ms,
                    enqueue_to_injection_complete_ms = report.enqueue_to_injection_complete_ms,
                    hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                    stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                    hotkey_up_elapsed_ms_at_worker_start =
                        report.hotkey_up_elapsed_ms_at_worker_start,
                    stop_message_elapsed_ms_at_worker_start =
                        report.stop_message_elapsed_ms_at_worker_start,
                    hotkey_up_elapsed_ms_at_completion =
                        report.hotkey_up_elapsed_ms_at_completion,
                    stop_message_elapsed_ms_at_completion =
                        report.stop_message_elapsed_ms_at_completion,
                    error = %error,
                    "injector worker reported failure"
                );
            }
            (None, None) => {
                info!(
                    session = %report.session_id,
                    origin = report.origin.as_str(),
                    daemon_latency_ms = report.daemon_latency_ms,
                    daemon_audio_ms = report.daemon_audio_ms,
                    queue_wait_ms = report.queue_wait_ms,
                    run_ms = report.run_ms,
                    total_worker_ms = report.total_worker_ms,
                    enqueue_to_injection_complete_ms = report.enqueue_to_injection_complete_ms,
                    hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                    stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                    hotkey_up_elapsed_ms_at_worker_start =
                        report.hotkey_up_elapsed_ms_at_worker_start,
                    stop_message_elapsed_ms_at_worker_start =
                        report.stop_message_elapsed_ms_at_worker_start,
                    hotkey_up_elapsed_ms_at_completion =
                        report.hotkey_up_elapsed_ms_at_completion,
                    stop_message_elapsed_ms_at_completion =
                        report.stop_message_elapsed_ms_at_completion,
                    "injector worker completed job"
                );
                audio_feedback.play_completion();
            }
            (error_kind, error) => {
                warn!(
                    session = %report.session_id,
                    origin = report.origin.as_str(),
                    error_kind = error_kind.map(InjectionErrorKind::as_str),
                    daemon_latency_ms = report.daemon_latency_ms,
                    daemon_audio_ms = report.daemon_audio_ms,
                    queue_wait_ms = report.queue_wait_ms,
                    run_ms = report.run_ms,
                    total_worker_ms = report.total_worker_ms,
                    enqueue_to_injection_complete_ms = report.enqueue_to_injection_complete_ms,
                    hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                    stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                    hotkey_up_elapsed_ms_at_worker_start =
                        report.hotkey_up_elapsed_ms_at_worker_start,
                    stop_message_elapsed_ms_at_worker_start =
                        report.stop_message_elapsed_ms_at_worker_start,
                    hotkey_up_elapsed_ms_at_completion =
                        report.hotkey_up_elapsed_ms_at_completion,
                    stop_message_elapsed_ms_at_completion =
                        report.stop_message_elapsed_ms_at_completion,
                    error = ?error,
                    "injector worker reported inconsistent error classification"
                );
            }
        }

        let processed = self
            .worker
            .metrics()
            .worker_success_total
            .load(Ordering::Relaxed)
            + self
                .worker
                .metrics()
                .worker_failure_total
                .load(Ordering::Relaxed);
        if processed.is_multiple_of(25) && processed > 0 {
            self.worker.metrics().log_summary();
            let snapshot = injector_metrics_snapshot();
            info!(
                clipboard_ready_success_total = snapshot.clipboard_ready_success_total,
                clipboard_ready_failure_total = snapshot.clipboard_ready_failure_total,
                clipboard_ready_duration_ms_total = snapshot.clipboard_ready_duration_ms_total,
                route_shortcut_success_total = snapshot.route_shortcut_success_total,
                route_shortcut_failure_total = snapshot.route_shortcut_failure_total,
                route_shortcut_duration_ms_total = snapshot.route_shortcut_duration_ms_total,
                backend_success_total = snapshot.backend_success_total,
                backend_failure_total = snapshot.backend_failure_total,
                backend_duration_ms_total = snapshot.backend_duration_ms_total,
                wl_copy_spawn_total = snapshot.wl_copy_spawn_total,
                wl_paste_spawn_total = snapshot.wl_paste_spawn_total,
                "injector stage metrics summary"
            );
        }

        overlay_router.route_injection_complete(report.session_id, success);
    }
}

#[derive(Debug)]
struct QueueInjectionRequest {
    session_id: Uuid,
    text: String,
    daemon_latency_ms: u64,
    daemon_audio_ms: u64,
    origin: InjectionOrigin,
    hotkey_up_elapsed_ms_at_enqueue: Option<u64>,
    stop_message_elapsed_ms_at_enqueue: Option<u64>,
}

fn log_dispatch_queued(session_id: Uuid, origin: InjectionOrigin) {
    match origin {
        InjectionOrigin::RawFinalResult => {
            debug!(session = %session_id, "final result queued for injector worker");
        }
        InjectionOrigin::LlmAnswer => {
            debug!(session = %session_id, "llm final answer queued for injector worker");
        }
        InjectionOrigin::Demo => {
            debug!(session = %session_id, "demo injection queued for injector worker");
        }
        InjectionOrigin::Unspecified => {
            debug!(session = %session_id, "injection queued for injector worker");
        }
    }
}

fn log_dispatch_enqueue_timeout(session_id: Uuid, origin: InjectionOrigin) {
    match origin {
        InjectionOrigin::RawFinalResult => {
            warn!(
                session = %session_id,
                queue_capacity = INJECTION_QUEUE_CAPACITY,
                enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                "injector queue remained full; dropping final result injection job"
            );
        }
        InjectionOrigin::LlmAnswer => {
            warn!(
                session = %session_id,
                queue_capacity = INJECTION_QUEUE_CAPACITY,
                enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                "injector queue remained full; dropping llm final answer"
            );
        }
        InjectionOrigin::Demo => {
            warn!(
                session = %session_id,
                queue_capacity = INJECTION_QUEUE_CAPACITY,
                enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                "injector queue remained full; dropping demo injection job"
            );
        }
        InjectionOrigin::Unspecified => {
            warn!(
                session = %session_id,
                queue_capacity = INJECTION_QUEUE_CAPACITY,
                enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                "injector queue remained full; dropping injection job"
            );
        }
    }
}

fn log_dispatch_worker_gone(session_id: Uuid, origin: InjectionOrigin) {
    match origin {
        InjectionOrigin::RawFinalResult => {
            warn!(
                session = %session_id,
                "injector worker unavailable; dropping final result injection job"
            );
        }
        InjectionOrigin::LlmAnswer => {
            warn!(session = %session_id, "injector worker unavailable; dropping llm final answer");
        }
        InjectionOrigin::Demo => {
            warn!(session = %session_id, "injector worker unavailable; dropping demo injection job");
        }
        InjectionOrigin::Unspecified => {
            warn!(session = %session_id, "injector worker unavailable; dropping injection job");
        }
    }
}

fn parent_focus_from_observation(observation: WaylandFocusObservation) -> ParentFocusCapture {
    match observation {
        WaylandFocusObservation::Fresh {
            snapshot,
            cache_age_ms,
        } => ParentFocusCapture {
            snapshot: Some(snapshot),
            source_selected: "wayland_cache".to_string(),
            wayland_cache_age_ms: Some(cache_age_ms),
            wayland_fallback_reason: None,
            captured_elapsed_ms: Some(0),
        },
        WaylandFocusObservation::LowConfidence {
            snapshot,
            cache_age_ms,
            reason,
        } => ParentFocusCapture {
            snapshot: Some(snapshot),
            source_selected: "wayland_cache_low_confidence".to_string(),
            wayland_cache_age_ms: Some(cache_age_ms),
            wayland_fallback_reason: Some(reason.to_string()),
            captured_elapsed_ms: Some(0),
        },
        WaylandFocusObservation::Unavailable {
            reason,
            cache_age_ms,
        } => ParentFocusCapture {
            snapshot: None,
            source_selected: "wayland_unavailable".to_string(),
            wayland_cache_age_ms: cache_age_ms,
            wayland_fallback_reason: Some(reason.to_string()),
            captured_elapsed_ms: Some(0),
        },
    }
}

fn parent_focus_for_enqueue(captured: &CapturedParentFocus) -> ParentFocusCapture {
    let mut focus = captured.focus.clone();
    focus.captured_elapsed_ms = Some(captured.captured_at.elapsed().as_millis() as u64);
    focus
}

impl ClientSessionRuntime {
    pub(crate) fn new() -> Self {
        Self {
            state: PttState::new(),
            active_intent: None,
            llm: LlmSessionRuntime::default(),
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

    pub(crate) fn record_stop_message_sent(&mut self, session_id: Uuid, sent_at: TokioInstant) {
        self.last_hotkey_up_at = Some(sent_at);
        self.last_stop_message = Some((session_id, sent_at));
    }

    pub(crate) fn reset(&mut self) {
        self.state.reset();
        self.active_intent = None;
    }

    pub(crate) fn reset_for_connection_drop(&mut self) {
        self.reset();
        self.llm.clear();
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
    focus_router: &mut ClientFocusRouter,
    overlay_router: &mut OverlayRouter<S>,
    injection_dispatcher: &ClientInjectionDispatcher,
) -> Result<()> {
    match message {
        ServerMessage::SessionStarted { session_id, .. } => {
            info!(session = %session_id, "session started ack");
            focus_router.reset_overlay_target();
            overlay_router.note_session_started(session_id);
        }
        ServerMessage::FinalResult {
            session_id,
            text,
            latency_ms,
            audio_ms,
            ..
        } => {
            injection_dispatcher
                .dispatch_raw_final_result(
                    FinalResultInjection {
                        session_id,
                        text,
                        latency_ms,
                        audio_ms,
                    },
                    runtime,
                    focus_router,
                )
                .await;
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
                focus_router.clear_session(session_id);
            }
            runtime.reset();
        }
        ServerMessage::InterimState {
            session_id,
            seq,
            state: interim_state,
        } => {
            overlay_router.route_daemon_interim_state_with_output_hint(
                runtime.active_session_id(),
                session_id,
                seq,
                interim_state,
                || focus_router.next_overlay_output_hint(),
            );
        }
        ServerMessage::InterimText {
            session_id,
            seq,
            text,
        } => {
            overlay_router.route_daemon_interim_text_with_output_hint(
                runtime.active_session_id(),
                session_id,
                seq,
                text,
                || focus_router.next_overlay_output_hint(),
            );
        }
        ServerMessage::AudioLevel {
            session_id,
            level_db,
        } => {
            overlay_router.route_audio_level(runtime.active_session_id(), session_id, level_db);
        }
        ServerMessage::SessionEnded { session_id, reason } => {
            let ends_active_session = runtime.active_session_id() == Some(session_id);
            focus_router.clear_session(session_id);
            overlay_router.route_session_ended(runtime.active_session_id(), session_id, reason);
            if ends_active_session {
                focus_router.reset_overlay_target();
            }
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

    use crate::audio_feedback::AudioFeedback;
    use crate::config::OverlayMode;
    use crate::injector_runtime::{
        spawn_injector_worker_with_capacity, InjectionErrorKind, InjectionJob, InjectionJobRunner,
        InjectionOrigin, InjectionReport, InjectionRunError, InjectionRunOutput,
        InjectorWorkerHandle,
    };
    use crate::overlay_process::{
        OverlayProcessManager, OverlayProcessMetrics, OverlayProcessSink,
    };
    use crate::overlay_router::{
        NoopOverlaySink, OverlayEvent, OverlayRouter, OverlaySink, OverlayTextProducer,
        RuntimeOverlaySink,
    };
    use crate::protocol::ServerMessage;
    use crate::surface_focus::{FocusSnapshot, WaylandFocusObservation};
    use anyhow::anyhow;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Instant as TokioInstant};
    use uuid::Uuid;

    use super::{
        handle_server_message, parent_focus_from_observation, ClientFocusRouter,
        ClientInjectionDispatcher, ClientSessionRuntime, InjectionDispatchOutcome,
        LlmAnswerInjection, LlmCompletionContext, LlmDeltaOverlay, SessionIntent,
    };

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

    struct RecordingJobRunner {
        seen: Arc<Mutex<Vec<InjectionJob>>>,
    }

    impl InjectionJobRunner for RecordingJobRunner {
        fn run(
            &self,
            job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            self.seen
                .lock()
                .expect("recording job lock should be available")
                .push(job.clone());
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
        let mut focus_router = ClientFocusRouter::default();
        let injection_dispatcher = ClientInjectionDispatcher::new(injector_worker.clone());
        handle_server_message(
            message,
            runtime,
            &mut focus_router,
            overlay_router,
            &injection_dispatcher,
        )
        .await
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
        runtime.record_stop_message_sent(session_id, stopped_at);
        (runtime, session_id)
    }

    fn test_focus_snapshot(output_name: Option<&str>) -> FocusSnapshot {
        FocusSnapshot {
            app_name: Some("Code".to_string()),
            object_name: Some("editor".to_string()),
            object_path: Some("/com/system76/Cosmic/Window/1".to_string()),
            service_name: Some(":1.42".to_string()),
            output_name: output_name.map(str::to_string),
            focused: true,
            active: true,
            resolver: "test".to_string(),
        }
    }

    #[test]
    fn client_focus_router_maps_parent_focus_for_injection() {
        let session_id = Uuid::new_v4();
        let parent_focus = parent_focus_from_observation(WaylandFocusObservation::LowConfidence {
            snapshot: test_focus_snapshot(Some("DP-1")),
            cache_age_ms: 17,
            reason: "within_transition_grace",
        });
        let mut focus_router = ClientFocusRouter::default();
        focus_router.record_parent_focus_for_tests(
            session_id,
            parent_focus,
            TokioInstant::now() - Duration::from_millis(25),
        );

        let routed = focus_router
            .take_parent_focus_for_enqueue(session_id)
            .expect("captured parent focus should be available for Injection");

        assert_eq!(routed.source_selected, "wayland_cache_low_confidence");
        assert_eq!(routed.wayland_cache_age_ms, Some(17));
        assert_eq!(
            routed.wayland_fallback_reason.as_deref(),
            Some("within_transition_grace")
        );
        assert_eq!(
            routed
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.output_name.as_deref()),
            Some("DP-1")
        );
        assert!(
            routed.captured_elapsed_ms.unwrap_or_default() >= 25,
            "captured elapsed should reflect time between stop capture and enqueue"
        );
        assert!(focus_router
            .take_parent_focus_for_enqueue(session_id)
            .is_none());
    }

    #[test]
    fn client_focus_router_deduplicates_overlay_output_hints_until_reset() {
        let mut focus_router = ClientFocusRouter::default();

        assert_eq!(
            focus_router.next_overlay_output_hint_for_tests(Some("DP-1")),
            Some("DP-1".to_string())
        );
        assert_eq!(
            focus_router.next_overlay_output_hint_for_tests(Some("DP-1")),
            None
        );
        assert_eq!(
            focus_router.next_overlay_output_hint_for_tests(Some("HDMI-A-1")),
            Some("HDMI-A-1".to_string())
        );

        focus_router.reset_overlay_target();

        assert_eq!(
            focus_router.next_overlay_output_hint_for_tests(Some("HDMI-A-1")),
            Some("HDMI-A-1".to_string())
        );
    }

    #[test]
    fn client_focus_router_output_hint_feeds_overlay_router() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut focus_router = ClientFocusRouter::default();
        let mut overlay_router = OverlayRouter::new(RecordingOverlaySink {
            seen: Arc::clone(&seen_overlay_events),
        });

        if let Some(output_name) = focus_router.next_overlay_output_hint_for_tests(Some("DP-1")) {
            overlay_router.route_output_hint(output_name);
        }
        if let Some(output_name) = focus_router.next_overlay_output_hint_for_tests(Some("DP-1")) {
            overlay_router.route_output_hint(output_name);
        }

        assert_eq!(
            seen_overlay_events
                .lock()
                .expect("overlay recording lock should be available")
                .as_slice(),
            &[OverlayEvent::OutputHint {
                output_name: "DP-1".to_string(),
            }]
        );
    }

    #[test]
    fn client_session_runtime_resets_reconnect_caches() {
        let mut runtime = ClientSessionRuntime::new();
        let session_id = runtime
            .begin_listening(SessionIntent::LlmQuery)
            .expect("llm session should start");
        runtime.stop_listening().expect("llm session should stop");
        runtime.record_stop_message_sent(session_id, TokioInstant::now());
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
        runtime.record_stop_message_sent(session_id, TokioInstant::now());
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
        assert!(!runtime.is_llm_busy());
        assert!(runtime.finish_llm_answer(session_id).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_injection_dispatcher_queues_llm_answer_with_focus_context() {
        let seen_jobs = Arc::new(Mutex::new(Vec::<InjectionJob>::new()));
        let injector = Arc::new(RecordingJobRunner {
            seen: Arc::clone(&seen_jobs),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);
        let dispatcher = ClientInjectionDispatcher::new(worker);
        let session_id = Uuid::new_v4();
        let mut focus_router = ClientFocusRouter::default();
        let parent_focus = parent_focus_from_observation(WaylandFocusObservation::Fresh {
            snapshot: test_focus_snapshot(Some("DP-1")),
            cache_age_ms: 8,
        });
        focus_router.record_parent_focus_for_tests(
            session_id,
            parent_focus,
            TokioInstant::now() - Duration::from_millis(15),
        );

        let outcome = dispatcher
            .dispatch_llm_answer(
                LlmAnswerInjection::new(
                    session_id,
                    "model answer".to_string(),
                    77,
                    1400,
                    LlmCompletionContext {
                        session_end_reason: Some("normal".to_string()),
                        session_end_was_deferred: true,
                        state_label: "idle",
                        hotkey_up_elapsed_ms_at_enqueue: Some(21),
                        stop_message_elapsed_ms_at_enqueue: Some(22),
                    },
                ),
                &mut focus_router,
            )
            .await;

        assert_eq!(outcome, InjectionDispatchOutcome::Queued);
        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("injection report should arrive")
            .expect("report channel should stay open");
        assert!(report.error.is_none());
        assert_eq!(report.origin, InjectionOrigin::LlmAnswer);

        let jobs = seen_jobs
            .lock()
            .expect("recording job lock should be available");
        let job = jobs
            .first()
            .expect("dispatcher should submit exactly one job");
        assert_eq!(job.session_id, session_id);
        assert_eq!(job.text, "model answer");
        assert_eq!(job.daemon_latency_ms, 77);
        assert_eq!(job.daemon_audio_ms, 1400);
        assert_eq!(job.origin, InjectionOrigin::LlmAnswer);
        assert_eq!(job.hotkey_up_elapsed_ms_at_enqueue, Some(21));
        assert_eq!(job.stop_message_elapsed_ms_at_enqueue, Some(22));
        let parent_focus = job
            .parent_focus
            .as_ref()
            .expect("focus router should provide parent focus to Injection");
        assert_eq!(parent_focus.source_selected, "wayland_cache");
        assert_eq!(parent_focus.wayland_cache_age_ms, Some(8));
        assert!(
            parent_focus.captured_elapsed_ms.unwrap_or_default() >= 15,
            "parent focus elapsed should reflect capture-to-dispatch time"
        );
        drop(jobs);
        assert!(focus_router
            .take_parent_focus_for_enqueue(session_id)
            .is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_injection_dispatcher_preserves_focus_after_enqueue_timeout() {
        let slow = Arc::new(SlowRunner {
            calls: Arc::new(AtomicU64::new(0)),
            sleep_ms: 200,
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(slow, 1);
        worker
            .enqueue(InjectionJob::new(
                Uuid::new_v4(),
                "busy worker".to_string(),
                1,
                1,
            ))
            .await
            .expect("first enqueue should occupy the worker");
        worker
            .enqueue(InjectionJob::new(
                Uuid::new_v4(),
                "queued job".to_string(),
                1,
                1,
            ))
            .await
            .expect("second enqueue should fill the bounded queue");

        let dispatcher = ClientInjectionDispatcher::new(worker);
        let session_id = Uuid::new_v4();
        let mut focus_router = ClientFocusRouter::default();
        focus_router.record_parent_focus_for_tests(
            session_id,
            parent_focus_from_observation(WaylandFocusObservation::Fresh {
                snapshot: test_focus_snapshot(Some("DP-1")),
                cache_age_ms: 3,
            }),
            TokioInstant::now() - Duration::from_millis(10),
        );

        let outcome = dispatcher
            .dispatch_llm_answer(
                LlmAnswerInjection::new(
                    session_id,
                    "preserve focus".to_string(),
                    1,
                    1,
                    LlmCompletionContext {
                        session_end_reason: None,
                        session_end_was_deferred: false,
                        state_label: "idle",
                        hotkey_up_elapsed_ms_at_enqueue: None,
                        stop_message_elapsed_ms_at_enqueue: None,
                    },
                ),
                &mut focus_router,
            )
            .await;

        assert_eq!(outcome, InjectionDispatchOutcome::QueueTimeout);
        let restored = focus_router
            .take_parent_focus_for_enqueue(session_id)
            .expect("enqueue failure should restore parent focus for the session");
        assert_eq!(restored.source_selected, "wayland_cache");
        assert_eq!(restored.wayland_cache_age_ms, Some(3));
        assert!(
            restored.captured_elapsed_ms.unwrap_or_default() >= 10,
            "restored focus should keep its original capture time"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_injection_dispatcher_routes_worker_report_completion() {
        let injector = Arc::new(RecordingRunner {
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 4);
        let dispatcher = ClientInjectionDispatcher::new(worker.clone());
        let session_id = Uuid::new_v4();
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(RecordingOverlaySink {
            seen: Arc::clone(&seen_overlay_events),
        });

        dispatcher.handle_report(
            InjectionReport {
                session_id,
                daemon_latency_ms: 20,
                daemon_audio_ms: 1000,
                origin: InjectionOrigin::RawFinalResult,
                queue_wait_ms: 1,
                run_ms: 2,
                total_worker_ms: 3,
                enqueue_to_injection_complete_ms: 3,
                hotkey_up_elapsed_ms_at_enqueue: None,
                stop_message_elapsed_ms_at_enqueue: None,
                hotkey_up_elapsed_ms_at_worker_start: None,
                stop_message_elapsed_ms_at_worker_start: None,
                hotkey_up_elapsed_ms_at_completion: None,
                stop_message_elapsed_ms_at_completion: None,
                error_kind: Some(InjectionErrorKind::BackendFailure),
                error: Some("stage=backend synthetic failure".to_string()),
            },
            &mut overlay_router,
            &AudioFeedback::new(false, None, 0),
        );

        assert_eq!(
            InjectionErrorKind::BackendFailure.as_str(),
            "backend_failure"
        );
        assert_eq!(
            worker
                .metrics()
                .worker_backend_failure_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            seen_overlay_events
                .lock()
                .expect("overlay recording lock should be available")
                .as_slice(),
            &[OverlayEvent::InjectionComplete {
                session_id,
                success: false,
            }]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_result_enqueues_injection_job() {
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);
        let (mut runtime, session_id) = runtime_waiting_for_result();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink);

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
        let mut focus_router = ClientFocusRouter::default();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink);
        let injection_dispatcher = ClientInjectionDispatcher::new(worker.clone());

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
            &mut focus_router,
            &mut overlay_router,
            &injection_dispatcher,
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
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink);

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
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink);
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
        let mut overlay_router = OverlayRouter::new(RecordingOverlaySink {
            seen: Arc::clone(&seen_overlay_events),
        });
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
        let mut overlay_router = OverlayRouter::new(RecordingOverlaySink {
            seen: Arc::clone(&seen_overlay_events),
        });
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
        let mut overlay_router = OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)));

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
        let mut overlay_router = OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)));

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
        let mut overlay_router = OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)));

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
        let mut overlay_router = OverlayRouter::new(RecordingOverlaySink {
            seen: Arc::clone(&seen_overlay_events),
        });
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
