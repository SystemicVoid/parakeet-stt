# Parakeet STT Daemon

Early skeleton for the local Parakeet push-to-talk daemon. It exposes the
JSON/WebSocket protocol defined in `SPEC.md` and keeps the audio/model plumbing
pluggable for the next iteration.

## Quickstart

```bash
uv sync --all-extras --dev
uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765
```

- Uses environment variables prefixed with `PARAKEET_` for config overrides
  (e.g. `PARAKEET_SHARED_SECRET`, `PARAKEET_MIC_DEVICE`).
- Optional `/status` endpoint is enabled by default (disable with `--no-status`).
- Current implementation stubs the transcription path until the NeMo pipeline is
  wired in; it still enforces single-session semantics and returns structured
  `session_started`/`final_result`/`error` messages.

## Notes

- The `inference` extra group pins `torch`, `torchaudio`, and
  `nemo_toolkit[asr]`; skip it if you only need protocol testing.
- Package uses a `src/` layout with Hatch build backend.
