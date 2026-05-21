# Daemon/Client Wire Protocol

The canonical WebSocket contract lives in `docs/protocol/schema/messages.schema.json`.
Canonical wire examples live in `docs/protocol/fixtures/`.

The schema is the source of truth for known daemon/client message fields. The Python
daemon and Rust client keep their hand-written codec APIs, but CI round-trips every
fixture through both implementations so schema drift is visible.

`StatusMessage` is also the protocol-level Runtime Truth contract for the
Daemon `/status` payload. Its `x-runtime-truth-field-groups` metadata names the
current contract groups used by Daemon tests and operator docs; do not create a
second status schema in code, Helper diagnostics, or docs.

## Adding A Field

1. Add the field to `docs/protocol/schema/messages.schema.json`.
2. Update both codecs:
   - Python: `parakeet-stt-daemon/src/parakeet_stt_daemon/messages.py`
   - Rust: `parakeet-ptt/src/protocol.rs`
3. If the field belongs to `StatusMessage`, add it to the Runtime Truth field
   groups in the schema and Python message model.
4. Add or update a fixture in `docs/protocol/fixtures/` that carries the field.
5. Run the conformance checks:
   - `cd parakeet-stt-daemon && uv run pytest -q tests/test_protocol_conformance.py`
   - `cd parakeet-ptt && cargo test protocol_conformance`

## Compatibility

Known messages may carry unknown fields. Receivers ignore those fields so a newer
sender can add data without breaking an older receiver.

Clients consuming daemon streams must treat unknown server message types as non-fatal
and continue decoding later known messages. Unsupported client command types are still
invalid requests because the daemon has no defined action for them.

Fixtures intentionally contain only known fields. That keeps round-trip equality strict:
if a field is added to the schema and fixture but one codec does not preserve it, the
conformance test fails.
