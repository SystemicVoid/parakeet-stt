# Parakeet STT – Canonical Specification (Living Document)

_Last updated: 2026-02-23_

This document is the single source of truth for the local, push-to-talk Parakeet speech-to-text solution on Pop!\_OS 24.04 (Wayland). Update it whenever significant design, implementation, or operational decisions are made so every agent and developer can stay in sync.

---

## 1. Goals & Non-Goals

- **Goals**
  - Local-only, GPU-accelerated speech-to-text using NVIDIA Parakeet-TDT 0.6B v3 with NeMo streaming.
  - Push-to-talk UX: Right Ctrl press starts listening; release stops and injected text appears in the focused field.
  - Minimal UI (no overlay for v1); headless background services only.
  - Keep the pipeline simple yet performant (KISS) with clear separation between ML responsibilities (Python) and OS-facing UX/integration (Rust).
  - Fast end-to-end latency (<150 ms from key release to text injection for typical utterances).
  - Privacy-first: all processing local; no cloud dependencies.
  - Runtime truth must be explicit: status/log signals must reflect effective device and streaming activation, not just configured intent.
  - Session lifecycle invariants are mandatory: disconnect/error paths must not leave orphaned active capture state.

- **Non-Goals (v1)**
  - On-screen transcription overlays or editing UI.
  - Wake-word activation, toggle modes, or conversation history.
  - Multi-user concurrency (single desktop user only).
  - Language model post-processing (LLMs). Can be future enhancement.

---

## 2. High-Level Architecture

```
┌─────────────────────────┐        ┌──────────────────────────────┐
│ parakeet-ptt (Rust)     │        │ parakeet-stt-daemon (Python) │
│ - evdev RightCtrl watch │        │ - Mic capture + pre-buffer   │
│ - WebSocket client      ├──────► │ - Parakeet streaming NeMo    │
│ - Text injection        │ ◄──────┤ - Session state machine      │
│ - Config/logging        │        │ - WebSocket server (localhost)│
└─────────────────────────┘        └──────────────────────────────┘
                                         │
                                         ▼
                                 RTX 5060 Ti (CUDA)
```

- **Control flow**
  1. User presses Right Ctrl → `parakeet-ptt` sends `start_session` to daemon.
  2. Daemon begins session capture and, when streaming engine activation succeeds, performs incremental RNNT decode.
  3. User releases Right Ctrl → `parakeet-ptt` issues `stop_session`.
  4. Daemon finalizes decoding (streaming finalization if active, otherwise explicit offline fallback), returns final transcription via WebSocket.
  5. `parakeet-ptt` writes transcript text to the clipboard and executes configured injection behavior (`paste` or `copy-only`), with adaptive shortcut routing available in paste mode.

- **Networking**: localhost WebSocket (JSON frames). No audio leaves the daemon process; control messages only.

---

## 3. Component Specifications

### 3.1 `parakeet-stt-daemon` (Python 3.11 via `uv`)

- **Process management**
  - Managed with `uv` (`uv sync`, `uv lock` committed).
  - Eventually shipped as a user-level systemd service for auto-start.

- **Dependencies**
  - Core daemon/runtime: `fastapi`, `uvicorn`, `pydantic`, `sounddevice`, `numpy`, `loguru`.
  - Inference/runtime lane: `torch` + `nemo-toolkit[asr]` pinned and resolved in `uv.lock` with an explicit CUDA wheel index strategy.
  - Optional performance lane: `cuda-python` for CUDA-graphs optimization, feature-gated and observable at runtime.
  - Keep versions pinned in `pyproject.toml`, lock with `uv lock`, and deploy with `uv sync --frozen`.

- **Audio capture**
  - Sample rate 16 kHz mono, 16‑bit PCM.
  - Use `sounddevice.InputStream` callback writing into a rolling pre-roll buffer (currently 2.5 s) plus per-session accumulation.
  - Input stream remains open; push-to-talk toggles whether callback frames are attached to the active session.
  - Streaming defaults: chunk 2.0 s, right context 2.0 s, left context 10 s, batch size 32.
  - Offline fallback remains allowed only when explicitly signaled in startup logs and `/status`.

- **Streaming inference**
  - Target documented NeMo streaming APIs for RNNT/conformer flows (cache-aware per-session stepping), not brittle internal helper imports.
  - On session start: allocate streaming state transactionally; if any downstream setup fails, rollback to idle.
  - Feed chunked frames continuously while the session is active; preserve room for future partials without requiring protocol churn in v1.
  - For `FrameBatchChunkedRNNT` finalization, build `AudioFeatureIterator(..., pad_to_frame_len=False)` to avoid synthetic padded tail frames that can perturb utterance-end decoding.
  - On session stop: flush remaining frames and finalize using streaming state when active; keep explicit offline finalization as guarded fallback.
  - Warm-up pass executed at daemon startup to eliminate first-use latency.

