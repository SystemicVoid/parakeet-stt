use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayTextProducer {
    #[default]
    DaemonSttInterim,
    LlmAnswerDelta,
}

impl OverlayTextProducer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DaemonSttInterim => "daemon_stt_interim",
            Self::LlmAnswerDelta => "llm_answer_delta",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayIpcMessage {
    OutputHint {
        output_name: String,
    },
    InterimState {
        session_id: Uuid,
        #[serde(default)]
        producer: OverlayTextProducer,
        seq: u64,
        state: String,
    },
    InterimText {
        session_id: Uuid,
        #[serde(default)]
        producer: OverlayTextProducer,
        seq: u64,
        text: String,
    },
    AudioLevel {
        session_id: Uuid,
        level_db: f32,
    },
    InjectionComplete {
        session_id: Uuid,
        success: bool,
        /// The client runs copy-only injection, so a success left the text on the
        /// clipboard rather than pasting it.
        #[serde(default)]
        copy_only: bool,
    },
    SessionEnded {
        session_id: Uuid,
        reason: Option<String>,
    },
    SessionWarning {
        session_id: Uuid,
        /// Seconds left before the Session cap when the warning fired.
        #[serde(default)]
        remaining_seconds: Option<f32>,
        /// The Session cap itself, in seconds.
        #[serde(default)]
        limit_seconds: Option<f32>,
    },
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{OverlayIpcMessage, OverlayTextProducer};

    #[test]
    fn overlay_ipc_message_round_trips_as_tagged_json() {
        let session_id = Uuid::new_v4();
        let message = OverlayIpcMessage::InterimText {
            session_id,
            producer: OverlayTextProducer::DaemonSttInterim,
            seq: 7,
            text: "hello".to_string(),
        };

        let encoded = serde_json::to_string(&message).expect("message should serialize");
        assert!(encoded.contains("\"type\":\"interim_text\""));
        assert!(encoded.contains("\"producer\":\"daemon_stt_interim\""));

        let decoded: OverlayIpcMessage =
            serde_json::from_str(&encoded).expect("message should deserialize");
        assert_eq!(decoded, message);
    }

    #[test]
    fn interim_messages_without_producer_decode_as_daemon_stt() {
        let session_id = Uuid::new_v4();
        let encoded = format!(
            r#"{{"type":"interim_text","session_id":"{session_id}","seq":7,"text":"hello"}}"#
        );

        let decoded: OverlayIpcMessage =
            serde_json::from_str(&encoded).expect("message should deserialize");
        assert_eq!(
            decoded,
            OverlayIpcMessage::InterimText {
                session_id,
                producer: OverlayTextProducer::DaemonSttInterim,
                seq: 7,
                text: "hello".to_string(),
            }
        );
    }

    #[test]
    fn audio_level_serialization_roundtrip() {
        let session_id = Uuid::new_v4();
        let message = OverlayIpcMessage::AudioLevel {
            session_id,
            level_db: -30.5,
        };

        let encoded = serde_json::to_string(&message).expect("message should serialize");
        assert!(encoded.contains("\"type\":\"audio_level\""));

        let decoded: OverlayIpcMessage =
            serde_json::from_str(&encoded).expect("message should deserialize");
        assert_eq!(decoded, message);
    }

    #[test]
    fn session_warning_decodes_without_cap_fields() {
        let session_id = Uuid::new_v4();
        let encoded = format!(r#"{{"type":"session_warning","session_id":"{session_id}"}}"#);
        let decoded: OverlayIpcMessage =
            serde_json::from_str(&encoded).expect("message should deserialize");
        assert_eq!(
            decoded,
            OverlayIpcMessage::SessionWarning {
                session_id,
                remaining_seconds: None,
                limit_seconds: None,
            }
        );

        let message = OverlayIpcMessage::SessionWarning {
            session_id,
            remaining_seconds: Some(120.0),
            limit_seconds: Some(600.0),
        };
        let encoded = serde_json::to_string(&message).expect("message should serialize");
        assert!(encoded.contains("\"remaining_seconds\":120.0"));
        let decoded: OverlayIpcMessage =
            serde_json::from_str(&encoded).expect("message should deserialize");
        assert_eq!(decoded, message);
    }

    #[test]
    fn output_hint_serialization_roundtrip() {
        let message = OverlayIpcMessage::OutputHint {
            output_name: "HDMI-A-1".to_string(),
        };

        let encoded = serde_json::to_string(&message).expect("message should serialize");
        assert!(encoded.contains("\"type\":\"output_hint\""));

        let decoded: OverlayIpcMessage =
            serde_json::from_str(&encoded).expect("message should deserialize");
        assert_eq!(decoded, message);
    }

    #[test]
    fn injection_complete_serialization_roundtrip() {
        let session_id = Uuid::new_v4();
        let message = OverlayIpcMessage::InjectionComplete {
            session_id,
            success: true,
            copy_only: false,
        };

        let encoded = serde_json::to_string(&message).expect("message should serialize");
        assert!(encoded.contains("\"type\":\"injection_complete\""));

        let decoded: OverlayIpcMessage =
            serde_json::from_str(&encoded).expect("message should deserialize");
        assert_eq!(decoded, message);
    }
}
