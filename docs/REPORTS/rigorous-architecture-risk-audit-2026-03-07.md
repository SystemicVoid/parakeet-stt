# Rigorous Architecture Risk Audit (2026-03-07)

## Internal Model Used For This Review

The system has two critical runtime invariants:

1. Exactly one authoritative capture/transcription session is active at a time, and only the owning control path should be able to mutate or terminate it.
2. Text injection must behave as a serialized side-effect pipeline (clipboard write -> shortcut emission -> handoff), even under retries, disconnects, and timeouts.

Critical boundaries audited:

1. Python daemon WebSocket/session lifecycle and model invocation boundaries.
2. Rust PTT client hotkey event ingestion, async worker lifecycle, and injection serialization guarantees.
3. Cross-process resilience assumptions under disconnects, stalled subprocesses, and partial failures.

---

## Finding 1

1. Title
Session ownership is not bound to a specific WebSocket, so unrelated disconnects can terminate active capture.
2. Severity: High
3. Confidence: High
4. Why this matters
The daemon allows only one active session globally, but it does not track which WebSocket owns that session. That creates a boundary flaw: connection lifecycle from one client can tear down another client’s active session.
5. Evidence
   - `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py` (`handle_websocket`, lines 132-139) resolves `expected_session_id` from global `self.sessions.active` and calls cleanup on disconnect.
   - No owner binding is recorded at session start (`_handle_start`, lines 168-187) beyond the `session_id` itself.
6. Failure mode
Concrete scenario: client A starts dictation; client B is also connected (or an old reconnecting socket still exists). Client B disconnects first. The disconnect path reads global active session (A’s), then cleans it up, abruptly aborting A’s capture and dropping user speech.
7. Why tests/linters might miss it
Current tests focus heavily on single-client cleanup invariants and interleaving races, but not multi-client ownership conflicts. Static checks cannot infer this distributed ownership invariant.
8. Recommended fix
Track `session_owner` (for example, `id(websocket)` or a per-connection UUID) alongside active session metadata. Only allow disconnect-triggered cleanup when the disconnecting socket matches the owner. If multi-client is intentionally unsupported, enforce that explicitly by rejecting second active control connections with a protocol-level error.
9. Is this a real issue or just a preference?
Real issue. This is a correctness boundary violation under plausible runtime topology, not a style preference.
10. Implementation status (2026-03-08)
Resolved in PR #19.

## Finding 2

1. Title
Concurrent model invocation can occur between live interim decoding and finalization.
2. Severity: High
3. Confidence: High
4. Why this matters
Model objects and underlying CUDA/NeMo decode stacks are often not safe for overlapping calls from different threads without explicit synchronization. Overlap can produce crashes, corrupted outputs, or non-deterministic latency spikes.
5. Evidence
   - `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py` (`_emit_live_interim_from_chunk`, lines 647-674) runs `self.transcriber.transcribe_samples` via `run_in_executor` with no shared inference lock.
   - `server.py` (`_finalise_transcription`, lines 852-870) also runs `self.transcriber.transcribe_samples` in executor.
   - `server.py` (`_stop_stream_drain_loop`, lines 893-900) cancels drain task but does not await task completion, so in-flight executor work may continue while finalization starts.
6. Failure mode
Concrete scenario: user releases hotkey while live interim decode is executing in thread pool. Stop path cancels drain loop and starts final decode immediately. Two thread-pool jobs hit the same transcriber/model concurrently; under load this can yield sporadic CUDA errors, transient empty transcripts, or rare deadlocks.
7. Why tests/linters might miss it
This requires tight timing and scheduler overlap. Unit tests typically run deterministically on mocks and won’t force executor races with real NeMo/CUDA runtime behavior.
8. Recommended fix
Introduce a single inference gate (for example, an `asyncio.Lock` used by *all* transcribe calls, including live interim). On stop, await drain task completion after cancellation (`await task` with `CancelledError` handling) before starting final decode.
9. Is this a real issue or just a preference?
Real issue. This is a concurrency safety gap around a shared heavy runtime object.

## Finding 3

1. Title
Injector execution timeout reports cancellation but does not guarantee blocking work actually stops.
2. Severity: High
3. Confidence: High
4. Why this matters
The injector worker is designed as a serialized side-effect channel. If timed-out jobs keep running in background threads, serialization is broken and side effects (clipboard/chords) can overlap out of order.
5. Evidence
   - `parakeet-ptt/src/main.rs` (`spawn_injector_worker_with_capacity`, lines 656-679): injection runs in `spawn_blocking`, timeout is applied, then `abort()` is called on timeout.
   - `spawn_blocking` work is not cooperatively cancellable once executing; aborting join handle does not reliably stop OS-level blocking operations.
