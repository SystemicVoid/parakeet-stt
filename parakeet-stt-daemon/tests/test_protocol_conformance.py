from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest
from jsonschema import Draft202012Validator, FormatChecker
from pydantic import BaseModel

from parakeet_stt_daemon.messages import (
    AbortSession,
    AudioLevelMessage,
    ErrorMessage,
    FinalResult,
    InterimStateMessage,
    InterimTextMessage,
    SessionEndedMessage,
    SessionStarted,
    SessionWarningMessage,
    StartSession,
    StatusMessage,
    StopSession,
    parse_client_message,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
SCHEMA_PATH = REPO_ROOT / "docs/protocol/schema/messages.schema.json"
FIXTURE_DIR = REPO_ROOT / "docs/protocol/fixtures"

CLIENT_MODELS: dict[str, type[BaseModel]] = {
    "start_session": StartSession,
    "stop_session": StopSession,
    "abort_session": AbortSession,
}
SERVER_MODELS: dict[str, type[BaseModel]] = {
    "session_started": SessionStarted,
    "final_result": FinalResult,
    "error": ErrorMessage,
    "status": StatusMessage,
    "interim_state": InterimStateMessage,
    "interim_text": InterimTextMessage,
    "audio_level": AudioLevelMessage,
    "session_ended": SessionEndedMessage,
    "session_warning": SessionWarningMessage,
}
MODEL_BY_TYPE = CLIENT_MODELS | SERVER_MODELS
SCHEMA_DEF_BY_TYPE = {message_type: model.__name__ for message_type, model in MODEL_BY_TYPE.items()}
EXPECTED_FIXTURES = frozenset(
    {
        "client_abort_session.json",
        "client_start_session.json",
        "client_stop_session.json",
        "server_audio_level.json",
        "server_error.json",
        "server_final_result.json",
        "server_interim_state.json",
        "server_interim_text.json",
        "server_session_ended.json",
        "server_session_started.json",
        "server_session_warning.json",
        "status_default.json",
        "status_offline.json",
        "status_stream_fallback.json",
        "status_vad_fallback.json",
    }
)


def _load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def _fixture_paths() -> list[Path]:
    return sorted(FIXTURE_DIR.glob("*.json"))


@pytest.fixture(scope="module")
def schema() -> dict[str, Any]:
    loaded = _load_json(SCHEMA_PATH)
    Draft202012Validator.check_schema(loaded)
    return loaded


@pytest.fixture(scope="module")
def validator(schema: dict[str, Any]) -> Any:
    return Draft202012Validator(schema, format_checker=FormatChecker())


def _round_trip_fixture(fixture: dict[str, Any]) -> dict[str, Any]:
    message_type = fixture["type"]
    if message_type in CLIENT_MODELS:
        return parse_client_message(fixture).model.model_dump(mode="json", exclude_none=False)
    return (
        SERVER_MODELS[message_type]
        .model_validate(fixture)
        .model_dump(mode="json", exclude_none=False)
    )


def _iter_validation_errors(error: Any) -> list[Any]:
    return [
        error,
        *(nested for context in error.context for nested in _iter_validation_errors(context)),
    ]


def test_fixture_inventory_covers_current_wire_messages() -> None:
    assert {path.name for path in _fixture_paths()} == EXPECTED_FIXTURES


def test_fixtures_validate_against_canonical_schema(validator: Any) -> None:
    for path in _fixture_paths():
        errors = sorted(validator.iter_errors(_load_json(path)), key=lambda error: error.path)
        assert not errors, f"{path.name}: {errors}"


def test_status_schema_rejects_negative_chunk_secs(validator: Any) -> None:
    fixture = _load_json(FIXTURE_DIR / "status_default.json") | {"chunk_secs": -0.1}

    errors = sorted(validator.iter_errors(fixture), key=lambda error: error.path)
    nested_errors = [nested for error in errors for nested in _iter_validation_errors(error)]

    assert any(list(error.path) == ["chunk_secs"] for error in nested_errors)


def test_fixtures_round_trip_through_pydantic_models() -> None:
    for path in _fixture_paths():
        fixture = _load_json(path)
        assert _round_trip_fixture(fixture) == fixture, path.name


def test_schema_properties_match_pydantic_model_fields(schema: dict[str, Any]) -> None:
    defs = schema["$defs"]
    for message_type, model in MODEL_BY_TYPE.items():
        schema_def = defs[SCHEMA_DEF_BY_TYPE[message_type]]
        assert set(schema_def["properties"]) == set(model.model_fields), message_type


def test_known_message_models_tolerate_unknown_fields() -> None:
    for path in _fixture_paths():
        fixture = _load_json(path)
        extended = fixture | {"future_extension": {"ignored": True}}
        assert _round_trip_fixture(extended) == fixture, path.name
