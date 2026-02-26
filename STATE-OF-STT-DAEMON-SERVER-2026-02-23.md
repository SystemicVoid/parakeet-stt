# State of STT Daemon Server (2026-02-23)

## Executive Summary

The daemon is functional for baseline offline dictation on GPU, but there are several code-truth issues that materially impact reliability and roadmap feasibility.

Top conclusions:

1. The highest-risk stability gap is session cleanup on websocket disconnect/handler failure. Active audio capture state can remain live without an owner.
2. The current CLI/settings merge logic unintentionally overrides env configuration for key booleans (`streaming_enabled`, `status_enabled`).
3. "Streaming enabled" is currently misleading in practice: the configured helper class import fails on this stack, so the daemon silently falls back to offline finalization.
4. The GPU stack still works for offline inference, but the runtime guidance in repo docs is outdated: `cu130` is now stable in upstream PyTorch, and the local lock is behind latest torch/nemo releases.
5. The daemon has no test suite today, so regressions in session invariants, cleanup, and protocol behavior are likely to recur.

Net: this is a strong candidate for a daemon hardening sprint before adding higher-level UX features that depend on reliable streaming semantics.

## Status Update (2026-02-25, External Research Synthesis — Streaming WER Gap)

Two independent deep research passes (GPT and Gemini) were run against a detailed technical brief
([`deep-research-streaming-quality.md`](deep-research-streaming-quality.md)) covering the streaming
WER gap, root cause analysis, and both-path improvements. Full findings and action items are in
that file. Key conclusions summarized here for operational continuity.

### Confirmed Root Causes (from Research)

1. **End-of-utterance drain missing (RC1 — highest confidence).** Buffered transducer decoders delay
   token emission until right-context frames arrive. When the pipeline stops at the last real audio
   frame those buffered tokens are never emitted. Our tail-padding experiments are direct behavioral
   evidence: every added 0.2s of silence improved WER monotonically (0.535 → 0.410). The proper fix
   is an explicit drain step (feed silence frames until the decoder flushes), not waveform padding.

2. **`tokens_per_chunk` overflow for TDT burst emission (RC2 — high confidence).** The RNNT-derived
   formula `ceil(chunk_secs / model_stride)` assumes linear 1:1 frame-to-token emission. TDT's
   duration skipping + FastEmit creates dense bursts that can exceed this limit at chunk boundaries,
   silently clipping the excess tokens. This is why grid-sweeping chunk sizes gave only marginal gains.

3. **`stateful_decoding` inheritance bug in `BatchedFrameASRTDT` (RC3 — needs verification).**
   Gemini identified that `BatchedFrameASRTDT.__init__` accepts `stateful_decoding` but never passes
   it to `super().__init__()`. The base class always runs stateless regardless of the caller's intent.
   Explains the paradox: `stateful_decoding=True` produced WER 0.686, *worse* than `False` (0.346).
   Must verify against installed NeMo source before acting.

### New Action Items (SA series)

| ID | Item | Priority |
|---|---|---|
| SA1 | Verify `stateful_decoding` inheritance bug in NeMo `BatchedFrameASRTDT` source | P0 |
| SA2 | Implement explicit EOU drain in streaming finalize (not waveform zero-padding) | P0 |
| SA3 | Upgrade NeMo to 2.6.2 (security-justified, explicit TDT streaming fix in release notes) | P1 |
| SA4 | Compute tail padding from model config (`hop_ms × shift_frames × 2 × sr/1000` ≈ 320ms) | P1 |
| SA5 | Implement Stream-Then-Seal: `conformer_stream_step()` for partials + `model.transcribe()` seal | P1 |
| SA6 | Install `cuda-python` (removes NeMo startup warning, enables CUDA graph decode) | P2 |
| SA7 | Integrate Silero VAD v6 to replace RMS-based endpointing | P2 |
| SA8 | Prototype `conformer_stream_step()` cache-aware streaming loop | P2 |
| SA9 | Evaluate NeMo 2.7.x for Transducer CUDA Graphs + decoder memory leak fixes | P3 |
| SA10 | Investigate TDT-correct `tokens_per_chunk` formula accounting for burst emission rate | P3 |

### Recommended Immediate Sequence

1. `SA1` (read NeMo source for the inheritance bug — diagnostic only, ~30min)
2. `SA2 + SA4` together (EOU drain + correct padding formula — same finalize code area)
3. `SA3` (upgrade NeMo to 2.6.2, re-run bench)
4. `SA5` (Stream-Then-Seal as the highest-confidence path to <0.20 WER)

Full rationale in `deep-research-streaming-quality.md` under "Research Synthesis".

## Status Update (2026-02-25, SA Execution Checklist + Acceptance Gates)

This section is the implementation checklist for SA1..SA10. It is intentionally
written as an operator gate sheet so changes can be landed incrementally with
small commits while protecting offline dictation behavior.

### Offline Safety Contract

- Default operator path remains offline because `scripts/stt-helper.sh` starts daemon with `--no-streaming`.
- Any change in `ParakeetStreamingSession.finalize()` must not alter `ParakeetTranscriber.transcribe_samples()` behavior.
- `SA3` (NeMo upgrade) is the only planned change that can affect offline inference semantics and requires hard pre/post benchmark gating.
- `SA7` (VAD) must launch as opt-in (`PARAKEET_VAD_ENABLED`) with default preserving current RMS trim behavior.

### Acceptance Criteria by SA Item

| ID | Acceptance Criteria | Verification Loop | Status |
|---|---|---|---|
| SA1 | Confirm whether `BatchedFrameASRTDT.__init__` forwards `stateful_decoding` to base class in installed NeMo; record exact source path + line evidence. | Inspect installed NeMo source directly; copy findings into this doc. | DONE |
| SA2 | Streaming finalize performs explicit end-of-utterance drain pass (feature-frame/decoder flush), not only waveform zero-padding; truncation reduced on bench set. | Unit tests for drain behavior + bench A/B run with streaming enabled. | DONE |
| SA3 | NeMo upgraded to 2.6.2 with no offline regression beyond thresholds; streaming helpers still initialize. | Run `check_model.py --bench-offline` before/after with fixed thresholds; run daemon smoke + tests. | DONE |
| SA4 | Tail/drain frame count derived from model config (`hop_length`, streaming shift/caches), no hardcoded seconds constant for correctness path. | Unit test asserting computed pad/drain samples from mocked model cfg values. | DONE |
| SA5 | Stream-Then-Seal enabled: partials come from streaming path, final committed transcript uses offline `model.transcribe()` seal pass. | Session integration tests + bench check that final text equals offline path for same audio. | DONE |
| SA6 | `cuda-python` installed/configured; NeMo startup warning removed; no inference API changes. | `parakeet-stt-daemon --check` warning-free for cuda-graphs note; benchmark latency snapshot. | DONE |
| SA7 | Silero VAD integration available behind opt-in flag; default behavior unchanged; both path metrics captured when enabled. | A/B tests with env flag on/off + regression tests for default path parity. | DONE |
| SA8 | Prototype `conformer_stream_step()` cache-aware partial streaming path without touching offline finalize path. | New targeted tests + manual streaming smoke with helper truth fields. | DONE |
| SA9 | Evidence-based decision doc for 2.7.x (latency/memory/leak fixes) after 2.6.2 stabilization. | Release-note/source audit + controlled benchmark comparison report. | DONE |
| SA10 | TDT-correct `tokens_per_chunk` candidate formulas documented and experimentally compared (baseline vs burst-aware variants). | Bench sweep script output committed (or archived) with WER/latency deltas. | DONE |

