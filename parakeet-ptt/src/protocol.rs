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

#[derive(Debug, Serialize, Deserialize)]
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
    use super::ServerMessage;

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
}
