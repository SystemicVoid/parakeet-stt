# Rigorous Architecture Risk Audit (2026-03-07)

## Internal Model Used For This Review

The system has two critical runtime invariants:

1. Exactly one authoritative capture/transcription session is active at a time, and only the owning control path should be able to mutate or terminate it.
2. Text injection must behave as a serialized side-effect pipeline (clipboard write -> shortcut emission -> handoff), even under retries, disconnects, and timeouts.

Critical boundaries audited:

1. Python daemon WebSocket/session lifecycle and model invocation boundaries.
2. Rust PTT client hotkey event ingestion, async worker lifecycle, and injection serialization guarantees.
3. Cross-process resilience assumptions under disconnects, stalled subprocesses, and partial failures.

## Post-Merge Reassessment (Commit 5c976fe, merged 2026-03-07)

1. Scope of code changes
The merged release changes the Rust client hotkey path (`parakeet-ptt/src/hotkey.rs`, `parakeet-ptt/src/main.rs`) plus operator/helper surfaces. It does **not** change the daemon session manager, daemon audio buffering, or injector timeout implementation.
2. Finding status summary
   - Findings 1-4 remain materially unchanged by this merge.
   - The original Finding 5 is no longer accurate as written. The merged hotkey implementation now has listener supervision, periodic rescans, synthetic key-up cleanup, and richer diagnostics (`parakeet-ptt/src/hotkey.rs`, lines 371-424 and 555-645).
3. Replacement risk
The hotkey area still has a narrower residual issue: listeners do not reconstruct already-held modifier state when they attach or re-attach, so the first utterance after startup or device recovery can still be misrouted.

## Post-Fix Reassessment (Commit efe286a, 2026-03-07)

1. Finding 1 is now resolved in code
The daemon now binds each active session to the WebSocket that started it, then checks that owner token before stop, abort, disconnect cleanup, and rollback cleanup paths run.
2. Implementation evidence
   - `parakeet-stt-daemon/src/parakeet_stt_daemon/session.py` now stores `owner_token` on `Session` and enforces that token in `start_session`, `stop_session`, and `clear`.
   - `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py` now derives `owner_token = id(websocket)` and requires owner matches in `handle_websocket`, `_handle_start`, `_handle_stop`, `_handle_abort`, and `_cleanup_active_session`.
   - `parakeet-stt-daemon/tests/test_session_cleanup.py` now covers non-owner disconnect, stop, and abort attempts, plus the existing disconnect-during-start interleaving race.
3. Practical effect
This closes the original boundary flaw: one connected client can no longer tear down another client’s active capture just by disconnecting or by replaying the other session ID.

---

## Finding 1

1. Title
Session ownership is now bound to the creating WebSocket; non-owner teardown paths are rejected.
2. Severity: Resolved locally (was High before fix)
3. Confidence: High
4. Why this matters
This was a real correctness bug before the fix. The daemon still allows only one active session globally, but it now records which WebSocket owns that session and refuses teardown from unrelated sockets.
5. Evidence
   - `parakeet-stt-daemon/src/parakeet_stt_daemon/session.py` now records `Session.owner_token` and enforces owner matches in `stop_session` and `clear`.
   - `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py` binds `owner_token = id(websocket)` at connection entry and requires that owner for disconnect cleanup, stop, abort, and start rollback.
   - `parakeet-stt-daemon/tests/test_session_cleanup.py` now proves that non-owner disconnect, stop, and abort paths leave the owner’s active session intact.
6. Current status
Resolved in commit `efe286a` on 2026-03-07. Non-owner sockets now receive `SESSION_NOT_FOUND` for stop/abort attempts, and unrelated disconnects no longer clear the active session.
7. Remaining caveat
The daemon still intentionally runs as a single-active-session service. A second client can connect, but it cannot mutate or terminate a session it does not own.
8. Recommended follow-through
Keep the new owner-binding regressions in the release gate. If multi-client operator UX becomes confusing later, consider an explicit protocol error that says "another controller owns the active session" instead of the current privacy-preserving `SESSION_NOT_FOUND`.
9. Is this a real issue or just a preference?
Real issue, now fixed locally.
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
10. Implementation status (2026-03-08)
Resolved in PR #20.

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
10. Implementation status (2026-03-08)
Resolved locally in commits `5a60dd0` and `55d9b51`. Injection jobs now run behind a killable subprocess boundary, and regression coverage asserts that a timed-out first job cannot leak late side effects after a later job completes.

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
10. Implementation status (2026-03-08)
Mitigated in PR #20.

## Finding 5