### Global Regression Gates (apply per commit where relevant)

- `cd parakeet-stt-daemon && uv run pytest -q tests/`
- `cd parakeet-stt-daemon && uv run ruff check .`
- `cd parakeet-stt-daemon && uv run ruff format --check .`
- `cd parakeet-stt-daemon && ty check .`
- `cd parakeet-stt-daemon && uv run --with pyright pyright src/parakeet_stt_daemon/ tests/`
- For offline-risk commits (`SA3`, opt-in promotion of `SA7`):
  - `cd parakeet-stt-daemon && uv run python check_model.py --bench-offline --device cuda --max-avg-wer <locked> --max-p95-infer-ms <locked> --max-p95-finalize-ms <locked>`

### Execution Log (2026-02-25)

- [x] SA1 complete and evidence captured
- [x] SA2+SA4 implemented with tests and bench deltas recorded
- [x] SA5 stream-then-seal landed with integration tests
- [x] SA6 installed + warning removal verified
- [x] SA8 prototype behind explicit flag
- [x] SA3 upgrade branch validated and merged (or deferred with reasons)
- [x] SA7 opt-in VAD landed with default-off safety
- [x] SA9+SA10 research follow-ups recorded

### SA1 Evidence (Installed NeMo 2.5.3)

- Verified module path: `/home/hugo/Documents/Engineering/parakeet-stt/parakeet-stt-daemon/.venv/lib/python3.11/site-packages/nemo/collections/asr/parts/utils/streaming_utils.py`.
- `BatchedFrameASRTDT.__init__(..., max_steps_per_timestep, stateful_decoding, tdt_search_boundary)` calls:
  - `super().__init__(asr_model, frame_len=frame_len, total_buffer=total_buffer, batch_size=batch_size)`
- `BatchedFrameASRRNNT.__init__` signature is:
  - `(..., max_steps_per_timestep: int = 5, stateful_decoding: bool = False)`
- Conclusion: `BatchedFrameASRTDT` currently drops the caller-supplied `max_steps_per_timestep` and `stateful_decoding` during `super()` construction in installed NeMo 2.5.3. The daemon-side workaround (`self.chunk_helper.stateful_decoding = ...` and `self.chunk_helper.max_steps_per_timestep = ...`) is still required until upstream fix/upgrade.

### SA2 + SA4 Progress (Implementation Pass 1)

- Streaming finalize now runs in two steps for TDT helper mode:
  1. decode real buffered audio,
  2. run an explicit drain pass over silence frames to flush delayed end-of-utterance tokens.
- Drain sizing now comes from model metadata first (`window_stride × shift_frames × 2 × sample_rate`), with fallback to `delay × model_stride_secs × sample_rate` when streaming shift metadata is unavailable.
- Added regression tests:
  - explicit drain pass executes for TDT helper (`tokens_per_chunk` + `delay` path),
  - config-derived drain sample count (Parakeet-like values) equals 5120 samples at 16kHz,
  - delay/stride fallback formula remains covered.
- Bench re-run after implementation (same 8-sample bench set, `chunk_secs=2.4`, `right_context_secs=1.6`):
  - average offline WER: `0.155`
  - average streaming WER: `0.414`
  - average offline infer latency: `71.2ms`
  - average streaming finalize latency: `253.4ms`
  - note: streaming WER improved vs prior documented `0.488` baseline, but remains far above target `<0.20`.
- Full local quality gate run after changes:
  - `uv run pytest -q tests/` -> `44 passed`
  - `uv run ruff check .` -> pass
  - `uv run ruff format --check .` -> pass
  - `ty check .` -> pass
  - `uv run --with pyright pyright src/parakeet_stt_daemon/ tests/` -> pass
- Follow-up after SA2/SA4: move to `SA5` (Stream-Then-Seal) for reliable final-result WER target.

### SA5 Progress (Implementation Pass 1)

- Added stream-then-seal finalize mode in `ParakeetStreamingSession.finalize()`:
  - default (`PARAKEET_STREAM_THEN_SEAL=1`): return final transcript from offline `transcribe_samples()` path,
  - opt-out (`PARAKEET_STREAM_THEN_SEAL=0`): keep helper-only finalize for streaming quality experiments.
- Safety property: this change is still scoped to streaming sessions only; offline daemon usage path is unchanged.
- Regression coverage added in `tests/test_streaming_chunk_padding.py`:
  - default mode returns sealed offline text,
  - helper-only mode remains testable for drain-path assertions.
- Bench re-run after SA5 (same 8-sample bench set, same chunk/right-context config):
  - average offline WER: `0.094`
  - average streaming WER: `0.094`
  - average offline infer latency: `68.8ms`
  - average streaming finalize latency: `53.0ms`
- Interpretation: committed streaming final result now matches offline quality on the current bench set while preserving sub-200ms finalize latency.

### SA6 Progress (Implementation Pass 1)

- Added `cuda-python` runtime dependency and pinned to a NeMo-compatible major range:
  - `cuda-python>=12.3,<13`
- Why pinning is required: `cuda-python` 13.x no longer exposes `from cuda import cuda`, but NeMo 2.5.3's CUDA graph detection still imports that path.
- Validation:
  - `uv run python -c "from cuda import cuda"` succeeds (with deprecation warning from upstream package).
  - `uv run parakeet-stt-daemon --check` no longer prints the prior NeMo warning:
    - `No conditional node support for Cuda ... Reason: No cuda-python module`.
- Scope: dependency/runtime optimization only; no daemon API surface changes.

### SA8 Progress (Implementation Pass 1)

- Added an explicit experimental path for cache-aware conformer partials behind env flag:
  - `PARAKEET_EXPERIMENTAL_CONFORMER_PARTIALS=1`
- Implementation details:
  - `ParakeetStreamingTranscriber.start_session()` now optionally initializes cache state via
    `encoder.get_initial_cache_state(batch_size=1)` and keeps runtime truth fields for partial path status.
  - `ParakeetStreamingSession.feed()` now invokes a best-effort conformer partial step when the flag is enabled,
    while preserving existing chunk buffering and finalize behavior.
  - Failures in the experimental path self-disable and publish a structured fallback reason
    (`partial_stream_failed:<ExceptionClass>`) without affecting finalize/offline seal behavior.
- Runtime truth surfacing:
  - `/status` now includes `partial_stream_active` and `partial_stream_fallback_reason`.
  - Startup runtime truth log now prints partial-stream truth fields alongside existing helper truth.
- Regression coverage added:
  - `tests/test_streaming_chunk_padding.py`
    - partial state disabled by default,
    - flag-enabled partial-state init,
    - partial feed updates text on mocked conformer step,
    - missing `conformer_stream_step` reports structured fallback reason.
  - `tests/test_streaming_truth.py`
    - status truth coverage for active/inactive partial stream and fallback-reason paths.
- Manual streaming smoke (CUDA, real model, flag enabled):
  - command: `PARAKEET_EXPERIMENTAL_CONFORMER_PARTIALS=1 uv run python - <<'PY' ...`
  - observed: partial path initializes at session start (`partial_before_feed True None`), then self-disables on
    first step with `partial_stream_failed:TypeError`; finalize still returns via existing safe path.
  - interpretation: SA8 prototype wiring and truth instrumentation are in place; step-level API adaptation remains
    incomplete and is now explicitly visible as fallback telemetry instead of silent behavior.
