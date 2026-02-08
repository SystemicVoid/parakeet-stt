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