- **API server**
  - WebSocket endpoint on `127.0.0.1:8765`.
  - Accepts JSON control messages (spec below). Supports one active session and enforces cleanup invariants across disconnect/error paths.
  - Only local connections allowed; optional shared secret environment variable for defense in depth.
  - `/status` HTTP endpoint (optional) exposes state and runtime truth (configured/effective device, streaming activation state, and optional diagnostic fields).

- **Configuration**
  - Env vars + CLI flags for preferred language hint, microphone device ID, WebSocket host/port, and device selection.
  - Precedence contract: **CLI explicit > ENV > defaults**.
  - Provide CLI `uv run python -m parakeet_stt_daemon --check` for diagnostics (lists microphones, verifies GPU, runs 1‑second test).

- **Observability**
  - Structured logs for each session: durations, frames processed, infer ms, output length, and failure stage attribution.
  - Startup logs must include streaming truth signals (`requested`, `active/fallback`, fallback reason) and effective runtime device.
  - Metrics aggregator (future) to feed UI/waybar when needed.
  - Quick smoke (from any directory):
    - `repo=$HOME/Documents/Engineering/parakeet-stt`
    - `(cd "$repo/parakeet-stt-daemon" && uv run parakeet-stt-daemon --check)`
    - `(cd "$repo/parakeet-stt-daemon" && uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765)`  # requires inference extra
    - In another shell: `(cd "$repo/parakeet-ptt" && cargo run --release)` (or `cargo run --manifest-path "$repo/parakeet-ptt/Cargo.toml" --release`)
    - Watch logs for `start_session`/`final_result`.

### 3.2 `parakeet-ptt` (Rust 1.89+)

- **Responsibilities**
  - Monitor Right Ctrl via `evdev` (requires user in `input` group or setcap helper).
  - Maintain simple state machine (Idle → Listening → WaitingResult → Idle).
  - Communicate over WebSocket; no audio capture in Rust for v1.
  - Inject resulting text using Wayland-friendly methods.

- **Crates**
  - `tokio`, `tokio-tungstenite`, `serde`/`serde_json`, `evdev`, `anyhow`, `uuid`.
  - `evdev`/`uinput` stack plus `ydotool` subprocess fallback for paste chord emission.

- **Hotkey handling**
  - Identify Right Ctrl (`KEY_RIGHTCTRL`, code 97). Use non-blocking event loop to avoid missed releases.
  - Debounce auto-repeat (ignore events when key state unchanged).
  - Provide override configuration for alternate hotkey if needed later.

- **Injection pipeline**
  - Default runtime path is clipboard choreography (`wl-copy` + readiness probe) and paste chord emission.
  - Injection execution is serialized through a dedicated bounded worker queue (`capacity=32`) so websocket/hotkey async paths do not run blocking clipboard/chord calls inline.
  - Paste backend ladder (helper default): `auto` => `uinput -> ydotool`.
  - Adaptive routing chooses shortcut by focused-surface class (`terminal`, `general`, `unknown`).
  - Focus metadata source is Wayland toplevel cache observations (with low-confidence handling on transition/staleness paths).
  - Low-confidence focus snapshots (`focus_focused=false`) are treated as `unknown` for routing.
  - Stage-attributed observability is emitted for `clipboard_ready`, `route_shortcut`, and `backend` with per-stage durations and totals.
  - Backend failure policy defaults to `copy-only` so transcript delivery is preserved via clipboard.

- **Resilience**
  - If daemon connection fails, show notification/log and retry with exponential backoff.
  - Optional system tray/waybar integration reserved for future (flag-protected).

---

## 4. WebSocket Protocol (v1)

All messages are JSON objects with a `type` string.

- **Client → Server**
  - `start_session`
    ```json
    {
      "type": "start_session",
      "session_id": "<uuid>",
      "mode": "push_to_talk",
      "preferred_lang": "auto|null",
      "timestamp": "<iso8601>"
    }
    ```
  - `stop_session`
    ```json
    { "type": "stop_session", "session_id": "<uuid>", "timestamp": "<iso8601>" }
    ```
  - `abort_session`
    ```json
    {
      "type": "abort_session",
      "session_id": "<uuid>",
      "reason": "timeout|user|error",
      "timestamp": "<iso8601>"
    }
    ```

- **Server → Client**
  - `session_started`
    ```json
    {
      "type": "session_started",
      "session_id": "<uuid>",
      "ts": "<iso8601>",
      "mic_device": "<name>",
      "lang": "auto"
    }
    ```
  - `final_result`
    ```json
    {
      "type": "final_result",
      "session_id": "<uuid>",
      "text": "<transcription>",
      "latency_ms": 120,
      "audio_ms": 2300,
      "lang": "en",
      "confidence": 0.91
    }
    ```
  - `error`
    ```json
    {
      "type": "error",
      "session_id": "<uuid>",
      "code": "SESSION_BUSY|SESSION_NOT_FOUND|SESSION_ABORTED|AUDIO_DEVICE|MODEL|INVALID_REQUEST|UNEXPECTED",
      "message": "<human friendly>"
    }
    ```
  - `status`
    ```json
    {
      "type": "status",
      "state": "idle|listening|processing",
      "sessions_active": 0,
      "gpu_mem_mb": 1320,
      "device": "cuda",
      "effective_device": "cuda",
      "streaming_enabled": true,
      "stream_helper_active": true,
      "stream_fallback_reason": null,
      "active_session_age_ms": 0,
      "audio_stop_ms": 12,
      "finalize_ms": 180,
      "infer_ms": 120,
      "send_ms": 5,
      "last_audio_ms": 2300,
      "last_infer_ms": 120,
      "last_send_ms": 5
    }
    ```