- Full local quality gate run after changes:
  - `uv run pytest -q tests/` -> `50 passed`
  - `uv run ruff check .` -> pass
  - `uv run ruff format --check .` -> pass
  - `ty check .` -> pass
  - `uv run --with pyright pyright src/parakeet_stt_daemon/ tests/` -> pass

### SA3 Progress (Implementation Pass 1)

- Dependency lane updated to NeMo 2.6.2:
  - `nemo-toolkit[asr]>=2.6.2,<2.7` in runtime deps and `inference` extra.
- During first upgrade pass, offline benchmark regressed to WER `1.0000` due a CUDA graph decode runtime mismatch when paired with `cuda-python<13`:
  - exception observed in direct probe: `ValueError: not enough values to unpack (expected 6, got 5)` from NeMo TDT CUDA-graph path.
- Compatibility fix for NeMo 2.6.2 lane:
  - moved runtime dep to `cuda-python>=13,<14`.
  - direct offline probe on `sample_01.wav` returns expected hypothesis text again.
- Offline benchmark gating (CUDA, `check_model.py --bench-offline`) with locked thresholds:
  - thresholds: `max_avg_wer=0.12`, `max_p95_infer_ms=300`, `max_p95_finalize_ms=300`.
  - post-upgrade result: `avg_wer=0.0938`, `infer_p95=184.43ms`, `finalize_p95=184.77ms` -> gate **PASS**.
- Streaming helper smoke validation after upgrade:
  - `PARAKEET_STREAMING_ENABLED=true uv run parakeet-stt-daemon --check`
  - helper status: `ACTIVE (class=BatchedFrameASRTDT)`.
- Offline startup smoke:
  - `uv run parakeet-stt-daemon --check --no-streaming` passed.
- Full local quality gate run after dependency update:
  - `uv run pytest -q tests/` -> `50 passed`
  - `uv run ruff check .` -> pass
  - `uv run ruff format --check .` -> pass
  - `ty check .` -> pass
  - `uv run --with pyright pyright src/parakeet_stt_daemon/ tests/` -> pass

### SA7 Progress (Implementation Pass 1)

- Added opt-in Silero VAD tail-trim path (default-off safety preserved):
  - new setting: `PARAKEET_VAD_ENABLED` (`ServerSettings.vad_enabled`, default `False`).
  - when enabled, `_trim_tail_silence()` attempts Silero VAD endpoint trim first, then falls back to existing RMS trim on any init/runtime failure.
- Added runtime dependency for opt-in path:
  - `silero-vad>=6,<7`.
- Safety/compatibility behavior:
  - if Silero model import/init fails, daemon logs warning once and keeps RMS path.
  - default operator flow remains unchanged because `PARAKEET_VAD_ENABLED` is off by default.
- Regression coverage:
  - `tests/test_streaming_truth.py`
    - default trim path uses RMS,
    - VAD-enabled path prefers VAD result,
    - VAD-enabled path falls back to RMS when VAD path returns `None`.
- A/B smoke checks (`--check --no-streaming`):
  - `PARAKEET_VAD_ENABLED=false uv run parakeet-stt-daemon --check --no-streaming` -> pass
  - `PARAKEET_VAD_ENABLED=true uv run parakeet-stt-daemon --check --no-streaming` -> pass
- Full local quality gate run after SA7 changes:
  - `uv run pytest -q tests/` -> `53 passed`
  - `uv run ruff check .` -> pass
  - `uv run ruff format --check .` -> pass
  - `ty check .` -> pass
  - `uv run --with pyright pyright src/parakeet_stt_daemon/ tests/` -> pass

### SA10 Progress (Investigation Pass 1)

- Ran controlled helper-only streaming sweep in `PARAKEET_STREAM_THEN_SEAL=0` mode over the canonical 8-sample bench set.
- Baseline model/streaming config:
  - `chunk_secs=2.4`, `right_context_secs=1.6`, helper `BatchedFrameASRTDT`, base `tokens_per_chunk=30`.
- Candidate formulas evaluated:
  - baseline: `tokens_per_chunk = base`
  - burst-aware 1: `ceil(base × 1.5)`
  - burst-aware 2: `ceil(base × 2.0)`
  - burst-aware 3: `base + max_steps_per_timestep`
- Average WER results:
  - baseline (`30`): `0.5022`
  - `ceil(base × 1.5)` (`45`): `0.5137`
  - `ceil(base × 2.0)` (`60`): `0.6481`
  - `base + max_steps_per_timestep` (`35`): `0.5752`
- Conclusion:
  - naive burst multipliers did **not** improve WER on this bench; baseline RNNT-derived `tokens_per_chunk` remained best among tested candidates.
  - SA10 outcome is recorded as a negative-result gate: keep current formula for now and defer deeper TDT-specific derivation work.

### SA9 Progress (Evaluation Pass 1)

- Release/source audit completed:
  - NeMo releases currently expose `2.6.x` stable line (`2.6.0`, `2.6.1`, `2.6.2`); no published `2.7.x` release tag available in upstream release feed at this time.
  - NeMo changelog confirms ASR/streaming additions in `2.6.0` and security-only `2.6.2` patch.
- Operational evidence from this lane:
  - `2.6.2` required compatibility correction to `cuda-python>=13,<14` in our runtime to avoid CUDA-graph decode failures.
  - upstream issue traffic still reports CUDA-graph decode instability in some `2.6.x` environments.
- Decision:
  - defer `2.7.x` upgrade work until a stable `2.7.x` release is published and validated against transducer CUDA-graph decode in upstream notes/issues.
  - remain on validated `2.6.2` + `cuda-python>=13,<14` for now.

---

## Status Update (2026-02-25, Offline In-Memory Finalize + Benchmark Priority)

- Offline finalize path now transcribes in-memory `np.ndarray` audio by default:
  - `DaemonServer._finalise_transcription(...)` now calls `ParakeetTranscriber.transcribe_samples(...)` instead of writing a temp wav first.
  - `ParakeetStreamingTranscriber._transcribe_offline(...)` also now uses in-memory transcription first.
- Compatibility fallback retained:
  - `ParakeetTranscriber.transcribe_samples(...)` falls back to temp wav transcription only if direct in-memory transcription raises.
- Warmup no longer requires a temp wav write; it runs against in-memory silence.
- Added regression coverage for:
  - in-memory transcription path,
  - temp-wav fallback path when array decode fails,
  - server offline finalize path using in-memory transcription.
- Prioritization update:
  - delivery order now explicitly gates UX phases behind a committed repeatable offline benchmark harness with stable WER/timing thresholds.

## Status Update (2026-02-23, Post A1/A2/A3)

This report was originally authored before the core hardening tranche landed. Current code state now differs for several high-risk findings:

- `A1` done: active-session cleanup is unified and wired into disconnect, handler-exception, and abort flows via `_cleanup_active_session(...)`.
- `A2` done: boolean precedence now follows `CLI explicit > ENV > defaults` for `status_enabled` and `streaming_enabled`.
- `A3` done: `start_session` now uses transactional rollback semantics; post-allocation failures in audio start, stream init, drain-loop startup, or session-start response send all rollback to idle.
- `A3+` done: websocket disconnect cleanup now snapshots active-session ownership and requires session-match, preventing cross-session teardown after start-path disconnect rollback.
- `A5` bootstrap done: daemon pytest harness exists, with focused lifecycle + precedence tests.
- `B2/C1` partial done: status/startup/completion now expose runtime truth fields (`requested/effective device`, helper active/fallback reason) plus last-stage timing fields (`audio/infer/send`).

## Status Update (2026-02-23, Post B1/A6)

