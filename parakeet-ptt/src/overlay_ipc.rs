use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayIpcMessage {
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
    SessionEnded {
        session_id: Uuid,
        reason: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::OverlayIpcMessage;

    #[test]
    fn overlay_ipc_message_round_trips_as_tagged_json() {
        let session_id = Uuid::new_v4();
        let message = OverlayIpcMessage::InterimText {
            session_id,
            seq: 7,
            text: "hello".to_string(),
        };

        let encoded = serde_json::to_string(&message).expect("message should serialize");
        assert!(encoded.contains("\"type\":\"interim_text\""));

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
}
