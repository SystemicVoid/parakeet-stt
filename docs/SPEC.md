# Parakeet STT – Canonical Specification (Living Document)

_Last updated: 2026-02-19_

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
  2. Daemon begins streaming mic audio (already owned by the daemon) through Parakeet’s RNNT streaming API.
  3. User releases Right Ctrl → `parakeet-ptt` issues `stop_session`.
  4. Daemon finalizes decoding, returns final transcription via WebSocket.
  5. `parakeet-ptt` writes transcript text to the clipboard and executes configured paste behavior (`paste`, `type`, or `copy-only`), with adaptive shortcut routing available in paste mode.

- **Networking**: localhost WebSocket (JSON frames). No audio leaves the daemon process; control messages only.

---

## 3. Component Specifications

### 3.1 `parakeet-stt-daemon` (Python 3.11 via `uv`)

- **Process management**
  - Managed with `uv` (`uv sync`, `uv lock` committed).
  - Eventually shipped as a user-level systemd service for auto-start.

- **Dependencies**
  - `nemo_toolkit[asr]`, `torch==2.x` (cu121 build for Pop!\_OS 24.04), `sounddevice`, `numpy`, `websockets` (or `fastapi[all]` + `uvicorn`), `pydantic` for configs, `loguru` (optional).
  - Keep versions pinned in `pyproject.toml` and lock via `uv`.

- **Audio capture**
  - Sample rate 16 kHz mono, 16‑bit PCM.
  - Use `sounddevice.RawInputStream` with callback writing into a lock-free ring buffer (~2 s). Stream remains open; push-to-talk simply toggles whether frames are fed into the model.
  - Optional: capture 200 ms pre-roll on session start.
  - Streaming defaults (HF-aligned): chunk 2.0 s, right context 2.0 s, left context 10 s, batch size 32; falling back to offline transcription is acceptable when streaming helper is unavailable.

- **Streaming inference**
  - Load `FastConformerTransducerModel` from NeMo with `model.enable_streaming()` (or equivalent) per HF config (`att_context_size=[256,256]`, chunk length 320 samples, stride 160).
  - On session start: reset RNNT states (`model.reset_states()`).
  - Feed 20 ms frames continuously. If eventual partial results are desired, expose them via `partial_result`.
  - On session stop: flush remaining frames, finalize RNNT decoding, normalize punctuation (model already auto-punctuates), package text.
  - Warm-up pass executed at daemon startup to eliminate first-use latency.

- **API server**
  - WebSocket endpoint on `127.0.0.1:8765`.
  - Accepts JSON control messages (spec below). Gracefully handles one active session; return error if new session requested while another is live.
  - Only local connections allowed; optional shared secret environment variable for defense in depth.
  - `/status` HTTP endpoint (optional) exposing GPU memory usage, average latency, current state.

- **Configuration**
  - TOML/YAML file or env vars for: preferred language hint, microphone device ID, WebSocket port, GPU device index.
  - Provide CLI `uv run python -m parakeet_stt_daemon --check` for diagnostics (lists microphones, verifies GPU, runs 1‑second test).

- **Observability**
  - Structured logs for each session: durations, frames processed, GPU ms, output length.
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
  - `evdev`/`uinput` stack plus subprocess backends (`ydotool`, `wtype`) for paste chord emission.

- **Hotkey handling**
  - Identify Right Ctrl (`KEY_RIGHTCTRL`, code 97). Use non-blocking event loop to avoid missed releases.
  - Debounce auto-repeat (ignore events when key state unchanged).
  - Provide override configuration for alternate hotkey if needed later.

- **Injection pipeline**
  - Default runtime path is clipboard choreography (`wl-copy` + readiness probe) and paste chord emission.
  - Paste backend ladder (helper default): `auto` => `uinput -> ydotool -> wtype`.
  - Adaptive routing chooses shortcut by focused-surface class (`terminal`, `general`, `unknown`).
  - Focus metadata source is configurable (`atspi|wayland|hybrid`); default remains `atspi` until matrix validation promotes hybrid.
  - Low-confidence AT-SPI snapshots (`focus_focused=false`) are treated as `unknown` for routing.
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
    { "type": "abort_session", "session_id": "<uuid>", "reason": "timeout|user|error" }
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
      "code": "SESSION_BUSY|AUDIO_DEVICE|MODEL",
      "message": "<human friendly>"
    }
    ```
  - `status`
    ```json
    {
      "type": "status",
      "state": "idle|listening|processing",
      "sessions_active": 0,
      "gpu_mem_mb": 1320
    }
    ```

Future messages (like `partial_result`) must be backward compatible; clients should ignore unknown `type`s.

---

## 5. Implementation Guidelines

1. **Use uv & cargo consistently**
   - Python: `uv run`, `uv pip`, `uv lock`. No `pip install` without uv.
   - Rust: standard `cargo` commands; consider workspace layout early.

2. **Coding standards**
   - Python: type hints, `ruff` + `black` for lint/format (invoked via `uv run`).
   - Rust: `cargo fmt`, `cargo clippy --all-targets`.

3. **Security & permissions**
   - Limit WebSocket server to localhost; optionally require auth token.
   - Ensure `parakeet-ptt` only opens necessary `/dev/input` descriptors and handles permission failures gracefully.

4. **Performance**
   - Keep model on GPU at all times.
   - Avoid copying audio data twice; use memoryview or `numpy.frombuffer` around the ring buffer.
   - Measure and log latency on every session to detect regressions.

5. **Testing**
   - Python: unit tests for streaming state machine, mocked audio input, and WebSocket handlers.
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
   - Integrate `wtype` fallback; type transcriptions into focused window.
   - Add configurables (delay, trimming).

4. **M3 – Hardening**
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

---

## 8. Repo Operations

- Remote `SystemicVoid/parakeet-stt` exists and is set to **private** (verified via `gh repo view`).
- To recreate/replace the remote via CLI while keeping it private:
  - `gh repo create SystemicVoid/parakeet-stt --private --source . --remote origin --push`
- Sanity-check visibility any time with: `gh repo view --json isPrivate,visibility`.

---

Keep this spec updated as implementation evolves. Every major change (new protocol fields, dependency shifts, UX decisions) should be reflected here promptly so future agents can onboard quickly.