- `B1/A6` done: streaming helper aligned with supported NeMo 2.5.3 API.
  - Replaced non-existent `ChunkedRNNTInfer` import with `FrameBatchChunkedRNNT` + `AudioFeatureIterator` from `nemo.collections.asr.parts.utils.streaming_utils`.
  - `finalize()` now uses frame-reader-based call pattern: `AudioFeatureIterator(samples, ...) → set_frame_reader() → transcribe()`.
  - `finalize()` now explicitly sets `pad_to_frame_len=False` on `AudioFeatureIterator` to match NeMo chunked semantics and avoid synthetic zero-padded tail frames on short final chunks.
  - `--check` and startup logs now report streaming helper truth (ACTIVE with class name, or INACTIVE with fallback reason).
  - Startup log promoted to WARNING when streaming is enabled but helper inactive.
  - `total_buffer_secs = chunk_secs + right_context_secs`; `left_context_secs` retained in config but managed internally by `FrameBatchASR` frame buffering.
  - 11 new streaming truth tests cover the full state matrix: disabled-by-config, enabled+active, enabled+inactive (import/init/reset failure), transcriber-unavailable, timing fields, session age.
  - Added dedicated regression coverage for chunked finalization padding behavior (`tests/test_streaming_chunk_padding.py`).

Validation snapshot for B1/A6:

- `cd parakeet-stt-daemon && uv run pytest -q tests/test_session_cleanup.py tests/test_cli_precedence.py tests/test_streaming_truth.py tests/test_streaming_chunk_padding.py` -> `28 passed`
- `cd parakeet-stt-daemon && uv run ruff check .` -> pass
- `cd parakeet-stt-daemon && uv run ruff format --check .` -> pass
- `cd parakeet-stt-daemon && ty check .` -> pass (2 warnings: pyright-specific type: ignore not recognized by ty)
- `cd parakeet-stt-daemon && uv run --with pyright pyright src/parakeet_stt_daemon/server.py src/parakeet_stt_daemon/model.py src/parakeet_stt_daemon/messages.py src/parakeet_stt_daemon/__main__.py tests/` -> pass

## Status Update (2026-02-24, Post B2/C1/B3)

- `B2/C1` done: `/status` now reports `gpu_mem_mb` (CUDA reserved memory) plus stable timing taxonomy fields (`audio_stop_ms`, `finalize_ms`, `infer_ms`, `send_ms`). Completion logs now use the new timing taxonomy. `last_*` timing fields remain for compatibility.
- `B3` done: error taxonomy expanded with `SESSION_NOT_FOUND`, `SESSION_ABORTED`, and `INVALID_REQUEST`. Stop/abort/parse paths now emit precise codes. `parakeet-ptt` logs classify error codes without breaking on unknown values.
- SPEC and client status parsing updated to match the enriched status payload.

## Status Update (2026-02-24, Streaming Validation Logs)

Manual dictation runs captured in `tmp-last-test-logs.txt`:

- Streaming run: `PARAKEET_STREAMING_ENABLED=true uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765`
  - Streaming helper initialised: `FrameBatchChunkedRNNT`.
  - Every stop logged `Streaming helper failed during finalization: too many values to unpack (expected 2)` from `parakeet_stt_daemon.model:finalize:165`.
  - After each finalize failure, offline `Transcribing` ran and sessions completed with `stream_helper_active=True` and `stream_fallback_reason=None`.
  - numpy warnings observed during one session: `Mean of empty slice` and `invalid value encountered in divide` (multiple entries at `2026-02-24 11:28:19`).
  - Example session timings (from log): `finalize_ms` 135–210ms, `infer_ms` 134–208ms, `latency_ms` 135–210ms, `audio_ms` 4925–8235.
- Non-streaming run: `PARAKEET_STREAMING_ENABLED=false uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765`
  - `stream_helper_active=False`, no streaming-finalize warnings.
  - Example session timings (from log): `finalize_ms` 46–71ms, `infer_ms` 46–70ms, `latency_ms` 46–71ms, `audio_ms` 5629–7923.

## Status Update (2026-02-24, Streaming Finalize Fix)

- Root cause: NeMo `FrameBatchChunkedRNNT` expects `rnnt_decoder_predictions_tensor(...)` to return two values, but current NeMo returns a single list of hypotheses for Parakeet TDT, causing the streaming finalize unpack error.
- Fix: wrapped `FrameBatchChunkedRNNT` with a compatibility `_get_batch_preds` that normalizes decoder output via `_coerce_rnnt_texts`, handling legacy tuple returns and current hypothesis lists (including N-best).
- Regression coverage: new unit tests for `_coerce_rnnt_texts` in `tests/test_streaming_chunk_padding.py`.
- Validation: local helper transcribe now runs without streaming-finalize warnings (silence input returns empty text without offline fallback).
- Tests: `cd parakeet-stt-daemon && uv run pytest -q tests/` -> `34 passed`

## Status Update (2026-02-24, Streaming Validation)

Streaming validation runs (CUDA, synthetic sine input):

- `cd parakeet-stt-daemon && uv run python check_model.py --device cuda`
  - Offline transcription: `''` (sine wave)
  - Streaming helper initialised (frame_len=0.5, total_buffer=1.5, batch_size=4)
  - Warning during finalize: `Kernel size can't be greater than actual input size` -> finalize fell back to offline path.
- `cd parakeet-stt-daemon && uv run python check_model.py --device cuda --duration 8.0`
  - Same finalize warning with `frame_len=0.5`.
- Server-equivalent chunk test:
  - `uv run python - <<'PY' ... chunk_secs=2.0 ... PY` (custom probe)
  - Streaming helper initialised (frame_len=2.0, total_buffer=4.0, batch_size=32)
  - No streaming-finalize warnings; result returned via streaming helper (empty transcript on sine).
  - Numpy warnings about `Mean of empty slice` persisted during synthetic input.

## Status Update (2026-02-24, Streaming Dictation Quality Check)

Manual dictation run (daemon log `/tmp/parakeet-daemon-streaming.log`, streaming enabled):

- Startup confirms `streaming_enabled=True`, helper active (`FrameBatchChunkedRNNT`) with `chunk_secs=2.0`.
- Four sessions completed with `stream_helper_active=True` and `stream_fallback_reason=None` (no offline fallback).
- One session still logged numpy warnings about `Mean of empty slice` during finalize.
- Operator reported perceived transcript quality worse than offline during this streaming run.

## Status Update (2026-02-24, Bench Audio A/B Validation)

Bench dataset (canonical location): `parakeet-stt-daemon/bench_audio/` with
`transcripts.txt` + `sample_01.wav` .. `sample_08.wav`.

Test harness: local A/B script over the bench WAVs using TDT helper (`BatchedFrameASRTDT`)
vs offline transcription on the same files.

Results (normalized WER):

- Average streaming WER: `0.679`
- Average offline WER: `0.088`

Qualitative notes:

- Streaming outputs are consistently truncated at the end (and occasionally the start).
- Offline outputs are largely complete; minor normalization/casing/punctuation differences only.
- Forcing stateful decoding on the TDT helper did not eliminate truncation in this run.

## Status Update (2026-02-24, TDT Helper Tuning)

Follow-up bench A/B runs compared helper variants and tuning knobs on the same
`bench_audio` dataset (`chunk_secs=2.0`, `right_context_secs=2.0`):