6. Failure mode
Concrete scenario: first injection call stalls (for example, backend hangs near process timeout boundary). Worker times out and proceeds to next job, but first blocking task still mutates clipboard/chord state. Result: second transcript can be pasted first, duplicated, or clobbered by late first job completion.
7. Why tests/linters might miss it
Existing tests validate that the queue “recovers” after timeout, but do not verify strict side-effect exclusivity across timeouts. Linters cannot reason about cancellation semantics of blocking threads.
8. Recommended fix
Make serialization explicit even on timeout: do not dequeue the next job until the previous blocking task is known terminated. If hard timeout is required, move injection to killable subprocess boundaries per job (or a dedicated worker process) so timeout can enforce real termination.
9. Is this a real issue or just a preference?
Real issue. This is a lifecycle/cancellation semantic mismatch that can reorder user-visible side effects.

## Finding 4

1. Title
Session audio buffering is unbounded, enabling memory exhaustion under stuck or malicious sessions.
2. Severity: High
3. Confidence: High
4. Why this matters
A long-running or never-stopped session accumulates every audio chunk in memory. A missed key-up, broken client, or malformed control flow can eventually OOM the daemon.
5. Evidence
   - `parakeet-stt-daemon/src/parakeet_stt_daemon/audio.py` (`_callback`, lines 167-169) appends every chunk to `_session_chunks` while active.
   - `audio.py` has no max session chunk/sample cap.
   - `parakeet-stt-daemon/src/parakeet_stt_daemon/session.py` provides lifecycle state but no duration/sample limit enforcement.
6. Failure mode
Concrete scenario: hotkey up event is lost after device hiccup while session remains active. Daemon keeps buffering mic audio for minutes/hours. Memory grows until OOM killer terminates process, causing service outage and dropped dictation.
7. Why tests/linters might miss it
Unit tests use short, controlled lifetimes and do not run long-duration soak scenarios. Type/lint checks cannot detect resource growth over time.
8. Recommended fix
Add server-side hard limits (`max_session_seconds` and/or `max_session_samples`) with automatic abort + explicit error message. For longer captures, spill to bounded ring/temporary file rather than unbounded in-memory vectors.
9. Is this a real issue or just a preference?
Real issue. This is a production robustness and resource-safety defect.

## Finding 5

1. Title
Hotkey device listeners fail closed and are not self-healing after device errors.
2. Severity: Medium
3. Confidence: High
4. Why this matters
Input-device failures are normal on Linux desktops (suspend/resume, USB re-enumeration, transient evdev errors). If listeners exit permanently, push-to-talk silently dies until manual restart.
5. Evidence
   - `parakeet-ptt/src/hotkey.rs` (`spawn_hotkey_loop`, lines 220-224) logs `hotkey device error` and `break`s from the device loop.
   - No device reopen/rescan supervision exists in the same module.
6. Failure mode
Concrete scenario: keyboard device path resets after resume. `fetch_events()` errors once; listener thread exits. App remains connected to daemon and appears healthy, but hotkey presses do nothing for the rest of the session.
7. Why tests/linters might miss it
Tests cover parser/shared-state logic, not device churn over long-running process lifetimes. Static checks do not model hardware lifecycle behavior.
8. Recommended fix
Replace fail-closed break with a supervised reopen loop: on error, backoff, reopen the same path, and periodically rescan `/dev/input/event*` for replacement devices. Emit health metrics/counters when listener count drops.
9. Is this a real issue or just a preference?
Real issue. This is an availability and operability gap under common runtime conditions.

---

## Incident Correlation Note (Alt -> LLM Not Triggering)

1. Observed behavior in provided logs
`/tmp/parakeet-ptt.log` shows multiple sessions where `query modifier engaged; current utterance promoted to llm_query` appears, followed by `final result received in llm_query mode` and `llm response completed` (for example sessions `73363d67...`, `754b0553...`, `9c202d06...`).
2. Contrasting failing pattern in same log window
Later sessions show only `start_session ... intent=Dictate` and plain `final result received` with immediate injection, with no `query modifier engaged` and no `final result received in llm_query mode` (for example `bee11f26...`, `4847d70a...`, `956402ec...`, `84c48939...`).
3. Why this matters for prioritization
This isolates the failure boundary to **client-side modifier-intent capture/promotion**, not daemon transcription, not injection backend, and not LLM residency/health.
4. Connection to findings
This maps directly to **Finding 5** (hotkey/input lifecycle fragility) and elevates it from a theoretical availability risk to a currently observed user-impacting defect class.
5. Priority update
Treat the hotkey/modifier path as an immediate reliability fix target: add self-healing input listener recovery, plus explicit modifier-event observability counters/logging so dropped modifier transitions are diagnosable in production.

