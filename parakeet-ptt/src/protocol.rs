use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // Keep wire-level message names aligned with protocol spec.
pub enum ClientMessage {
    StartSession {
        session_id: Uuid,
        mode: String,
        preferred_lang: Option<String>,
        timestamp: String,
    },
    StopSession {
        session_id: Uuid,
        timestamp: String,
    },
    AbortSession {
        session_id: Uuid,
        reason: String,
        timestamp: String,
    },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    SessionStarted {
        session_id: Uuid,
        ts: String,
        mic_device: Option<String>,
        lang: Option<String>,
    },
    FinalResult {
        session_id: Uuid,
        text: String,
        latency_ms: u64,
        audio_ms: u64,
        lang: Option<String>,
        confidence: Option<f32>,
    },
    Error {
        session_id: Option<Uuid>,
        code: String,
        message: String,
    },
    Status {
        state: String,
        sessions_active: u32,
        gpu_mem_mb: Option<u64>,
        device: Option<String>,
        effective_device: Option<String>,
        streaming_enabled: Option<bool>,
        stream_helper_active: Option<bool>,
        stream_fallback_reason: Option<String>,
        chunk_secs: Option<f64>,
        active_session_age_ms: Option<u64>,
        audio_stop_ms: Option<u64>,
        finalize_ms: Option<u64>,
        infer_ms: Option<u64>,
        send_ms: Option<u64>,
        last_audio_ms: Option<u64>,
        last_infer_ms: Option<u64>,
        last_send_ms: Option<u64>,
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
}

#[derive(Debug, PartialEq)]
pub enum DecodedServerMessage {
    Known(Box<ServerMessage>),
    UnknownType { message_type: String },
}

#[derive(Debug, Deserialize)]
struct MessageTypeEnvelope {
    #[serde(rename = "type")]
    message_type: String,
}

pub fn decode_server_message(raw: &str) -> Result<DecodedServerMessage, serde_json::Error> {
    match serde_json::from_str::<ServerMessage>(raw) {
        Ok(message) => Ok(DecodedServerMessage::Known(Box::new(message))),
        Err(err) => {
            let envelope = serde_json::from_str::<MessageTypeEnvelope>(raw);
            match envelope {
                Ok(envelope) if !is_known_server_message_type(&envelope.message_type) => {
                    Ok(DecodedServerMessage::UnknownType {
                        message_type: envelope.message_type,
                    })
                }
                _ => Err(err),
            }
        }
    }
}

fn is_known_server_message_type(message_type: &str) -> bool {
    matches!(
        message_type,
        "session_started"
            | "final_result"
            | "error"
            | "status"
            | "interim_state"
            | "interim_text"
            | "audio_level"
            | "session_ended"
    )
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub fn start_message(session_id: Uuid, preferred_lang: Option<String>) -> ClientMessage {
    ClientMessage::StartSession {
        session_id,
        mode: "push_to_talk".to_string(),
        preferred_lang,
        timestamp: now_rfc3339(),
    }
}

pub fn stop_message(session_id: Uuid) -> ClientMessage {
    ClientMessage::StopSession {
        session_id,
        timestamp: now_rfc3339(),
    }
}

#[allow(dead_code)]
pub fn abort_message(session_id: Uuid, reason: &str) -> ClientMessage {
    ClientMessage::AbortSession {
        session_id,
        reason: reason.to_string(),
        timestamp: now_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{decode_server_message, DecodedServerMessage, ServerMessage};

    #[test]
    fn status_deserializes_when_optional_fields_are_missing() {
        let raw = r#"{"type":"status","state":"idle","sessions_active":0}"#;
        let msg: ServerMessage =
            serde_json::from_str(raw).expect("status payload should deserialize");

        match msg {
            ServerMessage::Status {
                state,
                sessions_active,
                gpu_mem_mb,
                device,
                effective_device,
                streaming_enabled,
                stream_helper_active,
                stream_fallback_reason,
                chunk_secs,
                active_session_age_ms,
                audio_stop_ms,
                finalize_ms,
                infer_ms,
                send_ms,
                last_audio_ms,
                last_infer_ms,
                last_send_ms,
            } => {
                assert_eq!(state, "idle");
                assert_eq!(sessions_active, 0);
                assert_eq!(gpu_mem_mb, None);
                assert_eq!(device, None);
                assert_eq!(effective_device, None);
                assert_eq!(streaming_enabled, None);
                assert_eq!(stream_helper_active, None);
                assert_eq!(stream_fallback_reason, None);
                assert_eq!(chunk_secs, None);
                assert_eq!(active_session_age_ms, None);
                assert_eq!(audio_stop_ms, None);
                assert_eq!(finalize_ms, None);
                assert_eq!(infer_ms, None);
                assert_eq!(send_ms, None);
                assert_eq!(last_audio_ms, None);
                assert_eq!(last_infer_ms, None);
                assert_eq!(last_send_ms, None);
            }
            other => panic!("expected status message, got {other:?}"),
        }
    }

    #[test]
    fn decode_server_message_nonfatal_for_unknown_type() {
        let raw = r#"{"type":"daemon_future_extension","foo":"bar"}"#;

        let decoded = decode_server_message(raw).expect("unknown type should be tolerated");
        assert_eq!(
            decoded,
            DecodedServerMessage::UnknownType {
                message_type: "daemon_future_extension".to_string()
            }
        );
    }

    #[test]
    fn decode_server_message_known_variants_decode_normally() {
        let session_id = Uuid::new_v4();
        let raw = format!(
            r#"{{"type":"interim_text","session_id":"{}","seq":4,"text":"hello"}}"#,
            session_id
        );

        let decoded = decode_server_message(&raw).expect("known message should decode");
        assert_eq!(
            decoded,
            DecodedServerMessage::Known(Box::new(ServerMessage::InterimText {
                session_id,
                seq: 4,
                text: "hello".to_string(),
            }))
        );
    }

    #[test]
    fn decode_server_message_mixed_version_stream_tolerates_unknown_between_known_messages() {
        let session_id = Uuid::new_v4();
        let messages = [
            format!(
                r#"{{"type":"session_started","session_id":"{}","ts":"2026-02-28T00:00:00Z","mic_device":null,"lang":"en"}}"#,
                session_id
            ),
            r#"{"type":"daemon_future_extension","foo":"bar"}"#.to_string(),
            format!(
                r#"{{"type":"interim_state","session_id":"{}","seq":0,"state":"listening"}}"#,
                session_id
            ),
            r#"{"type":"daemon_future_extension_v2","extra":true}"#.to_string(),
            format!(
                r#"{{"type":"final_result","session_id":"{}","text":"ok","latency_ms":12,"audio_ms":345,"lang":"en","confidence":0.9}}"#,
                session_id
            ),
        ];

        let decoded = messages
            .iter()
            .map(|raw| decode_server_message(raw).expect("decode should remain non-fatal"))
            .collect::<Vec<_>>();

        assert!(matches!(
            decoded[0],
            DecodedServerMessage::Known(ref message)
                if matches!(&**message, ServerMessage::SessionStarted { .. })
        ));
        assert_eq!(
            decoded[1],
            DecodedServerMessage::UnknownType {
                message_type: "daemon_future_extension".to_string()
            }
        );
        assert!(matches!(
            decoded[2],
            DecodedServerMessage::Known(ref message)
                if matches!(&**message, ServerMessage::InterimState { .. })
        ));
        assert_eq!(
            decoded[3],
            DecodedServerMessage::UnknownType {
                message_type: "daemon_future_extension_v2".to_string()
            }
        );
        assert!(matches!(
            decoded[4],
            DecodedServerMessage::Known(ref message)
                if matches!(&**message, ServerMessage::FinalResult { .. })
        ));
    }

    #[test]
    fn decode_server_message_preserves_error_for_known_type_shape_mismatch() {
        let raw = r#"{"type":"final_result","session_id":"00000000-0000-0000-0000-000000000000"}"#;

        let err = decode_server_message(raw).expect_err("known type mismatch should error");
        let msg = err.to_string();
        assert!(msg.contains("missing field"));
    }

    #[test]
    fn server_message_round_trips_for_all_known_variants() {
        let session_id = Uuid::new_v4();
        let messages = vec![
            ServerMessage::SessionStarted {
                session_id,
                ts: "2026-02-28T00:00:00Z".to_string(),
                mic_device: Some("default".to_string()),
                lang: Some("en".to_string()),
            },
            ServerMessage::FinalResult {
                session_id,
                text: "hello".to_string(),
                latency_ms: 42,
                audio_ms: 1000,
                lang: Some("en".to_string()),
                confidence: Some(0.9),
            },
            ServerMessage::Error {
                session_id: Some(session_id),
                code: "SESSION_ABORTED".to_string(),
                message: "aborted".to_string(),
            },
            ServerMessage::Status {
                state: "idle".to_string(),
                sessions_active: 0,
                gpu_mem_mb: None,
                device: Some("cpu".to_string()),
                effective_device: Some("cpu".to_string()),
                streaming_enabled: Some(false),
                stream_helper_active: Some(false),
                stream_fallback_reason: None,
                chunk_secs: None,
                active_session_age_ms: None,
                audio_stop_ms: None,
                finalize_ms: None,
                infer_ms: None,
                send_ms: None,
                last_audio_ms: None,
                last_infer_ms: None,
                last_send_ms: None,
            },
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "listening".to_string(),
            },
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "hello".to_string(),
            },
            ServerMessage::AudioLevel {
                session_id,
                level_db: -25.5,
            },
            ServerMessage::SessionEnded {
                session_id,
                reason: Some("final".to_string()),
            },
        ];

        for message in messages {
            let raw = serde_json::to_string(&message).expect("serialize known server message");
            let round_tripped: ServerMessage =
                serde_json::from_str(&raw).expect("deserialize known server message");
            assert_eq!(round_tripped, message);
        }
    }
}
