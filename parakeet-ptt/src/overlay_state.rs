//! Overlay presentation state: the session view the renderer draws.
//!
//! The Overlay is display-only (ADR 0004: it is not a runtime-truth consumer).
//! This state machine turns the IPC stream from the client into one
//! `SessionView` per Session: which phase the machine is in, the transcript
//! and its draft tail, the LLM question and answer, the timer, and the cap
//! warning. Stale sequence numbers and mismatched sessions are dropped here so
//! the renderer never has to reason about ordering.

use std::collections::HashMap;
use std::time::Duration;

use uuid::Uuid;

use crate::overlay_ipc::{OverlayIpcMessage, OverlayTextProducer};

pub const DEFAULT_AUTO_HIDE_AFTER_MS: u64 = 600;
/// The verdict lamp (PASTED / FAILED / NO TEXT) stays this long before the sheet leaves.
pub const DONE_LINGER_MS: u64 = 900;

/// Daemon interim-state values (`InterimStateValue` in the daemon's messages module).
const DAEMON_STATE_LISTENING: &str = "listening";
const DAEMON_STATE_FINALIZING: &str = "finalizing";
/// Session end reason that means the Seal path produced a transcript.
const REASON_FINAL: &str = "final";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMode {
    Stt,
    Llm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPhase {
    Listening,
    Interim,
    Finalizing,
    Answering,
    Done { success: bool },
}

/// The Daemon's approaching-limit warning, as received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapWarning {
    pub at_ms: u64,
    /// Time left at `at_ms`; `None` when an older client did not send it.
    pub remaining_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub session_id: Uuid,
    pub mode: OverlayMode,
    pub phase: OverlayPhase,
    /// The Daemon interim transcript (STT mode), or the question once LLM mode starts.
    pub transcript: String,
    /// The last interim transcript at the moment the LLM started answering.
    pub question: Option<String>,
    pub answer: String,
    /// An LLM status line ("Generating answer...") shown until the first answer delta.
    pub status: Option<String>,
    /// A transient notice layered over the view (the busy rejection).
    pub notice: Option<String>,
    pub reason: Option<String>,
    /// The client injects copy-only, so a successful injection copied rather than pasted.
    pub copy_only: bool,
    pub started_ms: u64,
    /// Set when the Session ended (or finalization started); the timer freezes here.
    pub ended_ms: Option<u64>,
    pub warning: Option<CapWarning>,
}

impl SessionView {
    fn new(session_id: Uuid, producer: OverlayTextProducer, now_ms: u64) -> Self {
        let mode = match producer {
            OverlayTextProducer::DaemonSttInterim => OverlayMode::Stt,
            OverlayTextProducer::LlmAnswerDelta => OverlayMode::Llm,
        };
        Self {
            session_id,
            mode,
            phase: OverlayPhase::Listening,
            transcript: String::new(),
            question: None,
            answer: String::new(),
            status: None,
            notice: None,
            reason: None,
            copy_only: false,
            started_ms: now_ms,
            ended_ms: None,
            warning: None,
        }
    }

    /// Capturing audio: the coil and the timer run.
    pub fn is_live(&self) -> bool {
        matches!(self.phase, OverlayPhase::Listening | OverlayPhase::Interim)
    }

    /// The Session ended without a transcript (abort or error).
    pub fn failed(&self) -> bool {
        self.reason
            .as_deref()
            .is_some_and(|reason| reason != REASON_FINAL)
    }

    pub fn has_text(&self) -> bool {
        !self.transcript.trim().is_empty()
            || self.question.is_some()
            || !self.answer.trim().is_empty()
    }