Future messages (like `partial_result`) must be backward compatible; clients should ignore unknown `type`s.
Clients must also tolerate unknown error codes and unknown additional fields.
Fields beyond `state` and `sessions_active` in `status` should be treated as optional.
`audio_stop_ms`, `finalize_ms`, `infer_ms`, and `send_ms` represent the last completed session's stage
durations. `last_*` fields are retained for compatibility and will be deprecated after client uptake.
`gpu_mem_mb` reports CUDA reserved memory for the daemon process when running on a CUDA device.

---

## 5. Implementation Guidelines

1. **Use uv & cargo consistently**
   - Python: `uv run`, `uv sync`, `uv add`, `uv lock`. No direct `pip install` and no `uv pip install`.
   - Rust: standard `cargo` commands; consider workspace layout early.

2. **Coding standards**
   - Python: type hints, `ruff` for lint/format (invoked via `uv run`).
   - Rust: `cargo fmt`, `cargo clippy --all-targets`.

3. **Security & permissions**
   - Limit WebSocket server to localhost; optionally require auth token.
   - Ensure `parakeet-ptt` only opens necessary `/dev/input` descriptors and handles permission failures gracefully.

4. **Performance**
   - Keep model on GPU at all times.
   - Avoid copying audio data twice; use memoryview or `numpy.frombuffer` around the ring buffer.
   - Measure and log latency on every session to detect regressions.

5. **Testing**
   - Python: unit tests for session lifecycle invariants (disconnect/error cleanup + transactional start), streaming state machine, mocked audio input, and WebSocket handlers.
   - Include regression tests for chunked streaming finalize behavior at utterance boundaries (no padded synthetic frame insertion).
   - Integration: CLI script feeding a short WAV file to ensure deterministic output.
   - Rust: mock daemon responses to verify state transitions and injection calls; property-based tests for key event handling.

6. **Deployment**
   - Provide systemd user units: `parakeet-stt-daemon.service` (runs via `uv run`), `parakeet-ptt.service` (runs `cargo` binary).
   - Document required groups (`input`, maybe `audio`).
   - Offer a `setup.sh` (later) that installs dependencies, builds binaries, enables services.

---

## 6. Backlog & Milestones

1. **M0 – Bootstrap**
   - Initialize Python project with uv; verify CUDA + Parakeet streaming inference using sample WAV.
   - Create Rust crate skeleton; connect to dummy WebSocket server.

2. **M1 – Push-to-talk MVP**
   - Implement daemon ring buffer + streaming inference.
   - Implement Rust hotkey detection and WebSocket control.
   - Return transcription as log (no injection yet).

3. **M2 – Text Injection**
   - Harden clipboard + adaptive routing path across target app classes.
   - Keep `uinput -> ydotool` fallback behavior observable and deterministic.

4. **M3 – Hardening**
   - Daemon hardening gate: session cleanup invariants, start rollback semantics, config precedence tests, runtime truth status fields.
   - Error handling, reconnection logic, systemd units, metrics endpoint.
   - Permission setup script/instructions.

5. **M4 – Enhancements (future)**
   - Virtual-keyboard implementation.
   - Optional overlay with live partials.
   - CLI for offline file transcription via same daemon.
   - Post-processing (local LLM) and macros.

---

## 7. Open Questions / Future Decisions

| Topic | Status / Notes |
| --- | --- |
| Virtual keyboard support in COSMIC | Investigate compositor protocol availability before M3; fallback already defined. |
| Authentication | Decide whether to enforce shared secret or rely on localhost isolation. |
| Multi-language hints | Add config to pin language or rely on auto-detect per user preference. |
| Partial result overlay | Deferred; document design when prioritized. |
| Streaming default policy | Use offline helper default (`stt start` with `PARAKEET_STREAMING_ENABLED=false`) and keep `PARAKEET_STREAMING_ENABLED=true` as an explicit streaming validation path. |
| GPU stack refresh timing | Run staged update lane after streaming API integration is validated on current lock baseline. |

---

## 8. Repo Operations

- Remote `SystemicVoid/parakeet-stt` exists and is set to **private** (verified via `gh repo view`).
- To recreate/replace the remote via CLI while keeping it private:
  - `gh repo create SystemicVoid/parakeet-stt --private --source . --remote origin --push`
- Sanity-check visibility any time with: `gh repo view --json isPrivate,visibility`.

---

Keep this spec updated as implementation evolves. Every major change (new protocol fields, dependency shifts, UX decisions) should be reflected here promptly so future agents can onboard quickly.
