from __future__ import annotations

from uuid import uuid4

import pytest
from parakeet_stt_daemon.messages import (
    InterimStateMessage,
    InterimStateValue,
    InterimTextMessage,
)
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