    /// Session time shown by the timer; frozen once the Session ended.
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        self.ended_ms
            .unwrap_or(now_ms)
            .max(self.started_ms)
            .saturating_sub(self.started_ms)
    }

    /// Time left before the cap, once the warning arrived with a remaining value.
    pub fn remaining_ms(&self, now_ms: u64) -> Option<u64> {
        let warning = self.warning?;
        let remaining = warning.remaining_ms?;
        let clock = self.ended_ms.unwrap_or(now_ms).min(now_ms);
        Some(remaining.saturating_sub(clock.saturating_sub(warning.at_ms)))
    }

    fn enter_llm(&mut self) {
        if self.mode == OverlayMode::Stt {
            self.mode = OverlayMode::Llm;
        }
        if self.question.is_none() && !self.transcript.trim().is_empty() {
            self.question = Some(self.transcript.trim().to_string());
        }
        self.phase = OverlayPhase::Answering;
    }
}

// One value lives in the state machine; boxing would only add a pointer chase per frame.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayVisibility {
    Hidden,
    Visible(SessionView),
}

impl OverlayVisibility {
    pub fn view(&self) -> Option<&SessionView> {
        match self {
            Self::Hidden => None,
            Self::Visible(view) => Some(view),
        }
    }

    pub fn is_visible(&self) -> bool {
        matches!(self, Self::Visible(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    DroppedStaleSeq,
    DroppedSessionMismatch,
}

#[derive(Debug, Clone)]
pub struct OverlayStateMachine {
    visibility: OverlayVisibility,
    active_session_id: Option<Uuid>,
    last_seq_by_producer: HashMap<OverlayTextProducer, u64>,
    hide_deadline_ms: Option<u64>,
    /// When the transient notice clears; the busy sequence ends it with a nil
    /// `SessionEnded`, which would otherwise erase the notice before it was seen.
    notice_deadline_ms: Option<u64>,
    auto_hide_after_ms: u64,
}

impl OverlayStateMachine {
    pub fn new(auto_hide_after: Duration) -> Self {
        Self {
            visibility: OverlayVisibility::Hidden,
            active_session_id: None,
            last_seq_by_producer: HashMap::new(),
            hide_deadline_ms: None,
            notice_deadline_ms: None,
            auto_hide_after_ms: auto_hide_after.as_millis() as u64,
        }
    }

    pub fn visibility(&self) -> &OverlayVisibility {
        &self.visibility
    }

    pub fn apply_event(&mut self, message: OverlayIpcMessage, now_ms: u64) -> ApplyOutcome {
        match message {
            OverlayIpcMessage::OutputHint { .. } | OverlayIpcMessage::AudioLevel { .. } => {
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::InterimState {
                session_id,
                producer,
                seq,
                state,
            } => {
                if session_id.is_nil() {
                    self.set_notice(Some(state));
                    self.notice_deadline_ms = None;
                    return ApplyOutcome::Applied;
                }
                if let Some(outcome) = self.apply_seq(session_id, producer, seq) {
                    return outcome;
                }
                let view = self.view_for(session_id, producer, now_ms);
                match producer {
                    OverlayTextProducer::DaemonSttInterim => match state.as_str() {
                        DAEMON_STATE_LISTENING
                            if view.is_live() && view.transcript.trim().is_empty() =>
                        {
                            view.phase = OverlayPhase::Listening;
                        }
                        DAEMON_STATE_FINALIZING if view.mode == OverlayMode::Stt => {
                            if view.is_live() {
                                view.phase = OverlayPhase::Finalizing;
                            }
                            view.ended_ms.get_or_insert(now_ms);
                        }
                        // "processing", "interim" and anything new leave the phase alone
                        // and never reach the prose column.
                        _ => {}
                    },
                    OverlayTextProducer::LlmAnswerDelta => {
                        view.enter_llm();
                        view.status = Some(state);
                    }
                }
                self.hide_deadline_ms = None;
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::InterimText {
                session_id,
                producer,
                seq,
                text,
            } => {
                if session_id.is_nil() {
                    self.set_notice(Some(text));
                    self.notice_deadline_ms = None;
                    return ApplyOutcome::Applied;
                }
                if let Some(outcome) = self.apply_seq(session_id, producer, seq) {
                    return outcome;
                }
                let view = self.view_for(session_id, producer, now_ms);
                match producer {
                    OverlayTextProducer::DaemonSttInterim => {
                        if view.mode == OverlayMode::Stt {
                            view.transcript = text;
                            if view.phase == OverlayPhase::Listening {
                                view.phase = OverlayPhase::Interim;
                            }
                        }
                    }
                    OverlayTextProducer::LlmAnswerDelta => {
                        view.enter_llm();
                        view.answer = text;
                        view.status = None;
                    }
                }
                self.hide_deadline_ms = None;
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::SessionEnded { session_id, reason } => {
                if session_id.is_nil() {
                    self.notice_deadline_ms = Some(now_ms.saturating_add(self.auto_hide_after_ms));
                    return ApplyOutcome::Applied;
                }
                if let Some(active_session_id) = self.active_session_id {
                    if active_session_id != session_id {
                        return ApplyOutcome::DroppedSessionMismatch;
                    }
                }
                self.active_session_id = Some(session_id);
                self.last_seq_by_producer.clear();
                let view = self.view_for(session_id, OverlayTextProducer::DaemonSttInterim, now_ms);
                view.reason = reason;
                view.ended_ms.get_or_insert(now_ms);
                if view.is_live() {
                    view.phase = OverlayPhase::Finalizing;
                }
                self.hide_deadline_ms = Some(now_ms.saturating_add(self.auto_hide_after_ms));
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::SessionWarning {
                session_id,
                remaining_seconds,
                limit_seconds,
            } => {
                if self.active_session_id != Some(session_id) {
                    return ApplyOutcome::DroppedSessionMismatch;
                }
                if let OverlayVisibility::Visible(view) = &mut self.visibility {
                    if view.session_id == session_id {
                        let to_ms = |seconds: f32| (seconds.max(0.0) * 1000.0).round() as u64;
                        let elapsed_ms = view.elapsed_ms(now_ms);
                        // Prefer the Daemon's remaining time; fall back to the cap
                        // minus the time this view has seen.
                        let remaining_ms = remaining_seconds
                            .filter(|seconds| seconds.is_finite())
                            .map(to_ms)
                            .or_else(|| {
                                limit_seconds
                                    .filter(|seconds| seconds.is_finite())
                                    .map(|limit| to_ms(limit).saturating_sub(elapsed_ms))
                            });
                        view.warning = Some(CapWarning {
                            at_ms: now_ms,
                            remaining_ms,
                        });
                    }
                }
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::InjectionComplete {
                session_id,
                success,
                copy_only,
            } => match &mut self.visibility {
                OverlayVisibility::Visible(view) if view.session_id == session_id => {
                    view.phase = OverlayPhase::Done { success };
                    view.copy_only = copy_only;
                    view.ended_ms.get_or_insert(now_ms);
                    self.hide_deadline_ms = Some(now_ms.saturating_add(DONE_LINGER_MS));
                    ApplyOutcome::Applied
                }
                _ => ApplyOutcome::DroppedSessionMismatch,
            },
        }
    }

    /// Returns true when something to repaint changed: the transient notice
    /// expired, or the hide deadline passed and the Overlay went hidden.
    pub fn advance_time(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        if self
            .notice_deadline_ms
            .is_some_and(|deadline| now_ms >= deadline)
        {
            self.notice_deadline_ms = None;
            self.set_notice(None);
            changed = true;
        }
        if let Some(deadline_ms) = self.hide_deadline_ms {
            if now_ms >= deadline_ms {
                self.visibility = OverlayVisibility::Hidden;
                self.active_session_id = None;
                self.last_seq_by_producer.clear();
                self.hide_deadline_ms = None;
                self.notice_deadline_ms = None;
                return true;
            }
        }
        changed
    }

    fn set_notice(&mut self, notice: Option<String>) {
        if let OverlayVisibility::Visible(view) = &mut self.visibility {
            view.notice = notice;
        }
    }

    fn view_for(
        &mut self,
        session_id: Uuid,
        producer: OverlayTextProducer,
        now_ms: u64,
    ) -> &mut SessionView {
        let replace = match &self.visibility {
            OverlayVisibility::Visible(view) => view.session_id != session_id,
            OverlayVisibility::Hidden => true,
        };
        if replace {
            self.visibility =
                OverlayVisibility::Visible(SessionView::new(session_id, producer, now_ms));
            self.hide_deadline_ms = None;
            self.notice_deadline_ms = None;
        }
        match &mut self.visibility {
            OverlayVisibility::Visible(view) => view,
            OverlayVisibility::Hidden => unreachable!("view_for always leaves a visible view"),
        }
    }

    fn apply_seq(
        &mut self,
        session_id: Uuid,
        producer: OverlayTextProducer,
        seq: u64,
    ) -> Option<ApplyOutcome> {
        if self.active_session_id != Some(session_id) {
            self.active_session_id = Some(session_id);
            self.last_seq_by_producer.clear();
        }

        if let Some(last_seq) = self.last_seq_by_producer.get(&producer) {
            if seq <= *last_seq {
                return Some(ApplyOutcome::DroppedStaleSeq);
            }
        }

        self.last_seq_by_producer.insert(producer, seq);
        None
    }
}

impl Default for OverlayStateMachine {
    fn default() -> Self {
        Self::new(Duration::from_millis(DEFAULT_AUTO_HIDE_AFTER_MS))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use crate::overlay_ipc::{OverlayIpcMessage, OverlayTextProducer};

    use super::{
        ApplyOutcome, OverlayMode, OverlayPhase, OverlayStateMachine, OverlayVisibility,
        SessionView, DONE_LINGER_MS,
    };

    fn machine() -> OverlayStateMachine {
        OverlayStateMachine::new(Duration::from_millis(500))
    }

    fn state(session_id: Uuid, seq: u64, state: &str) -> OverlayIpcMessage {
        OverlayIpcMessage::InterimState {
            session_id,
            producer: OverlayTextProducer::DaemonSttInterim,
            seq,
            state: state.to_string(),
        }
    }

    fn text(session_id: Uuid, seq: u64, text: &str) -> OverlayIpcMessage {
        OverlayIpcMessage::InterimText {
            session_id,
            producer: OverlayTextProducer::DaemonSttInterim,
            seq,
            text: text.to_string(),
        }
    }

    fn llm_state(session_id: Uuid, seq: u64, state: &str) -> OverlayIpcMessage {
        OverlayIpcMessage::InterimState {
            session_id,
            producer: OverlayTextProducer::LlmAnswerDelta,
            seq,
            state: state.to_string(),
        }
    }

    fn llm_text(session_id: Uuid, seq: u64, text: &str) -> OverlayIpcMessage {
        OverlayIpcMessage::InterimText {
            session_id,
            producer: OverlayTextProducer::LlmAnswerDelta,
            seq,
            text: text.to_string(),
        }
    }

    fn ended(session_id: Uuid, reason: &str) -> OverlayIpcMessage {
        OverlayIpcMessage::SessionEnded {
            session_id,
            reason: Some(reason.to_string()),
        }
    }

    fn injected(session_id: Uuid, success: bool) -> OverlayIpcMessage {
        OverlayIpcMessage::InjectionComplete {
            session_id,
            success,
            copy_only: false,
        }
    }

    fn view(machine: &OverlayStateMachine) -> &SessionView {
        machine
            .visibility()
            .view()
            .expect("overlay should be visible")
    }

    #[test]
    fn stt_timeline_listening_interim_finalizing_done_hidden() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();

        assert_eq!(
            machine.apply_event(state(session_id, 1, "listening"), 100),
            ApplyOutcome::Applied
        );
        assert_eq!(view(&machine).phase, OverlayPhase::Listening);
        assert_eq!(view(&machine).mode, OverlayMode::Stt);
        assert_eq!(view(&machine).started_ms, 100);

        machine.apply_event(text(session_id, 2, "hello"), 400);
        assert_eq!(view(&machine).phase, OverlayPhase::Interim);
        assert_eq!(view(&machine).transcript, "hello");
        assert_eq!(view(&machine).elapsed_ms(1_100), 1_000);

        machine.apply_event(ended(session_id, "final"), 2_000);
        assert_eq!(view(&machine).phase, OverlayPhase::Finalizing);
        assert_eq!(view(&machine).ended_ms, Some(2_000));
        assert_eq!(
            view(&machine).elapsed_ms(5_000),
            1_900,
            "the timer freezes at session end"
        );
        assert!(!view(&machine).failed());

        assert_eq!(
            machine.apply_event(injected(session_id, true), 2_300),
            ApplyOutcome::Applied
        );
        assert_eq!(view(&machine).phase, OverlayPhase::Done { success: true });
        assert!(!machine.advance_time(2_300 + DONE_LINGER_MS - 1));
        assert!(machine.advance_time(2_300 + DONE_LINGER_MS));
        assert_eq!(machine.visibility(), &OverlayVisibility::Hidden);
    }

    #[test]
    fn transcript_text_after_finalizing_state_keeps_the_phase() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "send the"), 0);
        machine.apply_event(state(session_id, 2, "finalizing"), 100);
        assert_eq!(view(&machine).phase, OverlayPhase::Finalizing);
        machine.apply_event(text(session_id, 3, "send the invoice"), 200);
        assert_eq!(view(&machine).phase, OverlayPhase::Finalizing);
        assert_eq!(view(&machine).transcript, "send the invoice");
    }

    #[test]
    fn daemon_state_values_never_reach_the_transcript() {
        for value in [
            "listening",
            "processing",
            "interim",
            "finalizing",
            "something-new",
        ] {
            let mut machine = machine();
            let session_id = Uuid::new_v4();
            machine.apply_event(text(session_id, 1, "kept"), 0);
            machine.apply_event(state(session_id, 2, value), 10);
            assert_eq!(
                view(&machine).transcript,
                "kept",
                "state {value} must not overwrite prose"
            );
            let expected = match value {
                "finalizing" => OverlayPhase::Finalizing,
                _ => OverlayPhase::Interim,
            };
            assert_eq!(view(&machine).phase, expected, "state {value}");
        }
    }

    #[test]
    fn llm_timeline_keeps_the_question_and_streams_the_answer() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 5, "what is a mutex"), 0);
        machine.apply_event(llm_state(session_id, 1, "Generating answer..."), 900);
        let v = view(&machine);
        assert_eq!(v.mode, OverlayMode::Llm);
        assert_eq!(v.phase, OverlayPhase::Answering);
        assert_eq!(v.question.as_deref(), Some("what is a mutex"));
        assert_eq!(v.status.as_deref(), Some("Generating answer..."));

        machine.apply_event(llm_text(session_id, 2, "A mutex"), 1_200);
        assert_eq!(view(&machine).answer, "A mutex");
        assert_eq!(view(&machine).status, None);

        machine.apply_event(text(session_id, 6, "late daemon text"), 1_300);
        assert_eq!(view(&machine).question.as_deref(), Some("what is a mutex"));
        assert_eq!(
            view(&machine).answer,
            "A mutex",
            "daemon text does not disturb the answer"
        );

        machine.apply_event(ended(session_id, "final"), 2_000);
        assert_eq!(
            view(&machine).phase,
            OverlayPhase::Answering,
            "session end keeps the answer up"
        );
        machine.apply_event(injected(session_id, true), 2_100);
        assert_eq!(view(&machine).phase, OverlayPhase::Done { success: true });
    }

    #[test]
    fn nil_session_busy_notice_overlays_the_current_view() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "first question"), 0);
        machine.apply_event(llm_text(session_id, 1, "answering the first"), 500);

