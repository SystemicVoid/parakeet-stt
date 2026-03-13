# Overlay "Full-String Flash" Bug — Investigation Report

**Status:** FIXED — root cause identified and patched
**Last updated:** 2026-03-13
**Threads:** [Round 1](https://ampcode.com/threads/T-019ce3fb-eeb2-7679-bada-3eeb4c2d12bf) → [Round 2](https://ampcode.com/threads/T-019ce411-77f3-7313-97f4-89c26941f914)

---

## Revert Point

**Last safe clean state:** `b222ebebe72d470f3cd7395aad3750c26cd471b1`
**First fix commit:** `3be6b55` (chore: ignore investigation notes)

To revert all overlay flash fix attempts:
```bash
git reset --hard b222ebe
```

Commits in this fix series:
```
3be6b55 chore(gitignore): ignore local overlay flash bug investigation notes
0d5c868 fix(overlay): use append-only char animation to reduce full-string flash
4df29d4 fix(daemon): add token-level transcript stabilizer for overlay interim text
```

---

## 1. The Bug

The real-time speech-to-text overlay visually "flashes" or "fades in" the
**entire** transcribed string on every update, instead of looking like smooth
typing/appending.  Additionally, the overlay only shows the **last ~2 seconds**
of text instead of the full utterance from start, because the STT model
transcribes a rolling audio window.

The final paste into the terminal works perfectly — this is purely a visual
rendering issue in the Wayland overlay.

---

## 2. Architecture (How Text Reaches the Screen)

```
Microphone → AudioInput (16kHz float32)
    ↓
DaemonServer._emit_live_interim_from_chunk()        [server.py ~L968]
  — appends chunk to a rolling 2-second context window
  — calls _transcribe_samples_serialized() → gets FULL string from Parakeet
  — calls _stabilize_overlay_interim_text() → token-level accumulation
  — calls _emit_interim_text() which dedup-checks, then sends InterimTextMessage
    ↓
WebSocket → parakeet-ptt CLI → OverlayRouter → OverlayProcessManager
  — writes NDJSON {"type":"interim_text","session_id":...,"seq":N,"text":"..."} to stdin
    ↓
Overlay subprocess (overlay_renderer.rs)
  — main loop reads NDJSON from stdin + 50ms tick interval   [~L3417]
  — OverlayStateMachine.apply_event() REPLACES text wholesale [overlay_state.rs L142-153]
  — WaylandOverlayBackend.render():
      1. update_interim_headline_state()  — compute per-char fade range  [~L1727]
      2. compute interim_fade via interim_fade_state()                    [~L1906]
      3. render_frame():
         a. fill_frame(frame, [0,0,0,0])  — clear entire buffer
         b. draw shadow, background, border, accent/waveform
         c. draw_headline() — iterate chars, apply staggered_char_fade_alpha
      4. damage_buffer(full surface) + commit
      5. self.prev_headline = intent.headline
```

**Key constants:**
- `CHAR_FADEIN_MS = 100` — each char fades from alpha 0→1 over 100ms
- `CHAR_STAGGER_MS = 16` — each successive char starts 16ms after the previous
- Stream drain loop: `asyncio.sleep(0.05)` — ~20Hz update rate
- Tick interval: 50ms (renderer repaint when `is_fading()` is true)

---

## 3. Root Cause Analysis

Two separate but related problems:

### 3a. Full-String Flash (renderer-side)
The renderer used `changed_char_range()` to diff `prev_headline` vs `intent.headline`
using shared-prefix + shared-suffix logic.  Because the STT model re-transcribes a
rolling 2-second audio window, it frequently changes early characters (capitalization
shifts like "hello" → "Hello", word boundary changes like "go ahead" → "go, ahead").
This made `shared_prefix_len()` return 0, marking the **entire** string as the changed
range, triggering a full-string per-character fade-in animation on every single update.

### 3b. Rolling Window Truncation (daemon-side)
The daemon sends the raw model output, which only covers the last ~2 seconds of audio.
As the user speaks longer, early words drop off the model's context window.  The overlay
therefore only ever shows the most recent fragment, not the full utterance.

---

## 4. Round 1 — Append-Only Animation (renderer-side)

**Commit:** `0d5c868`

### Changes

**New function `appended_char_range()`:**
Replaced `changed_char_range` with an append-only policy.  Only returns
`Some((prev_len, cur_len))` when the new string starts with the previous string
and is strictly longer.  In-place corrections produce `None` — text appears
instantly at full alpha.

```rust
fn appended_char_range(previous: &str, current: &str) -> Option<(usize, usize)> {
    if previous == current { return None; }
    if !current.starts_with(previous) { return None; }
    let previous_len = previous.chars().count();
    let current_len = current.chars().count();
    (current_len > previous_len).then_some((previous_len, current_len))
}
```

**Updated `update_interim_headline_state()`:** calls `appended_char_range` instead
of `changed_char_range`.

**Gated old functions `#[cfg(test)]`:** `changed_char_range`, `shared_prefix_len`,
`shared_suffix_len` are test-only.

### Result
- 112 Rust tests pass
- Bug persists — the renderer fix alone is insufficient because the daemon still
  sends raw rolling-window snapshots.  The string content changes unpredictably,
  so the "stable prefix" assumption rarely holds.

---

## 5. Round 2 — Disable Per-Char Animation Entirely (reverted)

### Hypothesis
Per-character fade animation is fundamentally incompatible with 20Hz full-string
re-transcriptions.  Render each interim snapshot instantly at full alpha.

### Changes (all reverted)
1. `render()` — forced `interim_fade = None`
2. `is_fading()` — removed headline trigger
3. `update_interim_headline_state()` — always clears state

### Result — Made It Worse
- Text snaps abruptly with zero smoothing
- Container-level fades insufficient to mask content churn
- **All changes reverted** back to Round 1 state

---

## 6. Round 3 — Token-Level Transcript Stabilizer (daemon-side)

**Commit:** `4df29d4`

### Hypothesis
The daemon should accumulate a growing transcript rather than forwarding raw
rolling-window snapshots.  By tracking "committed" vs "draft" tokens and using
suffix-prefix overlap matching, the daemon can preserve earlier words as the
audio window slides forward.

### Changes

**New dataclass `OverlayInterimTranscriptState`:**
Tracks `committed_tokens` (words confirmed across multiple snapshots) and
`draft_tokens` (current model output, may change).

**New function `_longest_casefold_suffix_prefix_overlap()`:**
Finds the longest overlap between the tail of previous tokens and the head of
new tokens (case-insensitive).  This detects when the rolling window has slid
forward: if previous display was `["alpha", "beta", "gamma"]` and new model
output is `["beta", "gamma", "delta"]`, the overlap is 2 (`beta gamma`), so
`alpha` is promoted to committed and `delta` is appended.

**New method `_stabilize_overlay_interim_text()`:**
Called from both `_collect_interim_text_updates` and `_emit_live_interim_from_chunk`.
Converts raw model text into a growing accumulated transcript.

**New method `_flush_overlay_interim_pending_tail()`:**
At stop time, emits the full accumulated transcript so the overlay shows
everything before the final result replaces it.

**Updated `_collect_interim_text_updates()`:**
- Now takes `session_id` parameter
- Calls `_stabilize_overlay_interim_text()` instead of raw dedup
- Appends flush result after processing chunks

**Updated `_emit_interim_text()`:**
- Simplified dedup: exact-match only (stabilizer handles case normalization upstream)

**Session cleanup:** `_overlay_interim_transcript_by_session` cleared on session end.

### Test Results
- 112 Rust tests pass
- 59 Python tests pass (17 overlay + 42 others)
- New tests cover: first snapshot visibility, zero-overlap rewrites, case-only
  updates, rolling window preservation with accumulated transcript

### Result — Bug Still Present
User tested: the overlay **still** shows only the last few seconds of text
instead of the full utterance from start.  The token stabilizer's overlap
detection may not be firing correctly in practice, or the overlap heuristic
doesn't match the actual model output patterns.

---

## 7. Current State of the Code

All rounds of changes are committed:
- **Renderer:** `appended_char_range` in `update_interim_headline_state`
- **Daemon:** `OverlayInterimTranscriptState` with token stabilizer
- **Daemon:** Runtime stabilizer logging (PARAKEET_STREAMING_DEBUG)
- **Daemon:** Removed 2.0s context window cap (the actual fix)
- **Tests:** Updated fixtures and new stabilizer test coverage
- **Round 2 (disable animation) is reverted** — not in codebase

The overlay flash bug is **FIXED** — see Round 5.

---

## 7.5 Round 4 — Runtime Stabilizer Logging (daemon-side, diagnostic only)

**Status:** IMPLEMENTED locally on 2026-03-13, not yet validated against a live repro

### Goal
Capture the actual chunk-by-chunk model output and the stabilizer's internal
state transitions during a real utterance.  The unit tests only prove the
heuristic on synthetic sequences; they do not prove that real Parakeet output
looks like the test fixtures.

### Changes

**Opt-in logging gate:**
- Reuses existing env var `PARAKEET_STREAMING_DEBUG`
- Logging is active only when both:
  - `PARAKEET_STREAMING_DEBUG=1|true|yes|on`
  - `PARAKEET_OVERLAY_EVENTS_ENABLED=true`

**New per-attempt stabilizer logs in `server.py`:**
- `overlay_stabilizer ...`
- `overlay_stabilizer_skip ...`

Each log line includes:
- `session_id`
- `source=live|stop_replay`
- `source_seq=<n>` (monotonic per source, per session)
- `context_samples`, `context_secs`
- `raw_text`, `normalized_text`
- `previous_display`
- `committed_before`, `draft_before`
- `raw_tokens`
- `overlap`
- `committed_after`, `draft_after`
- `current_display`
- `action=emit|no_change|empty`

**Caller provenance threaded into `_stabilize_overlay_interim_text()`:**
- live path now labels each incremental decode as `source=live`
- stop-time chunk replay labels each incremental decode as `source=stop_replay`
- transcribe failures log `overlay_stabilizer_skip ... reason=transcribe_error`

### Tests Added
- live-path logging when debug is enabled
- stop-replay logging when debug is enabled
- empty-candidate logging (`action=empty`)
- transcribe-error skip logging
- logging stays disabled by default

### Verification
- `uv run pytest tests/test_overlay_event_stream.py` → 26 passed
- `uv run ruff check src/parakeet_stt_daemon/server.py tests/test_overlay_event_stream.py` → passed

### Why This Matters
This turns the stabilizer into a black box with a flight recorder.  Instead of
guessing whether overlap detection *should* have worked, the daemon can now
tell us exactly what text it received and what decision it made at each step.

---

## 8. Round 5 — Remove 2.0s Context Window Cap (THE FIX)

**Commit:** `faeb6e2`
**Status:** FIXED — verified visually in live session 2026-03-13

### Root Cause (Definitive)

The flight recorder logs from Round 4 proved the root cause with zero
ambiguity.  Across 28 stabilizer events in two live sessions, `overlap=0`
in every single one.  `committed_tokens` was always empty.

**The 2.0s context window was smaller than the 2.4s chunk size.**

```
OVERLAY_INTERIM_CONTEXT_WINDOW_SECS = 2.0    (32,000 samples at 16kHz)
chunk_secs                           = 2.4    (38,400 samples at 16kHz)
```

`_append_overlay_interim_context()` capped the audio buffer at 32,000
samples.  Each 38,400-sample chunk overwrote the entire buffer.
Consecutive calls to `_transcribe_samples_serialized()` heard completely
disjoint audio, so the model produced completely unrelated text.  The
stabilizer's overlap detection was algorithmically correct but structurally
impossible to satisfy.

Three compounding causes (all proven by logs):

1. **Context window (2.0s) < chunk size (2.4s)** — disjoint audio,
   overlap=0 always.
2. **Overlay used stateless offline transcriber** — while the streaming
   helper (`BatchedFrameASRTDT`, left_context=10s) accumulated properly,
   its output was never used for the overlay.
3. **Update rate ~0.42 Hz** — one transcription per 2.4s chunk, not the
   20 Hz the drain loop interval suggested.

### Changes

Removed `OVERLAY_INTERIM_CONTEXT_WINDOW_SECS` and the truncation logic
in `_append_overlay_interim_context()`.  The overlay audio buffer now
grows for the full session duration.  Consecutive transcriptions share
most of their audio, producing overlapping text that the stabilizer can
properly merge.

```python
# Before: capped at 2.0s (< 2.4s chunk = always disjoint)
max_samples = max(1, int(self.audio.sample_rate * OVERLAY_INTERIM_CONTEXT_WINDOW_SECS))
if combined.size > max_samples:
    return combined[-max_samples:]

# After: grow for full session
@staticmethod
def _append_overlay_interim_context(existing, chunk_audio):
    if existing.size == 0:
        return np.array(chunk_audio, copy=True)
    return np.concatenate((existing, chunk_audio))
```

### Test Changes

Replaced two "context window is bounded" tests with "context window grows
across session/chunks" tests that verify the buffer accumulates
monotonically and reaches the expected total size.

### Result
- 105 Python tests pass
- 112 Rust tests pass
- Overlay now shows progressively growing text during live dictation
- The stabilizer fires correctly with non-zero overlap values

---

## 9. Commit Audit — Essential vs Removable

Commits in the fix series (oldest to newest):

| Commit | Description | Essential? | Notes |
|--------|-------------|------------|-------|
| `3be6b55` | chore: gitignore OVERLAY-FLASH-BUG.md | **KEEP** | Repo hygiene |
| `0d5c868` | Renderer: append-only char animation | **KEEP** | Prevents full-string fade-in on corrections |
| `4df29d4` | Daemon: token-level transcript stabilizer | **KEEP** | Accumulates growing transcript from overlapping snapshots |
| `069ff95` | Daemon: runtime stabilizer diagnostics | **REMOVABLE** | Debug logging; served its diagnostic purpose |
| `faeb6e2` | Daemon: remove 2.0s context window cap | **KEEP** | The actual fix |

### What Could Be Cleaned Up

The **diagnostic logging** from Round 4 (`069ff95`) adds ~200 lines of
structured logging code that was instrumental for finding the root cause
but is no longer needed for production:

- `_log_overlay_stabilizer_event()` and its 18 keyword args
- `_log_overlay_stabilizer_skip()` and its 6 keyword args
- `_overlay_stabilizer_debug_enabled()`
- `_next_overlay_interim_source_seq()` and `_overlay_interim_source_seq_by_session`
- `_streaming_debug_enabled()` (if not used elsewhere)
- All stabilizer logging calls woven through `_stabilize_overlay_interim_text()`
  and `_emit_live_interim_from_chunk()`
- 5 corresponding test functions in `test_overlay_event_stream.py`

**Decision:** The logging is gated behind `PARAKEET_STREAMING_DEBUG=1` and
adds zero overhead in production.  It could be kept as a permanent flight
recorder for future regressions, or removed for code cleanliness.

---

## 10. Open Question — Reintroduce A Bound Without Rebreaking Overlay Correctness?

**Status:** OPEN FOLLOW-UP, not a revert candidate as of 2026-03-13

### Why this is open

`faeb6e2` intentionally removed the overlay interim audio cap because the old
bound was **smaller than one chunk**:

- old cap: `2.0s` = `32,000` samples at 16kHz
- chunk size: `2.4s` = `38,400` samples at 16kHz

That meant each new chunk fully displaced the previous buffer.  The overlay
transcriber heard disjoint audio on every update, the stabilizer observed
`overlap=0`, and the overlay rewrote text wholesale instead of accumulating.

Put differently: the old cap did not merely "limit context"; it destroyed the
minimum overlap the stabilizer needed to function at all.

### Why this should not be reverted blindly

The current unbounded behavior is the first version that fixed the live issue
in real sessions.  Reinstating the previous bounded-window behavior as-is would
be a likely correctness regression, because it would recreate the exact
"disjoint audio" failure mode that Round 4 diagnostics proved.

### Why this is still a real engineering question

The current fix trades correctness for replay cost.  Both live interim
(`_emit_live_interim_from_chunk`) and stop replay (`_collect_interim_text_updates`)
now re-transcribe an ever-growing audio buffer.  For long sessions, that makes
the overlay path's inference work grow roughly with session length on each
chunk, so total work trends toward quadratic over the utterance.

That does **not** invalidate `faeb6e2`; it means the current fix is a
correctness-first implementation, not necessarily the final performance shape.

### Precise follow-up question

Can we restore a **bounded** overlay interim context without reintroducing the
disjoint-audio bug, and if so, what is the minimum safe bound or alternate
architecture?

Any future answer must preserve these invariants:

1. The bound must be **larger than a single chunk** and large enough to retain
   stable overlap between consecutive transcriptions.
2. Overlay interim text must continue to accumulate correctly under the token
   stabilizer; a return to `overlap=0` behavior is a hard regression.
3. The fix must be validated against **real live sessions**, not only synthetic
   unit tests, because the original failure escaped test coverage.
4. If a bound is reintroduced, it should be justified by measured CPU/latency
   wins on long sessions, not by aesthetics or code neatness.

### Candidate directions when revisiting

- Replace "full session replay" with a **safe bounded ring buffer** sized from
  actual chunking/overlap requirements rather than the previous arbitrary 2.0s.
- Feed overlay interim text from a **truly incremental streaming source** rather
  than repeatedly re-running the stateless offline transcriber on growing audio.
- Add a long-session soak/perf check that captures overlay interim CPU/latency
  so future optimizations can be compared against the known-correct baseline.

### Bottom line

The open question is **not** "should we put the old cap back?"  That answer is
currently no.  The real question is "can we replace the unbounded correctness
fix with a bounded design that still guarantees overlap and keeps the overlay
stable?"

---

## 11. Key File Locations

| What | File | Line |
|------|------|------|
| `appended_char_range` | `overlay_renderer.rs` | ~L798 |
| `update_interim_headline_state` | `overlay_renderer.rs` | ~L1727 |
| `_append_overlay_interim_context` (unbounded) | `server.py` | ~L647 |
| `OverlayInterimTranscriptState` | `server.py` | ~L61 |
| `_longest_casefold_suffix_prefix_overlap` | `server.py` | ~L73 |
| `_stabilize_overlay_interim_text` | `server.py` | ~L940 |
| `_collect_interim_text_updates` | `server.py` | ~L1033 |
| `_emit_live_interim_from_chunk` | `server.py` | ~L1129 |
