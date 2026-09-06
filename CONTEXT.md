# Parakeet STT

Local, low-latency, push-to-talk speech-to-text for Linux/Wayland. Two cooperating processes:
a Python daemon (NeMo Parakeet ASR) and a Rust client (global hotkey, WebSocket, text injection).

## Language

**Daemon**:
The Python process (`parakeet-stt-daemon`) that owns mic capture, ASR inference, and the WebSocket server.
_Avoid_: server, backend, service.

**Client**:
The Rust process (`parakeet-ptt`) that owns the global hotkey, daemon WebSocket connection, and text injection.
_Avoid_: frontend, ptt-app, agent.

**Helper**:
`scripts/stt-helper.sh` — the canonical shell wrapper exposing the `stt` function (`stt start`, `stt off`, `stt status`, …). Source of truth for start flags, defaults, and env wiring.
_Avoid_: launcher, runner, wrapper script.

**Session**:
One discrete capture bracketed by a push-to-talk press and release. The daemon owns session state; finalization always emits a transcript or an explicit error.
_Avoid_: recording, capture, request, utterance (utterance is an ASR-internal concept).

**Push-to-talk (PTT)**:
The activation model: hold Right Ctrl to capture, release to finalize. The only activation mode in v1.
_Avoid_: hotword, wake word, toggle, hands-free.

**Profile**:
A named bundle of helper defaults that maps to a startup flag set. Current profiles are `stt` / `stt start` (online, stream+seal, overlay on), `stt off` (offline on CUDA, no streaming, overlay off), and `stt cpu` (offline on CPU, no streaming, overlay off).
_Avoid_: mode, preset, config.

**Stream path / Seal path**:
The two inference paths the daemon can run. **Stream** = chunked NeMo streaming during the session; **Seal** = offline finalization at session end. The default profile runs both (stream during, seal at end) and reconciles.
_Avoid_: live/final, partial/final, hot/cold.

**Pre-roll buffer**:
The rolling audio ring (currently 2.5 s) the daemon keeps regardless of session state, so the session can include audio captured just before the press.
_Avoid_: lookback, prefix, jitter buffer.

**Overlay**:
The compact on-screen feedback widget rendered by the client during a session. Optional, on by default in the `stt` profile. Its presentation is the **sheet**: a paper card with an instrument column (the coil level trace, the **lamp** word such as `REC` / `DECODING` / `PASTED`, and the session timer) beside the prose. Interim words arrive as the italic **draft tail** and set to roman ink once the next burst lands; the **seal rule** under the prose fills while the Seal path runs.
_Avoid_: HUD, popup, indicator, panel, toast.

**Injection**:
Writing the transcript into the focused window. Two injection behaviors: `paste` (clipboard + paste shortcut) and `copy-only` (clipboard only).
_Avoid_: typing, output, paste (use only when naming the specific behavior).

**Runtime truth**:
Status/log signals that reflect the effective state (which device, which path actually ran), not just configured intent. Required for `/status`, helper diagnostics, and any logged decision the operator must trust.
_Avoid_: actual state, real status.

## Relationships

- A **Client** opens one WebSocket to the **Daemon**; PTT key events become `start_session` / `stop_session` control frames.
- A **Session** is the unit of work; the **Daemon** owns its state machine and guarantees no orphaned active capture across disconnect or error.
- The default **Profile** runs both **Stream path** and **Seal path** per session; the **Pre-roll buffer** feeds the session start regardless of path.
- The **Helper** is the only sanctioned way to start/stop both processes; ad-hoc `cargo run` / `uv run` invocations exist but are not the operator interface.
- **Injection** happens client-side after the **Daemon** returns the final transcript; **Overlay** is client-only and never gates injection.

## Example dialogue

> **Dev:** "Did the streaming path actually run last session, or did we fall back?"
> **Domain expert:** "Check **runtime truth** in `/status` — it shows the effective inference path, not the configured profile. If the **stream path** failed to allocate, the daemon should have logged the rollback and finalized via the **seal path** only."

## Flagged ambiguities

- "service" was used loosely to mean both the **Daemon** and the systemd unit that supervises it — resolved: **Daemon** is the process; "systemd unit" or "user service" names the supervisor.
- "ptt" was used to mean both the activation model and the Rust binary — resolved: **PTT** (capitalized) for the activation model, **Client** for the Rust process.