        machine.apply_event(
            llm_state(Uuid::nil(), 1, "LLM busy; wait for current answer"),
            600,
        );
        let v = view(&machine);
        assert_eq!(v.session_id, session_id);
        assert_eq!(v.answer, "answering the first");
        assert_eq!(
            v.notice.as_deref(),
            Some("LLM busy; wait for current answer")
        );

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::SessionEnded {
                    session_id: Uuid::nil(),
                    reason: Some("busy".to_string()),
                },
                700
            ),
            ApplyOutcome::Applied
        );
        // The notice outlives the nil SessionEnded that closes the busy sequence.
        assert_eq!(
            view(&machine).notice.as_deref(),
            Some("LLM busy; wait for current answer")
        );
        assert_eq!(view(&machine).reason, None);
        assert!(!machine.advance_time(700 + 499));
        assert!(machine.advance_time(700 + 500));
        assert_eq!(view(&machine).notice, None);
        assert_eq!(view(&machine).reason, None);

        assert_eq!(
            machine.apply_event(ended(session_id, "final"), 800),
            ApplyOutcome::Applied
        );
        assert_eq!(
            machine.apply_event(injected(session_id, true), 900),
            ApplyOutcome::Applied
        );
        assert_eq!(view(&machine).phase, OverlayPhase::Done { success: true });
    }

    #[test]
    fn warning_stores_remaining_time_and_counts_down() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "long talk"), 0);
        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::SessionWarning {
                    session_id,
                    remaining_seconds: Some(120.0),
                    limit_seconds: Some(600.0),
                },
                480_000
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(view(&machine).remaining_ms(480_000), Some(120_000));
        assert_eq!(view(&machine).remaining_ms(500_000), Some(100_000));

        machine.apply_event(ended(session_id, "final"), 510_000);
        assert_eq!(
            view(&machine).remaining_ms(600_000),
            Some(90_000),
            "frozen at session end"
        );
    }

    #[test]
    fn warning_without_remaining_is_kept_without_a_countdown() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "talk"), 0);
        machine.apply_event(
            OverlayIpcMessage::SessionWarning {
                session_id,
                remaining_seconds: None,
                limit_seconds: None,
            },
            100,
        );
        assert!(view(&machine).warning.is_some());
        assert_eq!(view(&machine).remaining_ms(200), None);
    }

    #[test]
    fn warning_with_only_the_cap_counts_down_from_the_cap_minus_elapsed() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "talk"), 0);
        machine.apply_event(
            OverlayIpcMessage::SessionWarning {
                session_id,
                remaining_seconds: None,
                limit_seconds: Some(10.0),
            },
            4_000,
        );
        assert_eq!(view(&machine).remaining_ms(4_000), Some(6_000));
        assert_eq!(view(&machine).remaining_ms(9_000), Some(1_000));
    }

    #[test]
    fn copy_only_injection_is_remembered_on_the_view() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "talk"), 0);
        machine.apply_event(
            OverlayIpcMessage::InjectionComplete {
                session_id,
                success: true,
                copy_only: true,
            },
            100,
        );
        assert_eq!(view(&machine).phase, OverlayPhase::Done { success: true });
        assert!(view(&machine).copy_only);
    }

    #[test]
    fn warning_for_other_session_is_dropped() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "talk"), 0);
        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::SessionWarning {
                    session_id: Uuid::new_v4(),
                    remaining_seconds: Some(1.0),
                    limit_seconds: Some(2.0),
                },
                10
            ),
            ApplyOutcome::DroppedSessionMismatch
        );
        assert_eq!(view(&machine).warning, None);
    }

    #[test]
    fn session_end_without_injection_auto_hides() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "hello"), 0);
        machine.apply_event(ended(session_id, "final"), 1_000);
        assert!(!machine.advance_time(1_499));
        assert!(machine.advance_time(1_500));
        assert_eq!(machine.visibility(), &OverlayVisibility::Hidden);
    }

    #[test]
    fn abort_reason_marks_the_session_failed() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(state(session_id, 1, "listening"), 0);
        machine.apply_event(ended(session_id, "abort"), 100);
        assert!(view(&machine).failed());
        assert!(!view(&machine).has_text());
    }

    #[test]
    fn session_end_while_hidden_creates_a_finalizing_view() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        assert_eq!(
            machine.apply_event(ended(session_id, "error"), 50),
            ApplyOutcome::Applied
        );
        assert_eq!(view(&machine).phase, OverlayPhase::Finalizing);
        assert_eq!(view(&machine).started_ms, 50);
    }

    #[test]
    fn injection_complete_is_accepted_from_any_visible_phase() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "hello"), 0);
        assert_eq!(
            machine.apply_event(injected(session_id, false), 10),
            ApplyOutcome::Applied
        );
        assert_eq!(view(&machine).phase, OverlayPhase::Done { success: false });
        assert_eq!(
            machine.apply_event(injected(Uuid::new_v4(), true), 20),
            ApplyOutcome::DroppedSessionMismatch
        );
    }

    #[test]
    fn injection_complete_while_hidden_is_dropped() {
        let mut machine = machine();
        assert_eq!(
            machine.apply_event(injected(Uuid::new_v4(), true), 0),
            ApplyOutcome::DroppedSessionMismatch
        );
        assert_eq!(machine.visibility(), &OverlayVisibility::Hidden);
    }

    #[test]
    fn state_machine_drops_stale_sequence_numbers() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        assert_eq!(
            machine.apply_event(text(session_id, 5, "five"), 0),
            ApplyOutcome::Applied
        );
        assert_eq!(
            machine.apply_event(text(session_id, 5, "stale"), 1),
            ApplyOutcome::DroppedStaleSeq
        );
        assert_eq!(
            machine.apply_event(text(session_id, 4, "older"), 2),
            ApplyOutcome::DroppedStaleSeq
        );
        assert_eq!(view(&machine).transcript, "five");
    }

    #[test]
    fn state_machine_sequences_are_independent_by_text_producer() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        assert_eq!(
            machine.apply_event(text(session_id, 10, "daemon"), 0),
            ApplyOutcome::Applied
        );
        assert_eq!(
            machine.apply_event(llm_text(session_id, 1, "answer"), 1),
            ApplyOutcome::Applied
        );
        assert_eq!(
            machine.apply_event(llm_text(session_id, 1, "stale"), 2),
            ApplyOutcome::DroppedStaleSeq
        );
        assert_eq!(
            machine.apply_event(text(session_id, 10, "stale"), 3),
            ApplyOutcome::DroppedStaleSeq
        );
        assert_eq!(view(&machine).answer, "answer");
        assert_eq!(view(&machine).question.as_deref(), Some("daemon"));
    }

    #[test]
    fn session_ended_for_other_session_is_dropped() {
        let mut machine = machine();
        let session_id = Uuid::new_v4();
        machine.apply_event(text(session_id, 1, "hello"), 0);
        assert_eq!(
            machine.apply_event(ended(Uuid::new_v4(), "final"), 10),
            ApplyOutcome::DroppedSessionMismatch
        );
        assert_eq!(view(&machine).phase, OverlayPhase::Interim);
    }

    #[test]
    fn sequence_resets_for_new_session() {
        let mut machine = machine();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        machine.apply_event(text(first, 9, "first"), 0);
        assert_eq!(
            machine.apply_event(text(second, 1, "second"), 10),
            ApplyOutcome::Applied
        );
        let v = view(&machine);
        assert_eq!(v.session_id, second);
        assert_eq!(v.transcript, "second");
        assert_eq!(v.started_ms, 10, "a new session restarts the timer");
    }
}