1. Title
Hotkey listeners now self-heal after device churn, but intent capture does not reconstruct already-held modifier state on attach/recovery.
2. Severity: Low
3. Confidence: Medium
4. Why this matters
The merge closes the larger availability bug, but the new pre-modifier design still depends on seeing a modifier-down event after a listener attaches. If Shift is already held during startup, resume, or device re-enumeration, the first talk-down can still be classified as `Dictate` instead of `llm_query`.
5. Evidence
   - `parakeet-ptt/src/hotkey.rs` (`run_hotkey_supervisor`, lines 555-645) now supervises listener exits, periodically rescans `/dev/input/event*`, and respawns listeners for recovered devices.
   - `parakeet-ptt/src/hotkey.rs` (`handle_listener_exit`, lines 371-424) releases dead listener state and can emit a synthetic `HotkeyEvent::Up`, so the original fail-closed/stuck-key defect is materially fixed.
   - `parakeet-ptt/src/hotkey.rs` (`hotkey_listener_for_path`, lines 669-708) opens the device and immediately starts consuming future events; it does not seed state from the device's current pressed-key bitmap.
   - `parakeet-ptt/src/hotkey.rs` (`derive_hotkey_event`, lines 800-828) snapshots intent from `state.pre_modifier_active()`, which is only updated from observed modifier transitions.
6. Failure mode
Concrete scenario: the keyboard re-enumerates after resume while the user is already holding Shift. The replacement listener attaches after the Shift-down event already happened, so shared state still says "modifier up." The next RightCtrl press starts a plain dictate session for that first utterance.
7. Why tests/linters might miss it
The new tests cover shared-state cleanup, supervised exits, and post-attach event handling, but they do not model "listener attaches while modifier is already held." Static checks cannot infer this temporal hardware-state edge.
8. Recommended fix
On listener attach and reattach, read the device's current pressed-key state and seed `ListenerPressedState` / `HotkeySharedState` before processing new events. If evdev state cannot be trusted everywhere, add an explicit diagnostic when the first talk-down after attach occurs with no observed modifier history.
9. Is this a real issue or just a preference?
Real issue, but much narrower than the original finding. The availability hole was materially fixed; this is a residual intent-capture edge case.
10. Implementation status (2026-03-08)
Resolved locally pending PR review. Listeners now seed already-held LLM pre-modifier state from the kernel key bitmap at attach time, so the first post-attach utterance no longer depends on observing a fresh modifier-down event.

---

## Incident Correlation Note (Alt -> LLM Not Triggering)

1. Observed behavior in provided logs
`/tmp/parakeet-ptt.log` shows multiple sessions where `query modifier engaged; current utterance promoted to llm_query` appears, followed by `final result received in llm_query mode` and `llm response completed` (for example sessions `73363d67...`, `754b0553...`, `9c202d06...`).
2. Contrasting failing pattern in same log window
Later sessions show only `start_session ... intent=Dictate` and plain `final result received` with immediate injection, with no `query modifier engaged` and no `final result received in llm_query mode` (for example `bee11f26...`, `4847d70a...`, `956402ec...`, `84c48939...`).
3. Why this matters for prioritization
This isolates the failure boundary to **client-side modifier-intent capture/promotion**, not daemon transcription, not injection backend, and not LLM residency/health.
4. Connection to findings
This mapped directly to the **original** Finding 5 and explains why commit `5c976fe` targeted the hotkey/input lifecycle path first.
5. Priority update
That original priority action has now been shipped. The remaining hotkey follow-up is narrower: seed already-held modifier state on listener attach/recovery so the first post-recovery utterance cannot be misclassified.

## Implementation Status Update (2026-03-07, Hotkey Reliability Pass)

1. Scope preserved for traceability
The incident evidence above remains the original trigger signal, including historical `query modifier engaged` logs, so future readers can map the observed production behavior to this audit.
2. Canonical trigger architecture now implemented
Runtime intent selection is now a single deterministic path: **Shift (any) held before RightCtrl** selects `llm_query`, and intent is snapshotted on hotkey-down for the full utterance.
3. Simplification decision
Mid-utterance promotion behavior was intentionally removed. This trades flexibility for reliability and avoids ambiguous modifier-transition races during active capture.
4. Reliability boundary materially addressed
The shipped change closes the original hotkey listener availability gap: listener exits are supervised, device paths are rescanned, dead listeners release key state, and diagnostics now expose listener churn.
5. Not an LLM availability fix
This pass does **not** alter LLM model/server readiness semantics. If an utterance is captured with `Dictate` intent, the LLM path is intentionally bypassed regardless of LLM health.
6. Residual edge still open
The merged implementation does **not** reconstruct modifier state that was already held when a listener attached or re-attached. Operationally, a first utterance after startup/recovery can still be misrouted unless the modifier is re-pressed.
7. Operational interpretation change
After this change, absence of LLM routing for a given utterance should be interpreted first as an intent-capture outcome (pre-modifier not active at talk-down), then as a possible attach-state blind spot if device recovery just occurred, before suspecting LLM runtime outages.
8. Terminology migration completed
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

