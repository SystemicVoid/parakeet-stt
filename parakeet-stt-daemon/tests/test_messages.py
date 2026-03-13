from __future__ import annotations

from uuid import uuid4

import pytest
from parakeet_stt_daemon.messages import (
    InterimStateMessage,
    InterimStateValue,
    InterimTextMessage,
    SessionEndedMessage,
    SessionEndReason,
    StatusMessage,
)
from parakeet_stt_daemon.session import SessionState
from pydantic import ValidationError


def test_interim_text_requires_text_field() -> None:
    with pytest.raises(ValidationError):
        InterimTextMessage.model_validate(
            {
                "type": "interim_text",
                "session_id": str(uuid4()),
                "seq": 1,
            }
        )


def test_interim_state_rejects_unknown_state_value() -> None:
    with pytest.raises(ValidationError):
        InterimStateMessage.model_validate(
            {
                "type": "interim_state",
                "session_id": str(uuid4()),
                "seq": 2,
                "state": "unknown",
            }
        )


def test_interim_seq_must_be_non_negative() -> None:
    with pytest.raises(ValidationError):
        InterimTextMessage.model_validate(
            {
                "type": "interim_text",
                "session_id": str(uuid4()),
                "seq": -1,
                "text": "hello",
            }
        )


def test_interim_state_accepts_enum_values() -> None:
    session_id = uuid4()

    msg = InterimStateMessage.model_validate(
        {
            "type": "interim_state",
            "session_id": str(session_id),
            "seq": 0,
            "state": "processing",
        }
    )

    assert msg.session_id == session_id
    assert msg.seq == 0
    assert msg.state == InterimStateValue.PROCESSING


def test_status_message_accepts_session_state_enum() -> None:
    msg = StatusMessage(state=SessionState.IDLE, sessions_active=0)
    assert msg.state == SessionState.IDLE
    dumped = msg.model_dump(mode="json")
    assert dumped["state"] == "idle"


def test_status_message_accepts_session_state_string() -> None:
    msg = StatusMessage.model_validate(
        {"type": "status", "state": "listening", "sessions_active": 1}
    )
    assert msg.state == SessionState.LISTENING


def test_status_message_rejects_invalid_state() -> None:
    with pytest.raises(ValidationError):
        StatusMessage.model_validate({"type": "status", "state": "unknown", "sessions_active": 0})


def test_session_ended_accepts_reason_enum() -> None:
    sid = uuid4()
    msg = SessionEndedMessage(session_id=sid, reason=SessionEndReason.FINAL)
    assert msg.reason == SessionEndReason.FINAL
    dumped = msg.model_dump(mode="json")
    assert dumped["reason"] == "final"


def test_session_ended_accepts_none_reason() -> None:
    sid = uuid4()
    msg = SessionEndedMessage(session_id=sid)
    assert msg.reason is None
    dumped = msg.model_dump(mode="json")
    assert dumped["reason"] is None