- Offline baseline: average WER `0.071` (unchanged).
- RNNT helper (`FrameBatchChunkedRNNT`): average WER `0.608` (truncation persists).
- TDT helper default (stateful decoding enabled): average WER `0.686`.
- TDT helper with `stateful_decoding=False`: average WER improved to ~`0.446`
  (still truncated but better than stateful mode).
- `tdt_search_boundary` sweep: `4` and `8` were similar (~`0.47` WER); `12` regressed.
- Delay experiments: default mid-delay (`38` tokens) was best; right-context delay
  (`25` tokens) worsened (~`0.585` WER) and half-delay (`13`) was much worse (~`0.851`).
- `pad_to_frame_len` must stay enabled for TDT; disabling it raises shape mismatches
  inside `BatchedFeatureFrameBufferer` for partial tail frames.

Action taken: updated TDT helper init to default `stateful_decoding=False`
(`parakeet-stt-daemon/src/parakeet_stt_daemon/model.py`) since it measurably improves
WER on the current bench set. Truncation remains and needs further alignment work.

## Status Update (2026-02-24, Streaming Default Tuning)

Grid sweep on the bench set (TDT helper, `stateful_decoding=False`) across
`chunk_secs ∈ {1.6, 2.0, 2.4}` and `right_context_secs ∈ {1.6, 2.0, 2.4}`:

- Best observed average WER: `0.346` at `chunk_secs=2.4`, `right_context_secs=1.6`
  (total buffer `4.0s`).
- Previous defaults (`chunk_secs=2.0`, `right_context_secs=2.0`) averaged `0.419`.

Action taken: updated `ServerSettings` defaults to the best-performing config
(`chunk_secs=2.4`, `right_context_secs=1.6`) in
`parakeet-stt-daemon/src/parakeet_stt_daemon/config.py`.

## Status Update (2026-02-24, Streaming Baseline Re-Verify)

Re-ran bench A/B on `bench_audio/` using current defaults
(`chunk_secs=2.4`, `right_context_secs=1.6`, TDT helper, `stateful_decoding=False`):

- Average offline WER: `0.142`
- Average streaming WER: `0.488`

Streaming remains truncated vs offline at the new defaults.

## Status Update (2026-02-24, Streaming Tail Pad Experiment)

Added optional streaming tail pad to help recover end-of-utterance truncation:
`PARAKEET_STREAMING_TAIL_PAD_SECS` (default `0.0`, no change unless set).

Pad sweep on the bench set (streaming only):

- `0.0s` → `0.535` average WER
- `0.2s` → `0.483`
- `0.4s` → `0.454`
- `0.6s` → `0.410`

Tail padding improves WER somewhat but remains far worse than offline and adds latency.
This remains an optional diagnostic lever, not a default change.

Extra debugging hook: `PARAKEET_STREAMING_DEBUG=1` logs chunk counts and pad sizes
during streaming finalize.

Action taken: documented and kept tail padding + debug logging as opt-in only; no
default changes beyond recording these results.

## Status Update (2026-02-24, Streaming Improvement Avenues)

Concise next options (tradeoffs included):

- LCS-based merge helper for chunk alignment to reduce boundary loss; tradeoff: more compute and alignment complexity.
- Increase chunk/total-buffer (or right-context) for more tail coverage; tradeoff: higher latency and resource use.
- Add tail padding or a short forced trailing-silence window before finalize; tradeoff: adds end-of-utterance lag.
- Mirror NeMo buffered inference utilities for TDT alignment (e.g., `decoder_timestamps_utils`); tradeoff: integration risk and extra code surface.

## Handoff For Next Agent (Atomic-Commit Continuation)

Merged in this lane (2026-02-23):

1. `c2d1420` — `fix(daemon): guard disconnect cleanup with session ownership`
   - Adds strict `require_session_match` semantics to `_cleanup_active_session(...)`.
   - `handle_websocket` disconnect path now snapshots active session id and uses match-required cleanup.
   - Adds lifecycle regressions for websocket disconnect interleaving and streaming send-failure rollback teardown.
2. `6462df3` — `feat(daemon): expose runtime truth and stage timing metrics`
   - Adds status fields: `effective_device`, `stream_helper_active`, `stream_fallback_reason`, `active_session_age_ms`, `last_audio_ms`, `last_infer_ms`, `last_send_ms`.
   - Tracks effective model device at load time and stream-helper fallback reasons.
   - Emits runtime truth at startup and richer completion telemetry.
3. `b7503c2` — `fix(daemon): align streaming helper with supported NeMo API`
   - Replaces non-existent `ChunkedRNNTInfer` with `FrameBatchChunkedRNNT` + `AudioFeatureIterator`.
   - Adapts finalize() to frame-reader-based NeMo streaming call pattern.
   - `--check` and startup logs report explicit helper truth (ACTIVE/INACTIVE + reason).
   - Startup log promoted to WARNING when streaming enabled but helper inactive.
4. `759c107` — `test(daemon): cover streaming helper active/fallback truth paths`
   - 11 tests covering streaming truth state matrix and observability contracts.

Remaining highest-priority execution order:

1. Streaming quality validation: run short dictation sampling with streaming enabled to verify `FrameBatchChunkedRNNT` output quality versus offline fallback. Consider `BatchedFrameASRTDT` if TDT-specific alignment handling is needed for quality.

Non-negotiable constraints for continuation:

- Keep reliability-first behavior and protocol compatibility unless explicitly approved.
- Preserve cleanup ownership invariant introduced in `c2d1420`; do not reintroduce unscoped disconnect cleanup.
- Keep commits atomic by fix area; do not combine streaming integration + protocol taxonomy in one commit.

Suggested next atomic commits:

1. `docs(runtime): document streaming quality validation results` (state doc + spec update if needed).

Verification commands to run after each commit:

- `cd parakeet-stt-daemon && uv run pytest -q tests/`
- `cd parakeet-stt-daemon && uv run ruff check .`
- `cd parakeet-stt-daemon && uv run ruff format --check .`
- `cd parakeet-stt-daemon && ty check .`
- `cd parakeet-stt-daemon && uv run --with pyright pyright src/parakeet_stt_daemon/ tests/`

Current highest-priority unresolved items for follow-up agents:

1. Streaming quality gate: validate `FrameBatchChunkedRNNT` output quality on GPU via manual dictation sampling; if TDT alignment handling is needed, switch to `BatchedFrameASRTDT` (requires `tokens_per_chunk` and `delay` computation).

## Scope and Canonical Sources

Per request, this report treats code/runtime as source of truth and docs as secondary drift signals.

Primary evidence sources:

- Daemon code: `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/model.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/audio.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/__main__.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/messages.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/session.py`
- Runtime packaging: `parakeet-stt-daemon/pyproject.toml`, `parakeet-stt-daemon/uv.lock`
- Runtime surfaces: `scripts/stt-helper.sh`, `deploy/parakeet-daemon.service`, `parakeet-ptt/src/protocol.rs`
- Local runtime checks:
  - `uv run ruff check .` -> pass
  - `uv run pyright` -> pass
  - `uv run parakeet-stt-daemon --check` and `--check --no-streaming`
  - `nvidia-smi`
  - torch/nemo version probes
- Upstream package state:
  - PyPI: `nemo-toolkit` latest and metadata
  - PyPI: `torch` latest and metadata
  - PyTorch wheel index (`download.pytorch.org`) for `cu130`

## Current Runtime Snapshot (Observed)

Date of local checks: 2026-02-23.

