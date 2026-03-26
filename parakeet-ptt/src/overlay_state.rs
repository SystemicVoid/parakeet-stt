use std::time::Duration;

use uuid::Uuid;

use crate::overlay_ipc::OverlayIpcMessage;

pub const DEFAULT_AUTO_HIDE_AFTER_MS: u64 = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayVisibility {
    Hidden,
    Listening {
        session_id: Uuid,
    },
    Interim {
        session_id: Uuid,
        text: String,
    },
    Finalizing {
        session_id: Uuid,
        reason: Option<String>,
        last_text: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayRenderPhase {
    Hidden,
    Listening,
    Interim,
    Finalizing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayRenderIntent {
    pub phase: OverlayRenderPhase,
    pub visible: bool,
    pub headline: String,
    pub detail: Option<String>,
    pub warning: bool,
}

impl OverlayVisibility {
    pub fn to_render_intent(&self, warning: bool) -> OverlayRenderIntent {
        match self {
            Self::Hidden => OverlayRenderIntent {
                phase: OverlayRenderPhase::Hidden,
                visible: false,
                headline: String::new(),
                detail: None,
                warning: false,
            },
            Self::Listening { .. } => OverlayRenderIntent {
                phase: OverlayRenderPhase::Listening,
                visible: true,
                headline: "Listening...".to_string(),
                detail: None,
                warning,
            },
            Self::Interim { text, .. } => OverlayRenderIntent {
                phase: OverlayRenderPhase::Interim,
                visible: true,
                headline: if text.trim().is_empty() {
                    "Listening...".to_string()
                } else {
                    text.clone()
                },
                detail: None,
                warning,
            },
            Self::Finalizing {
                reason, last_text, ..
            } => OverlayRenderIntent {
                phase: OverlayRenderPhase::Finalizing,
                visible: true,
                headline: last_text
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
                    .or_else(|| {
                        reason
                            .as_ref()
                            .filter(|value| !value.trim().is_empty())
                            .cloned()
                    })
                    .unwrap_or_else(|| "Finalizing...".to_string()),
                detail: None,
                warning,
            },
        }
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
    last_seq: Option<u64>,
    finalize_deadline_ms: Option<u64>,
    auto_hide_after_ms: u64,
    warning_active: bool,
}

impl OverlayStateMachine {
    pub fn new(auto_hide_after: Duration) -> Self {
        Self {
            visibility: OverlayVisibility::Hidden,
            active_session_id: None,
            last_seq: None,
            finalize_deadline_ms: None,
            auto_hide_after_ms: auto_hide_after.as_millis() as u64,
            warning_active: false,
        }
    }

    pub fn visibility(&self) -> &OverlayVisibility {
        &self.visibility
    }

    pub fn warning_active(&self) -> bool {
        self.warning_active
    }

    pub fn apply_event(&mut self, message: OverlayIpcMessage, now_ms: u64) -> ApplyOutcome {
        match message {
            OverlayIpcMessage::OutputHint { .. } => ApplyOutcome::Applied,
            OverlayIpcMessage::InterimState {
                session_id,
                seq,
                state,
            } => {
                if let Some(outcome) = self.apply_seq(session_id, seq) {
                    return outcome;
                }
                if state == "listening" {
                    self.visibility = OverlayVisibility::Listening { session_id };
                } else {
                    self.visibility = OverlayVisibility::Interim {
                        session_id,
                        text: state,
                    };
                }
                self.finalize_deadline_ms = None;
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::InterimText {
                session_id,
                seq,
                text,
            } => {
                if let Some(outcome) = self.apply_seq(session_id, seq) {
                    return outcome;
                }
                self.visibility = OverlayVisibility::Interim { session_id, text };
                self.finalize_deadline_ms = None;
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::AudioLevel { .. } => ApplyOutcome::Applied,
            OverlayIpcMessage::SessionEnded { session_id, reason } => {
                if let Some(active_session_id) = self.active_session_id {
                    if active_session_id != session_id {
                        return ApplyOutcome::DroppedSessionMismatch;
                    }
                }

                let last_text = match &self.visibility {
                    OverlayVisibility::Interim { text, .. } if !text.trim().is_empty() => {
                        Some(text.clone())
                    }
                    _ => None,
                };

                self.active_session_id = Some(session_id);
                self.last_seq = None;
                self.warning_active = false;
                self.visibility = OverlayVisibility::Finalizing {
                    session_id,
                    reason,
                    last_text,
                };
                self.finalize_deadline_ms = Some(now_ms.saturating_add(self.auto_hide_after_ms));
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::SessionWarning { session_id } => {
                if self.active_session_id != Some(session_id) {
                    return ApplyOutcome::DroppedSessionMismatch;
                }
                self.warning_active = true;
                ApplyOutcome::Applied
            }
            OverlayIpcMessage::InjectionComplete {
                session_id,
                success: _,
            } => match &self.visibility {
                OverlayVisibility::Finalizing {
                    session_id: finalizing_session,
                    ..
                } if *finalizing_session == session_id => {
                    self.visibility = OverlayVisibility::Hidden;
                    self.active_session_id = None;
                    self.last_seq = None;
                    self.finalize_deadline_ms = None;
                    self.warning_active = false;
                    ApplyOutcome::Applied
                }
                _ => ApplyOutcome::DroppedSessionMismatch,
            },
        }
    }

    pub fn advance_time(&mut self, now_ms: u64) -> bool {
        if let Some(deadline_ms) = self.finalize_deadline_ms {
            if now_ms >= deadline_ms {
                self.visibility = OverlayVisibility::Hidden;
                self.active_session_id = None;
                self.last_seq = None;
                self.finalize_deadline_ms = None;
                self.warning_active = false;
                return true;
            }
        }

        false
    }

    fn apply_seq(&mut self, session_id: Uuid, seq: u64) -> Option<ApplyOutcome> {
        if self.active_session_id != Some(session_id) {
            self.active_session_id = Some(session_id);
            self.last_seq = None;
            self.warning_active = false;
        }

        if let Some(last_seq) = self.last_seq {
            if seq <= last_seq {
                return Some(ApplyOutcome::DroppedStaleSeq);
            }
        }

        self.last_seq = Some(seq);
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

    use crate::overlay_ipc::OverlayIpcMessage;

    use super::{
        ApplyOutcome, OverlayRenderIntent, OverlayRenderPhase, OverlayStateMachine,
        OverlayVisibility,
    };

    #[test]
    fn state_machine_transitions_listening_interim_finalizing_hidden() {
        let mut machine = OverlayStateMachine::new(Duration::from_millis(500));
        let session_id = Uuid::new_v4();

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InterimState {
                    session_id,
                    seq: 1,
                    state: "listening".to_string(),
                },
                0
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(
            machine.visibility(),
            &OverlayVisibility::Listening { session_id }
        );

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InterimText {
                    session_id,
                    seq: 2,
                    text: "hello".to_string(),
                },
                10
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(
            machine.visibility(),
            &OverlayVisibility::Interim {
                session_id,
                text: "hello".to_string(),
            }
        );

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::SessionEnded {
                    session_id,
                    reason: Some("normal".to_string()),
                },
                20
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(
            machine.visibility(),
            &OverlayVisibility::Finalizing {
                session_id,
                reason: Some("normal".to_string()),
                last_text: Some("hello".to_string()),
            }
        );

        assert!(!machine.advance_time(519));
        assert_eq!(
            machine.visibility(),
            &OverlayVisibility::Finalizing {
                session_id,
                reason: Some("normal".to_string()),
                last_text: Some("hello".to_string()),
            }
        );

        assert!(machine.advance_time(520));
        assert_eq!(machine.visibility(), &OverlayVisibility::Hidden);
    }

    #[test]
    fn state_machine_drops_stale_sequence_numbers() {
        let mut machine = OverlayStateMachine::default();
        let session_id = Uuid::new_v4();

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InterimText {
                    session_id,
                    seq: 10,
                    text: "newest".to_string(),
                },
                0
            ),
            ApplyOutcome::Applied
        );

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InterimText {
                    session_id,
                    seq: 9,
                    text: "stale".to_string(),
                },
                1
            ),
            ApplyOutcome::DroppedStaleSeq
        );

        assert_eq!(
            machine.visibility(),
            &OverlayVisibility::Interim {
                session_id,
                text: "newest".to_string(),
            }
        );
    }

    #[test]
    fn session_ended_for_other_session_is_dropped() {
        let mut machine = OverlayStateMachine::default();
        let active_session = Uuid::new_v4();
        let other_session = Uuid::new_v4();

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InterimState {
                    session_id: active_session,
                    seq: 1,
                    state: "listening".to_string(),
                },
                0
            ),
            ApplyOutcome::Applied
        );

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::SessionEnded {
                    session_id: other_session,
                    reason: Some("ignored".to_string()),
                },
                3
            ),
            ApplyOutcome::DroppedSessionMismatch
        );

        assert_eq!(
            machine.visibility(),
            &OverlayVisibility::Listening {
                session_id: active_session
            }
        );
    }

    #[test]
    fn sequence_resets_for_new_session() {
        let mut machine = OverlayStateMachine::default();
        let old_session = Uuid::new_v4();
        let new_session = Uuid::new_v4();

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InterimText {
                    session_id: old_session,
                    seq: 30,
                    text: "old".to_string(),
                },
                0
            ),
            ApplyOutcome::Applied
        );

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InterimText {
                    session_id: new_session,
                    seq: 1,
                    text: "new".to_string(),
                },
                1
            ),
            ApplyOutcome::Applied
        );

        assert_eq!(
            machine.visibility(),
            &OverlayVisibility::Interim {
                session_id: new_session,
                text: "new".to_string(),
            }
        );
    }

    #[test]
    fn hidden_visibility_maps_to_hidden_render_intent() {
        assert_eq!(
            OverlayVisibility::Hidden.to_render_intent(false),
            OverlayRenderIntent {
                phase: OverlayRenderPhase::Hidden,
                visible: false,
                headline: String::new(),
                detail: None,
                warning: false,
            }
        );
    }

    #[test]
    fn interim_empty_text_maps_to_listening_headline() {
        let session_id = Uuid::new_v4();
        assert_eq!(
            OverlayVisibility::Interim {
                session_id,
                text: "   ".to_string(),
            }
            .to_render_intent(false),
            OverlayRenderIntent {
                phase: OverlayRenderPhase::Interim,
                visible: true,
                headline: "Listening...".to_string(),
                detail: None,
                warning: false,
            }
        );
    }

    #[test]
    fn finalizing_without_reason_uses_default_headline() {
        let session_id = Uuid::new_v4();
        assert_eq!(
            OverlayVisibility::Finalizing {
                session_id,
                reason: None,
                last_text: None,
            }
            .to_render_intent(false),
            OverlayRenderIntent {
                phase: OverlayRenderPhase::Finalizing,
                visible: true,
                headline: "Finalizing...".to_string(),
                detail: None,
                warning: false,
            }
        );
    }

    #[test]
    fn finalizing_prefers_last_text_headline() {
        let session_id = Uuid::new_v4();
        assert_eq!(
            OverlayVisibility::Finalizing {
                session_id,
                reason: Some("normal".to_string()),
                last_text: Some("recognized text".to_string()),
            }
            .to_render_intent(false),
            OverlayRenderIntent {
                phase: OverlayRenderPhase::Finalizing,
                visible: true,
                headline: "recognized text".to_string(),
                detail: None,
                warning: false,
            }
        );
    }

    #[test]
    fn injection_complete_hides_matching_finalizing_session() {
        let mut machine = OverlayStateMachine::new(Duration::from_millis(500));
        let session_id = Uuid::new_v4();

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InterimText {
                    session_id,
                    seq: 1,
                    text: "hello".to_string(),
                },
                0
            ),
            ApplyOutcome::Applied
        );

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::SessionEnded {
                    session_id,
                    reason: Some("normal".to_string()),
                },
                10
            ),
            ApplyOutcome::Applied
        );

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InjectionComplete {
                    session_id,
                    success: true,
                },
                20
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(machine.visibility(), &OverlayVisibility::Hidden);
        assert!(!machine.advance_time(510));
    }

    #[test]
    fn injection_complete_before_finalizing_is_dropped() {
        let mut machine = OverlayStateMachine::default();
        let session_id = Uuid::new_v4();

        assert_eq!(
            machine.apply_event(
                OverlayIpcMessage::InjectionComplete {
                    session_id,
                    success: true,
                },
                0
            ),
            ApplyOutcome::DroppedSessionMismatch
        );
    }
}