## Implementation Status Update (2026-03-07, Hotkey Reliability Pass)

1. Scope preserved for traceability
The incident evidence above remains the original trigger signal, including historical `query modifier engaged` logs, so future readers can map the observed production behavior to this audit.
2. Canonical trigger architecture now implemented
Runtime intent selection is now a single deterministic path: **Shift (any) held before RightCtrl** selects `llm_query`, and intent is snapshotted on hotkey-down for the full utterance.
3. Simplification decision
Mid-utterance promotion behavior was intentionally removed. This trades flexibility for reliability and avoids ambiguous modifier-transition races during active capture.
4. Reliability boundary addressed
The shipped change targets **client hotkey/input lifecycle robustness** (listener supervision, re-enumeration, and clearer diagnostics), directly addressing the boundary identified in Finding 5.
5. Not an LLM availability fix
This pass does **not** alter LLM model/server readiness semantics. If an utterance is captured with `Dictate` intent, the LLM path is intentionally bypassed regardless of LLM health.
6. Operational interpretation change
After this change, absence of LLM routing for a given utterance should be interpreted first as an intent-capture outcome (pre-modifier not active at talk-down) before suspecting LLM runtime outages.
7. Terminology migration completed
Operator-facing surfaces use `llm_pre_modifier` naming (default `KEY_SHIFT`) instead of `query_modifier`, aligning CLI/helper UX with the canonical trigger model.

## Implementation Status Update (2026-03-08, Daemon Session Guardrails)

1. Finding 4 now mitigated in daemon runtime
Session buffering now has hard server-side guardrails: `max_session_seconds` and `max_session_samples`, with auto-abort when either boundary is exceeded.
2. Capture-layer memory boundary is enforced, not advisory
The audio callback path now clips/halts accumulation at the configured sample cap, so memory cannot grow unbounded even if stop/abort control messages are lost.
3. Server watchdog closes the loop for stuck control paths
The daemon now runs a per-session guard task that monitors active session age/limit state and emits explicit session termination errors when limits trigger.
4. Operational surfacing completed
Session guardrail settings are now wired through CLI/env and startup diagnostics so operators can tune limits and verify effective values.
5. Finding 1 tracking updated
Finding 1 is now tracked as resolved in PR #19.

## Open High-Risk Items (Post 2026-03-08)

1. Concurrent transcriber/model use across executor paths can yield non-deterministic failures (Finding 2).
2. Injector timeout semantics can violate serialized side effects (Finding 3).

---

## Historical Ranked Top-5 Risk List (Pre-2026-03-08)

1. Unbounded session audio accumulation can OOM the daemon (Finding 4).
2. Concurrent transcriber/model use across executor paths can yield non-deterministic failures (Finding 2).
3. Injector timeout semantics can violate serialized side effects (Finding 3).
4. WebSocket disconnect ownership ambiguity can abort the wrong active session (Finding 1).
5. Hotkey listeners are not self-healing after device churn (Finding 5).

## Likely AI-Generated Anti-Patterns

1. Fail-open/fail-soft behavior without preserving invariants (timeouts and fallback paths that keep progressing while hidden work may still run).
2. Global state with implicit ownership (active session tracked globally, but no explicit owner boundary).
3. “Best effort” cancellation that reports success semantics stronger than actual runtime guarantees.
4. Silent resource growth assumptions (in-memory accumulation trusted to external control flow correctness).
5. Runtime degradation paths that log warnings but do not enforce safe operational contracts.

## Things That Are Unusual But Acceptable

1. Overlay event transport is intentionally non-fatal and lossy; this is acceptable because overlay is explicitly presentation-only and not the source of truth.
2. Offline-seal finalization as authoritative output while streaming is used mainly for interim UX is a defensible quality-over-latency tradeoff.
3. Copy-only fallback on paste backend failure is operationally reasonable because it preserves transcript delivery even when injection backend is degraded.

## What I Would Inspect Next

1. End-to-end stress test with synthetic hotkey jitter/disconnects to validate daemon/client session invariants under reconnect storms.
2. NeMo/CUDA thread-safety assumptions in real hardware runs with forced overlap to confirm race manifestation frequency.
3. Long-duration soak tests for daemon memory profile under intentionally stuck sessions and partial control-plane failure.
4. Injection correctness under timeout/retry pressure with deterministic clipboard probes and ordering assertions.
5. Security trust boundary hardening for local WebSocket control (auth defaults, optional Unix socket, and privilege separation posture).