- GPU: `NVIDIA GeForce RTX 5060 Ti`, driver `580.119.02`, CUDA runtime reported by `nvidia-smi` as `13.0`
- Installed daemon runtime:
  - `torch 2.9.1+cu128` (`torch.version.cuda == 12.8`, `torch.cuda.is_available == True`)
  - `nemo_toolkit 2.5.3`
  - `fastapi 0.121.3`, `uvicorn 0.38.0`, `pydantic 2.12.4`
- `parakeet-stt-daemon --check` with default streaming path showed:
  - model loads and warms on CUDA
  - warning: missing `cuda-python` reduces CUDA graph decode optimization
  - warning: streaming helper import path fails (`ChunkedRNNTInfer` cannot import), offline fallback used

Important lockfile facts:

- `uv.lock` resolves `torch` at `2.9.1` (`parakeet-stt-daemon/uv.lock:2840`, wheels at `parakeet-stt-daemon/uv.lock:2868` and `parakeet-stt-daemon/uv.lock:2869`)
- CUDA runtime libs in lock include `nvidia-cuda-runtime-cu12` (`parakeet-stt-daemon/uv.lock:1559`)
- `nemo-toolkit` entry present at `parakeet-stt-daemon/uv.lock:1351`

## Findings (Ranked)

Note: findings below represent the pre-hardening snapshot. Use the status update above as canonical state for what is already resolved.

## Critical

1. Missing active-session cleanup on websocket disconnect/error
- Evidence:
  - disconnect path only logs and returns (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:102`, `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:103`)
  - no `audio.stop_session_with_streaming()` or `sessions.clear(...)` in disconnect handler
  - audio callback keeps appending while `_session_active` true (`parakeet-stt-daemon/src/parakeet_stt_daemon/audio.py:144`, `parakeet-stt-daemon/src/parakeet_stt_daemon/audio.py:145`)
- Why it matters:
  - orphaned session state can keep accumulating in-memory chunks after client disconnect.
  - this can degrade performance and eventually pressure memory.
- Fix direction:
  - add a unified `_cleanup_active_session(reason: str)` called from disconnect and exception paths.
  - cleanup should atomically stop audio session capture, stop stream drain task, clear session manager state, and null `self._active_stream`.
- Acceptance criteria:
  - forced websocket disconnect during listening leaves `sessions_active=0` and no continued growth in captured buffer.

2. CLI settings merge overrides env booleans unintentionally
- Evidence:
  - `_build_settings` always sets `status_enabled`/`streaming_enabled` from argparse booleans (`parakeet-stt-daemon/src/parakeet_stt_daemon/__main__.py:89`, `parakeet-stt-daemon/src/parakeet_stt_daemon/__main__.py:90`)
  - parser only defines `--no-status` / `--no-streaming`; default values are `True` (`parakeet-stt-daemon/src/parakeet_stt_daemon/__main__.py:45` to `parakeet-stt-daemon/src/parakeet_stt_daemon/__main__.py:56`)
  - local proof:
    - `PARAKEET_STREAMING_ENABLED=false` with no CLI flags still produced `streaming_enabled True` via `_build_settings`
    - direct `ServerSettings()` respected env and returned `False`
- Why it matters:
  - operator intent via env is silently ignored; this is configuration drift and can explain "why is streaming on?" incidents.
- Fix direction:
  - parse bool flags with `default=None` and only set kwargs when explicitly passed.
  - or move merge logic to a dedicated precedence function with tests.
- Acceptance criteria:
  - env values apply when CLI flags absent; explicit CLI flags override env.

## High

3. Streaming path is effectively degraded on current stack
- Evidence:
  - code imports `ChunkedRNNTInfer` (`parakeet-stt-daemon/src/parakeet_stt_daemon/model.py:180`)
  - runtime check error: cannot import `ChunkedRNNTInfer` from `nemo.collections.asr.parts.utils.streaming_utils`
  - helper falls back silently to offline path (`parakeet-stt-daemon/src/parakeet_stt_daemon/model.py:205`)
  - introspection of `streaming_utils` found classes like `FrameBatchChunkedRNNT`, not `ChunkedRNNTInfer`
- Why it matters:
  - roadmap items requiring genuine streaming behavior are blocked; current behavior is effectively offline-finalize.
- Fix direction:
  - align implementation with current NeMo streaming API (or pin to a known API-compatible NeMo version explicitly).
  - expose whether streaming helper is truly active through `/status` and startup logs as a hard signal.
- Acceptance criteria:
  - streaming mode uses actual chunked helper in production checks; no fallback warning on supported stack.

4. Error code taxonomy conflates distinct failure modes
- Evidence:
  - unknown session on stop emits `SESSION_BUSY` (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:152`)
  - explicit abort also emits `SESSION_BUSY` (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:213`)
  - allowed codes are limited (`parakeet-stt-daemon/src/parakeet_stt_daemon/messages.py:92`)
- Why it matters:
  - client-side UX and debugging cannot reliably distinguish stale stop, intentional abort, and real busy contention.
- Fix direction:
  - extend codes with `SESSION_NOT_FOUND`, `SESSION_ABORTED`, `INVALID_REQUEST`, keep backward-compat fallback mapping in client.
- Acceptance criteria:
  - each server failure class maps to one deterministic code; client metrics can segment correctly.

5. Start-session failure handling is not transactional
- Evidence:
  - session state is allocated before downstream start steps (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:124`)
  - subsequent calls (audio/session stream setup/send response) are not wrapped in rollback guard
- Why it matters:
  - if any post-allocation step fails, session manager can remain in non-idle state until separate cleanup.
- Fix direction:
  - wrap start pipeline with try/except and rollback (`sessions.clear`, stream cleanup) on any failure after allocation.
- Acceptance criteria:
  - injected failure in start path returns error and leaves daemon idle.

## Medium

6. `/status` omits key truth and may report device intent instead of effective state
- Evidence:
  - `gpu_mem_mb` hardcoded `None` (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:229`)
  - `device` comes from config, not effective resolved device (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:230`)
- Why it matters:
  - operations cannot validate actual runtime mode (`cuda` requested but maybe running cpu fallback).
- Fix direction:
  - persist resolved model device in server state and expose it.
  - expose `stream_helper_active`, `last_session_ms`, and optional `gpu_mem_mb` when torch cuda present.

7. Helper defaults force non-streaming daemon start
- Evidence:
  - helper start path hardcodes `--no-streaming` when launching daemon (`scripts/stt-helper.sh:580`)
- Why it matters:
  - even after daemon streaming fixes, default operator path bypasses streaming.
- Fix direction:
  - decide explicit policy: keep offline as reliability default (with rationale), or add opt-in helper profile for validated streaming.

