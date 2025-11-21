use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttState {
    Idle,
    Listening { session_id: Uuid },
    WaitingResult { session_id: Uuid },
}

impl PttState {
    pub fn new() -> Self {
        Self::Idle
    }

    pub fn begin_listening(&mut self) -> Option<Uuid> {
        match self {
            PttState::Idle => {
                let session_id = Uuid::new_v4();
                *self = PttState::Listening { session_id };
                Some(session_id)
            }
            _ => None,
        }
    }

    pub fn stop_listening(&mut self) -> Option<Uuid> {
        match *self {
            PttState::Listening { session_id } => {
                *self = PttState::WaitingResult { session_id };
                Some(session_id)
            }
            _ => None,
        }
    }

    pub fn reset(&mut self) {
        *self = PttState::Idle;
    }
}