## Implementation Status Update (2026-03-08, Daemon Inference Serialization)

1. Finding 2 now mitigated in daemon runtime
All transcriber/model entry points now share a single async inference gate, so live interim decode, stop-path interim recomputation, and authoritative final decode cannot overlap on the same runtime object.
2. Cancellation semantics now preserve the safety boundary
When stop cancels the drain task, daemon shutdown now awaits that task and keeps the inference gate held until any already-started executor inference has actually finished, preventing hidden overlap after coroutine cancellation.
3. Finalization does not wait on a backlog of new live work
Stop closes the drain loop first, so key-up can only wait for an already-running interim decode, not for additional streaming jobs queued after release. This bounds the latency tradeoff to the single in-flight decode window.
4. Regression coverage now targets the race directly
Daemon tests now force the interleaving where live interim inference is active during stop and assert both serialized model access and awaited drain shutdown before final send.
5. Finding 2 tracking updated
Finding 2 is now tracked as resolved in PR #20.

## Implementation Status Update (2026-03-08, Injector Timeout Isolation)

1. Finding 3 now resolved locally in client runtime
Injection work no longer relies on `spawn_blocking` cancellation semantics for timeout enforcement. Each queued injection job now executes in a one-shot subprocess boundary that the worker can kill and reap before proceeding.
2. Timeout semantics now match the user-visible contract
If the injector worker reports `execution_timeout`, the timed-out job process has already been terminated and reaped. The queue does not advance on the assumption that background clipboard/chord work probably stopped.
3. FIFO side-effect isolation is preserved across timeout recovery
The next queued job only starts after the prior child process is confirmed gone, so clipboard writes and paste shortcuts cannot arrive out of order due to late completion from a timed-out predecessor.
4. Regression coverage now proves the exact audit scenario
Client tests now model a first injection job that exceeds the timeout and a second job that succeeds, then assert that only the second job's side effect is ever observed after the timeout window passes.
5. Finding 3 tracking updated
Finding 3 is now tracked as resolved locally pending PR review.

## Open High-Risk Items (Post 2026-03-08)

1. None currently open in this audit after the local Finding 5 fix.

---

## Historical ROI-Prioritized Backlog (Pre-2026-03-08)

This ordering is preserved for audit traceability from the pre-2026-03-08 mitigation state. Findings 1, 2, and 4 have since been resolved or mitigated as noted above.

1. Session audio hard limits (Finding 4).
Why this ranks first: this is still the best resilience-per-line-item trade. A hard cap is a simple circuit breaker against daemon-wide OOM and does not require protocol redesign.
2. Attach-time modifier-state seeding (Finding 5).
Why this ranks second: the impact is narrower than Finding 4, but the cost is very low because the hotkey supervision rewrite already centralizes attach/re-attach behavior. This is like fixing the last loose hinge while the door is already off the frame.
3. Unified transcriber inference gate (Finding 2).
Why this ranks third: the reliability payoff is high, but it touches the live-interim/finalization boundary and needs careful latency/regression validation around shared model access.
4. Killable injector timeout boundary (Finding 3).
Why this ranks fourth: the user-facing corruption risk is real, but the durable fix likely needs a subprocess or worker-process boundary, which makes it materially more expensive than the remaining daemon and hotkey work.

## Likely AI-Generated Anti-Patterns

1. Fail-open/fail-soft behavior without preserving invariants (timeouts and fallback paths that keep progressing while hidden work may still run).
2. Global singleton runtime objects that depend on explicit serialization/ownership discipline.
3. “Best effort” cancellation that reports success semantics stronger than actual runtime guarantees.
4. Silent resource growth assumptions (in-memory accumulation trusted to external control flow correctness).
5. Runtime degradation paths that log warnings but do not enforce safe operational contracts.

## Things That Are Unusual But Acceptable

1. Overlay event transport is intentionally non-fatal and lossy; this is acceptable because overlay is explicitly presentation-only and not the source of truth.
2. Offline-seal finalization as authoritative output while streaming is used mainly for interim UX is a defensible quality-over-latency tradeoff.
3. Copy-only fallback on paste backend failure is operationally reasonable because it preserves transcript delivery even when injection backend is degraded.

## What I Would Inspect Next

1. Release-gate multi-client reconnect/disconnect runs that keep the new owner-binding invariant from regressing.
2. Long-duration soak tests with intentional stuck sessions to validate the new daemon memory caps under saturation.
3. Hotkey attach/recovery regression coverage in CI/release gates so attach-time modifier seeding stays fixed.
4. NeMo/CUDA overlap stress runs with forced interim/final decode contention to validate the inference-gate fix.
5. Short operator validation runs on real clipboard/paste backends to confirm the subprocess boundary adds no unacceptable latency in the common path.