8. Offline transcription path does temp-file roundtrip per finalization
- Evidence:
  - writes wav temp then transcribes then deletes (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:268` to `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:274`)
  - streaming fallback also writes temp file (`parakeet-stt-daemon/src/parakeet_stt_daemon/model.py:218` to `parakeet-stt-daemon/src/parakeet_stt_daemon/model.py:226`)
- Why it matters:
  - extra filesystem I/O adds avoidable tail latency and complexity.
- Fix direction:
  - benchmark alternatives (in-memory buffer pathway if supported, or tmpfs `/dev/shm` fallback).

9. No daemon tests currently present
- Evidence:
  - no `tests/` files found under `parakeet-stt-daemon`
  - `uv run pytest -q` failed because `pytest` is not installed in dev group
- Why it matters:
  - high-risk behavioral paths (session cleanup/config precedence/protocol mappings) are unguarded.
- Fix direction:
  - add `pytest` + focused unit/integration tests for session lifecycle and protocol surface.

## Low

10. Dependency packaging surface is heavier than intended and comments are misleading
- Evidence:
  - base dependencies already include `nemo-toolkit[asr]` (`parakeet-stt-daemon/pyproject.toml:19`)
  - `inference` extra repeats `nemo-toolkit[asr]` and adds `torch` (`parakeet-stt-daemon/pyproject.toml:23` to `parakeet-stt-daemon/pyproject.toml:26`)
- Why it matters:
  - "without GPU stack" installation semantics are not truly represented by package metadata.
- Fix direction:
  - split core protocol/server deps from inference deps if lightweight mode is still a goal.

## Performance and UX Opportunities

High-impact improvements once critical/high issues are addressed:

1. Add server-side per-stage timing metrics
- capture: `audio_stop_ms`, `finalize_ms`, `infer_ms`, `send_ms`
- expose via logs and optional `/status` summary.

2. Add session safety rails
- max session duration timeout (daemon-enforced) to prevent infinite capture.
- cap per-session buffered samples and fail fast with explicit error code.

3. Improve operator UX from daemon truth
- explicit startup line: `streaming_mode=requested`, `streaming_helper=active|fallback_offline`.
- explicit effective device in startup and status (`requested=cuda`, `effective=cpu|cuda`).

4. Protocol ergonomics
- add precise error classes while preserving backward compatibility at client parse layer.
- optional correlation ID or monotonic sequence for easier cross-log joining.

## GPU Stack Analysis (Does It Still Hold?)

## What still holds

- Current stack works for offline GPU inference:
  - CUDA available, model loads on GPU, warmup works.
- Existing torch/nemo combination is operational for baseline final-result transcription.

## What no longer holds cleanly

1. Streaming helper assumption in code does not hold on this runtime.
- Implementation expects `ChunkedRNNTInfer`, but installed NeMo surface exposes different helper classes.
- Result: silent fallback to offline path while "streaming enabled" may appear true.

2. Installation guidance around `nightly/cu130` is stale.
- Upstream PyTorch wheel index currently has stable `+cu130` wheels (including cp311 manylinux x86_64).
- Latest torch on PyPI is newer than lockfile (`2.10.0` vs locked `2.9.1`).

3. Optimization gap flagged by runtime
- NeMo warns `cuda-python` is missing, disabling CUDA-graph conditional path and reducing decode speed.

## Recommendation

Treat stack update as a two-lane decision:

- Lane A (stability-first immediate):
  - keep current lock, fix daemon API integration and lifecycle bugs first.
  - make streaming status truthful and explicit.
  - decide whether to add `cuda-python>=12.3` after controlled benchmark.

- Lane B (stack refresh track, parallel):
  - evaluate upgrade path to latest torch/nemo combination with explicit compatibility matrix.
  - validate streaming API integration against current NeMo helpers.
  - benchmark and compare p50/p95 finalize latency, GPU memory, and session failure rate.

## Public Interface and Type Changes Recommended

These are suggested for next implementation cycle:

1. Error code enum extension in daemon protocol
- from: `SESSION_BUSY|AUDIO_DEVICE|MODEL|UNEXPECTED`
- to include: `SESSION_NOT_FOUND`, `SESSION_ABORTED`, `INVALID_REQUEST`, `SESSION_TIMEOUT`

2. Status payload enrichment
- add: `effective_device`, `stream_helper_active`, `last_error_code`, `gpu_mem_mb` (if available), `active_session_age_ms`.

3. Configuration precedence contract
- codify: `CLI explicit > ENV > defaults`
- add tests to lock behavior.

## Suggested Implementation Sequence

Phase 0 (urgent hardening, 1-2 days)

1. Implement disconnect/error cleanup invariant in daemon session lifecycle.
2. Fix CLI/env precedence for boolean flags.
3. Add start-session transactional rollback.
4. Add tests for the above.

Phase 1 (streaming truthfulness + observability, 2-4 days)

1. Update streaming helper integration to current NeMo API or pin known API-compatible version.
2. Expose effective streaming/helper/device state in status and startup logs.
3. Add precise error code taxonomy and backward-compat client mapping.

Phase 2 (performance + UX surface, 2-3 days)

1. Add per-stage timing metrics and status summaries.
2. Evaluate temp-file path alternatives and apply if net-positive.
3. Add session guardrails (timeout and memory cap).

## Agent Delegation Matrix (Execution Planning)

Legend:
- Agent level `L1`: low-reasoning executor for small, deterministic edits.
- Agent level `L2`: medium-reasoning builder for cross-file behavior changes.
- Agent level `L3`: high-reasoning/senior agent for high-stakes architecture or protocol changes.
- Agent level `R`: research-only agent (no code mutation expected).

| ID | Action Item | Effort | Stakes | Best Agent Level | Planning Needed | Parallelization Note |
| --- | --- | --- | --- | --- | --- | --- |
| A1 | Disconnect/error cleanup invariant in daemon session lifecycle | M | High (reliability, leak risk) | L3 | Medium | Can run in parallel with A2 and B1; merge carefully with A3 (same files). |
| A2 | Fix CLI/env precedence for boolean flags | S | Medium | L2 | Low | Safe parallel lane; mostly isolated to `__main__.py` + tests. |
| A3 | Add start-session transactional rollback | M | High (state machine integrity) | L3 | Medium | Can run alongside A2; coordinate with A1 due overlap in `server.py`. |
| A4 | Add tests for A1-A3 | M | High (regression prevention) | L2 | Medium | Best started after A1-A3 API shape stabilizes; can split by module. |
| B1 | Update streaming helper integration or pin compatible NeMo version | L | High (feature correctness + stack risk) | L3 | High | Run in dedicated lane with R1 research; avoid parallel edits to model stack files by others. |
| B2 | Expose effective streaming/helper/device state in status + startup logs | M | Medium | L2 | Low | Can run in parallel with B1 if interface contract is pre-agreed; otherwise do after B1. |
| B3 | Add precise error-code taxonomy + backward-compatible client mapping | M | High (wire contract) | L3 | High | Requires coordinated daemon+ptt changes; keep as single owner lane. |
| C1 | Add per-stage timing metrics and status summaries | M | Medium | L2 | Medium | Parallel-safe after A1-A3 land; avoid conflicts with B2 status payload changes. |
| C2 | Benchmark and reduce temp-file finalization overhead | M | Medium | L3 | Medium | Can run parallel to C1 if one owner handles perf harness and one handles implementation. |
| C3 | Add session guardrails (timeout + memory cap) | M | High (user-facing behavior, fail-fast policy) | L3 | Medium | Can run after A1-A3; coordinate with metrics/testing lane for thresholds and assertions. |
| R1 | GPU stack compatibility/dependency refresh research (parallel prompt) | M | High (dependency/infra decisions) | R | High | Fully parallel now; should finish before B1 implementation freeze. |

## Parallel Workstreams for Orchestration

Recommended concurrent lanes:

1. Lane Core-Hardening: `A1 + A3` (single senior owner), then `A4` test backfill.
2. Lane Config-Sanity: `A2` (can start immediately, low coupling).
3. Lane Stack-Research: `R1` (external deep research, no repo mutations).
4. Lane Observability: `B2 + C1` (start once status schema draft is agreed).
5. Lane Protocol: `B3` (single owner; start after core hardening stabilizes).
6. Lane Perf/Guards: `C2 + C3` (after core hardening and baseline metrics exist).

Critical sequencing constraints:

- Complete `A1/A3` before broad soak tests and before declaring stability baseline.
- Complete `R1` before finalizing `B1` dependency/API choices.
- Complete `B3` before releasing any client/daemon pair broadly, due wire-contract implications.

## Execution Board (Delegation + Orchestration)

Use this as the concrete multi-agent runbook.

### Owner Slots

- `Owner-S1` (Senior): daemon lifecycle/state integrity work (`A1`, `A3`, `C3`).
- `Owner-M1` (Mid): config/observability/status payload work (`A2`, `B2`, `C1`).
- `Owner-S2` (Senior): streaming integration + stack decisions (`B1`, `C2`).
- `Owner-S3` (Senior): protocol contract work across daemon/client (`B3`).
- `Owner-R1` (Research): GPU stack deep validation (`R1`, report-only).

### Branch + PR Plan

| Order | Branch | Owner | Scope | PR Title (suggested) | Depends On |
| --- | --- | --- | --- | --- | --- |
| 1 | `agent/a2-config-precedence` | Owner-M1 | `A2` + tests | `fix(daemon): respect env precedence for status/streaming flags` | none |
| 2 | `agent/a1-a3-session-hardening` | Owner-S1 | `A1` + `A3` + tests | `fix(daemon): enforce session cleanup and transactional start invariants` | none |
| 3 | `agent/r1-gpu-stack-research` | Owner-R1 | `R1` research artifact only | `docs(gpu): compatibility matrix and upgrade recommendation` | none |
| 4 | `agent/b2-c1-observability` | Owner-M1 | `B2` + `C1` | `feat(daemon): expose effective runtime state and stage timing metrics` | PR-1 + PR-2 |
| 5 | `agent/b1-streaming-integration` | Owner-S2 | `B1` (API integration or pinning) | `fix(daemon): align streaming helper implementation with current nemo api` | PR-2 + PR-3 |
| 6 | `agent/b3-protocol-taxonomy` | Owner-S3 | `B3` daemon + ptt | `feat(protocol): add explicit session/error taxonomy with client compatibility` | PR-2 |
| 7 | `agent/c2-c3-perf-guardrails` | Owner-S2 | `C2` + `C3` | `feat(daemon): add session guardrails and finalize-path performance improvements` | PR-4 + PR-5 |

### Merge Gates (Per PR)

All PRs must pass:

- `cd parakeet-stt-daemon && uv run ruff check .`
- `cd parakeet-stt-daemon && uv run pyright`

PR-specific gates:

- PR-1/PR-2/PR-7:
  - new/updated daemon lifecycle tests pass
  - forced-disconnect cleanup scenario validated
- PR-4:
  - `/status` fields verified against effective runtime state
  - timing metrics emitted without hot-path stalls
- PR-5:
  - `--check` shows streaming helper active on chosen supported stack OR explicit pin rationale documented
  - no silent fallback when streaming is expected active
- PR-6:
  - daemon and `parakeet-ptt` protocol compatibility tests pass together
  - backward-compat behavior verified for unknown/new fields

### Conflict and Integration Rules

1. `Owner-S1` is lock owner for `server.py` lifecycle sections until PR-2 merges.
2. `Owner-S2` is lock owner for `model.py` streaming stack sections until PR-5 merges.
3. `Owner-S3` is lock owner for message schemas after PR-6 opens.
4. No force-push after review starts unless requested by reviewer.
5. Rebase branches onto `main` before final CI run to avoid false green.

### Fast Timeline (If Parallelized)

- Day 1: PR-1, PR-2, PR-3 open in parallel.
- Day 2: merge PR-1/PR-2; start PR-4 and PR-6.
- Day 3: merge PR-3/PR-4/PR-6; start PR-5 and PR-7.
- Day 4: merge PR-5/PR-7; run soak validation and produce release notes.

## Test Matrix to Require Before Merge

1. Session lifecycle
- start -> stop success
- start -> disconnect before stop
- start -> abort
- stale stop ID

2. Config precedence
- env-only control of `PARAKEET_STREAMING_ENABLED` and `PARAKEET_STATUS_ENABLED`
- explicit CLI override behavior

3. Streaming mode truth
- helper available path reports active
- helper unavailable path reports fallback explicitly

4. Protocol and client compatibility
- new error codes parsed in Rust client and surfaced correctly
- unknown server fields tolerated by client

5. Stability soak
- repeated dictation cycles (>=200) with no active-session leaks and stable memory trend

## Deep Research Output

See the finalized GPU stack research report in `GPU Inference Stack Audit and Upgrade Proposal for parakeet-stt-daemon-deep-research-report.md` for the recommendation, compatibility matrix, migration/rollback commands, benchmark protocol, and risk register.

## Appendix: Key Code References

The canonical source list is in "Scope and Canonical Sources" above. These are only the
line-level hotspots referenced by top-ranked findings:

- Session disconnect handling gap: `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:102`
- Transcription temp-file path: `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py:266`
- Streaming helper import/fallback path: `parakeet-stt-daemon/src/parakeet_stt_daemon/model.py:180`, `parakeet-stt-daemon/src/parakeet_stt_daemon/model.py:205`
- Helper default forcing offline mode: `scripts/stt-helper.sh:580`

## Handoff For Next Agent (Post SA3/SA7/SA8/SA9/SA10)

Current head includes these lane commits:

1. `40b1081` — `feat(streaming): prototype conformer partial path behind flag`
2. `bff850b` — `build(runtime): upgrade NeMo to 2.6.2 with cuda-python 13`
3. `dc3ba80` — `feat(vad): add opt-in silero tail trimming with rms fallback`
4. `2fa585a` — `docs(state): record SA9 decision and SA10 token sweep results`

All SA checklist items are now marked `DONE`, with two explicit deferred/follow-up outcomes:

- SA8 follow-up: conformer partial stream path is wired and telemetry-complete but currently self-disables at runtime on first step (`partial_stream_failed:TypeError`); default finalize path remains safe.
- SA9 follow-up: `2.7.x` upgrade is deferred until stable upstream `2.7.x` release is available and validated against transducer CUDA-graph decode behavior.

### Remaining Work Queue (Practical)

1. **SA8 Phase 2 hardening**
   - Make `conformer_stream_step()` partial path produce stable partials (no first-step self-disable).
   - Keep behind `PARAKEET_EXPERIMENTAL_CONFORMER_PARTIALS` until proven.
   - Add targeted runtime test/probe evidence in this doc before changing status semantics.

2. **SA10 deeper formula research (optional)**
   - Only if pursuing helper-only quality: investigate non-naive TDT-aware token budgeting (beyond scalar multipliers).
   - Keep stream-then-seal default untouched.

3. **SA9 revisit trigger**
   - Re-open only when upstream publishes stable `2.7.x` with relevant ASR/transducer fix notes.
   - Re-run the same offline gate envelope + streaming helper smoke matrix used in SA3.

### Methodology Contract (Must Keep)

- Small, atomic commits by fix area.
- Before/after evidence recorded in this state document during each lane.
- For each relevant code/dependency change, run full gates in `parakeet-stt-daemon`:
  - `uv run pytest -q tests/`
  - `uv run ruff check .`
  - `uv run ruff format --check .`
  - `ty check .`
  - `uv run --with pyright pyright src/parakeet_stt_daemon/ tests/`
- For offline-risk changes, run offline benchmark gate:
  - `uv run python check_model.py --bench-offline --device cuda --max-avg-wer 0.12 --max-p95-infer-ms 300 --max-p95-finalize-ms 300`
- Preserve offline safety contract:
  - no default offline behavior regressions,
  - any experimental streaming/VAD path remains opt-in until benchmark-validated.
