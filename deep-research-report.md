# GPU Inference Stack Audit and Upgrade Proposal for parakeet-stt-daemon

## Recommendation

**Go** for an immediate **staged update**, but treat it as a *streaming-fix + controlled dependency refresh*, not a “YOLO upgrade”. The current stack is already GPU-functional for offline dictation, yet it is *not actually streaming* in practice: the daemon tries to import `ChunkedRNNTInfer`, that import fails, and the daemon silently falls back to offline finalisation; additionally, the runtime warns that missing `cuda-python>=12.3` disables a CUDA-graphs optimisation path. fileciteturn0file2

A staged update is warranted because:

- The “streaming enabled” mode is currently misleading: the import failure disables the intended streaming path and blocks roadmap features that depend on real-time semantics. fileciteturn0file2
- Upstream now has a stable **PyTorch 2.10.0** release (released **21 Jan 2026**) and **NeMo 2.6.2** (released **6 Feb 2026**) that support Python ≥3.10, so Python 3.11 is in-scope. citeturn20view0turn21view0
- The PyTorch ecosystem has formalised a “wheel variants” approach for **CUDA 12.6 / 12.8 / 13.0** on Linux, which aligns with your host driver reporting CUDA runtime **13.0** via `nvidia-smi`. citeturn22view0 fileciteturn0file2
- NVIDIA’s NeMo documentation explicitly exposes a supported streaming step API (`conformer_stream_step`) that supports transducer (RNNT-style) “previous hypotheses” state and cache tensors—meaning you can integrate streaming without depending on a brittle internal helper import path. citeturn12view1

**Strong opinion (with love):** this is not a “dependency problem”; it’s an **API integration contract problem**. You should upgrade, but only after switching the daemon to a documented streaming API surface *and* adding hard signals to prevent “streaming-but-not-really” from ever happening again.

## Decision-ready compatibility matrix

The table below is designed to be *decision-ready*: pick a row, apply the pinned set, run the verification + benchmark protocol, then decide whether to promote to production.

| Profile | Torch | CUDA build strategy | NeMo | Python | Streaming viability | When to choose |
|---|---|---|---|---|---|---|
| Current (as observed) | 2.9.1 (+ cu128 in your lock/install) | CUDA 12.8 build | 2.5.3 | 3.11 | **Broken** (daemon import path fails → offline fallback) | Only as rollback baseline |
| Minimal-change “fix streaming only” | 2.9.1 | Keep CUDA 12.8 (cu128) | 2.5.3 | 3.11 | **Works** if you switch to NeMo’s documented streaming step API | Lowest dependency risk; fastest path to “real streaming” |
| Align-to-driver | 2.9.1 | Move to CUDA 13.0 wheel (cu130) | 2.5.3 | 3.11 | **Works** with the documented streaming API | If you want driver/runtime alignment without adopting newest torch |
| Recommended stable upgrade | **2.10.0** | Prefer CUDA 13.0 (cu130) on this host | **2.6.2** | 3.11 | **Works** with the documented streaming API; likely fewer streaming bugs | Best balance of recency + stability; reduces drift you already note |
| “Latest-but-riskier” | 2.10.0 + variant workflow | Wheel-variant auto-selection | 2.6.2 | 3.11 | Works, but packaging behaviour is newer | Only if you want uv “wheel variants” behaviour in prod |

Version and compatibility basis:
- Torch 2.10.0 release date and supported Python versions are shown in the PyPI metadata for `torch`. citeturn20view0
- PyTorch 2.9.1 explicitly publishes wheels for **CUDA 12.6 / 12.8 / 13.0** via the official `download.pytorch.org` wheel indices. citeturn18search0
- PyTorch 2.10.0 wheel variants officially support **CUDA 12.6 / 12.8 / 13.0** on Linux. citeturn22view0
- NeMo 2.6.2 requires Python ≥3.10 and PyTorch ≥2.6 (per PyPI metadata). citeturn24view0
- Your current daemon runtime snapshot (Torch 2.9.1+cu128, NeMo 2.5.3, streaming import failure, cuda-python warning) is captured in your system audit note. fileciteturn0file2

## Correct current NeMo streaming API and how the daemon should integrate

### The API surface you should target

Instead of importing private-ish helpers from `nemo.collections.asr.parts.utils.streaming_utils`, use NeMo’s documented ASR streaming step API:

- `conformer_stream_step(...)` simulates a forward step with caching for streaming, and explicitly accepts:
  - cache tensors (`cache_last_channel`, `cache_last_time`, and `cache_last_channel_len`)
  - `previous_hypotheses` **for RNNT models**, i.e., transducer-style decoding state
  - and returns updated caches plus hypotheses/transcription outputs. citeturn12view1
- `transcribe_simulate_cache_aware_streaming(...)` exists, but it is file-path-based and aimed at simulation; it’s useful for offline regression tests, not for a live microphone websocket daemon. citeturn12view2

### Integration mapping to parakeet-stt-daemon

Your current daemon behaviour is: start session → buffer audio → stop → write temp WAV → transcribe offline; “streaming” is configured but actually falls back to offline due to the import failure. fileciteturn0file2

A robust integration is to introduce an explicit **streaming session state machine** owned by the daemon session:

- **State that must live per websocket session**
  - `cache_last_channel: torch.Tensor | None`
  - `cache_last_time: torch.Tensor | None`
  - `cache_last_channel_len: torch.Tensor | None`
  - `previous_hypotheses: list[Hypothesis] | None` (NeMo’s hypothesis type is referenced by the API doc) citeturn12view1
  - `text_aggregator`: a dedup/merge layer to reconcile overlaps (see below)

- **Call pattern per audio chunk**
  1. Convert PCM to the model’s expected input representation.
  2. Call `model.conformer_stream_step(...)`, passing prior caches and `previous_hypotheses`.
  3. Store returned caches + returned `best_hyp` / decoded text.
  4. Optionally emit interim transcript events or only update internal state (to preserve websocket protocol compatibility).

- **Finalisation**
  - Flush remaining buffered frames by calling one final `conformer_stream_step` with the last chunk and using returned hypotheses/text as the authoritative final, rather than writing a temp WAV and running an offline transcribe. This eliminates the “temp-file roundtrip” tail latency you already flagged. fileciteturn0file2
  - Keep the offline WAV finalisation as an explicit fallback path for safety.

### Overlap/duplication handling

Transducer buffered streaming commonly needs *merge logic* to handle repeated text across overlapping chunks. Your upstream NeMo docs list a “Buffered Transducer inference with LCS Merge” tutorial, which is an explicit signal that some form of string merge is expected in practice. citeturn9view0

Because you are preserving websocket protocol compatibility, the safest default is:

- Do not emit interim tokens by default.
- Still perform streaming decode internally to reduce stop-latency and enable future interim results.
- On finalisation, merge chunk outputs conservatively (LCS-style) and run a basic sanity check (no runaway duplication).

This is the least risky way to get “real streaming semantics” without changing your client contract.

## cuda-python assessment

### Benefit

You have a runtime warning that missing `cuda-python>=12.3` disables a CUDA-graphs optimisation path. fileciteturn0file2 That warning is plausibly meaningful because:

- CUDA graphs are a standard way to reduce CPU overhead and stabilise latency by capturing and replaying GPU work; NVIDIA’s documentation shows Python CUDA graph capture flows via `from cuda import cudart ...`, which is exactly the package surface provided by `cuda-python`. citeturn17search2
- NVIDIA’s NeMo user guide changelog explicitly references fixes related to “CUDA graphs” and streaming inference for RNNT/TDT (including “Fixed TDT streaming inference”). This increases the odds that your stack benefits from enabling the CUDA-graphs path, especially for streaming workloads where p95/p99 latency matters more than mean. citeturn7search10

### Safety

From PyPI metadata, `cuda-python` is now a metapackage with versioned subpackages, and recent releases exist in both 13.0.x and 13.1.x series. citeturn16search10 This reduces the operational risk compared with “random third-party CUDA bindings”, but you should still treat it as a performance optional, not a hard requirement for core correctness.

### Recommendation for this runtime

- Add `cuda-python` as an **optional dependency** (feature flag: “cuda_graphs_enabled”), not as a mandatory install requirement.
- Pin it to a **CUDA 13.x** line if you adopt cu130 torch builds, to avoid a weird “CUDA 13 torch + CUDA 12 cuda-python” mismatch. The exact micro-pin can be decided after a quick import/feature check in your verification script (below). citeturn16search10turn22view0

## Proposed dependency set and pinning strategy

### Production dependency set

Recommended production pins (for the “Recommended stable upgrade” matrix row):

- `torch==2.10.0` citeturn20view0
- `nemo-toolkit[asr]==2.6.2` citeturn21view0turn24view0
- `cuda-python` **optional**, gated behind a config flag; require `>=12.3` as the lowest bound implied by your runtime warning, but prefer a CUDA 13.x pin when using cu130 torch. fileciteturn0file2 citeturn16search10

Everything else should be treated as “daemon core” (FastAPI/websocket/audio/logging), which your audit already enumerates as part of the working baseline. fileciteturn0file2

### Dev dependency set

Your repo already uses `ruff`, `pyright`, and formatting checks, and you explicitly call out that there is no test suite yet (which increases regression risk). fileciteturn0file2
Add **pytest** (+ one thin integration harness) as a dev dependency so streaming correctness and session cleanup invariants can be regression-tested.

### Constraints strategy (pyproject + lock + rollback)

Stability-first strategy:

- Keep **two named lock states** in git history:
  - `baseline`: the currently-known-good offline stack (your existing `uv.lock`).
  - `streaming-upgrade`: new lock after implementing `conformer_stream_step` integration + upgrading torch/nemo.

- Use **feature flags** in config (env + CLI) so you can decouple “upgrade dependencies” from “turn on streaming” and from “turn on cuda graphs”.

- Enforce “truth in status”:
  - Add `streaming_helper_active: bool` and `cuda_graphs_active: bool` into `/status` so operators can’t be gaslit by config drift (your audit already flags that streaming mode reporting is misleading today). fileciteturn0file2

## Migration plan with exact commands, verification, and rollback

The repo already demonstrates uv flows for CUDA indices and optional inference extras. fileciteturn0file0 The plan below keeps that style.

### Create an isolated test environment and lock

From the daemon directory:

```bash
# 1) New branch for the migration (critical for rollback)
git checkout -b chore/gpu-stack-2026-02

# 2) Fresh env (keep it isolated from your baseline venv name)
uv venv --python 3.11 .venv-gpu-2026-02
source .venv-gpu-2026-02/bin/activate

# 3) Sync deps (stable channel, CUDA 13.0 build)
# Use the torch CUDA index + "unsafe-best-match" when mixing PyPI + torch index.
uv sync --dev --extra inference \
  --index https://download.pytorch.org/whl/cu130 \
  --index-strategy unsafe-best-match

# 4) Lock the result for reproducibility
uv lock
```

Notes:
- Your README already uses the `--index` + `--index-strategy unsafe-best-match` pattern for CUDA installs. fileciteturn0file0
- If you want to *avoid* switching to cu130 initially, swap the index to cu128.

### Verification script and commands

Run these in the new environment:

```bash
# Sanity: torch + CUDA
python - <<'PY'
import torch
print("torch:", torch.__version__)
print("torch cuda available:", torch.cuda.is_available())
print("torch.version.cuda:", torch.version.cuda)
if torch.cuda.is_available():
    print("device:", torch.cuda.get_device_name(0))
PY

# Sanity: NeMo + ASR model load (no audio yet)
python - <<'PY'
import nemo
import nemo.collections.asr as nemo_asr
print("nemo:", nemo.__version__)
print("has ASRModel:", hasattr(nemo_asr.models, "ASRModel"))
PY

# Optional: cuda-python import gate (should not crash the daemon if absent)
python - <<'PY'
try:
    import cuda  # provided by cuda-python
    print("cuda-python: import OK")
except Exception as e:
    print("cuda-python: NOT available:", repr(e))
PY

# Daemon checks already used in-repo
uv run ruff check .
uv run pyright
uv run parakeet-stt-daemon --check
```

The last command (`--check`) is already part of your current operational checks and is known to expose the two key warnings (streaming helper failure and cuda-python missing). Post-migration it should instead report: “streaming active” (or explicit “streaming disabled by config”) and should not silently fall back. fileciteturn0file2

### Rollback commands

Rolling back should be boring (that’s the goal):

```bash
# 1) Hard reset to baseline branch / baseline lock
git checkout main
git reset --hard origin/main

# 2) Recreate baseline env (or reuse the old one if you keep it)
uv venv --python 3.11 .venv-baseline
source .venv-baseline/bin/activate
uv sync --frozen --dev
```

If you need to roll back on a host already running the daemon via the helper, keep the websocket protocol unchanged and make rollback simply a “restart with baseline lock + baseline wheel index”.

## Benchmark protocol

This protocol is designed to answer the practical question: “Did streaming + updated CUDA stack reduce tail latency and remain correct?”

### Corpus and profiles

Use a local-only corpus folder committed or shipped alongside (no network required at runtime), with:

- 20–50 short utterances (2–10s) spanning:
  - quiet speech, normal speech, fast speech
  - punctuation-heavy dictation (“comma… full stop…”)
  - numbers/dates
- 5–10 long-form clips (30–120s) to stress cache growth and tail latency.

Add at least one “real mic” profile:
- 5 live microphone captures (same speaker, consistent room), repeated across runs.

### Metrics

Measure *both* offline and streaming modes:

- **Cold start latency**
  - daemon process start → first model-ready status
- **Warm start latency**
  - start-session → first chunk processed
- **Finalisation latency distribution**
  - stop-session → final transcript delivered
  Capture p50/p95/p99.
- **GPU memory (VRAM)**
  - peak VRAM during steady streaming and during finalisation.
- **Failure rate**
  - any dropped websocket sessions, stuck session state, or audio buffer overruns.
- **Transcript correctness sanity**
  - not full WER (unless you want it), but:
    - basic keyword presence checks
    - duplication checks (common streaming overlap failure)
    - stable casing/punctuation behaviour

For streaming systems, it’s standard to separate “intermediate latency” vs “final latency”; NVIDIA’s ASR tooling documents this distinction for streaming clients, and adopting the same terminology will make your results easier to reason about. citeturn2search9

### Run procedure

- Run each scenario 30 times:
  - 10 cold (fresh daemon start)
  - 20 warm (daemon already loaded, model warmed)
- For each run:
  - log chunk duration, GPU name, torch/nemo versions, and whether CUDA graphs path was active.

## Risk register and mitigations

### Streaming correctness risks

**Overlap duplication / transcript drift**
Mitigation: implement conservative LCS-style merge for chunk outputs, and add a regression test corpus that asserts “no repeated 5-gram beyond threshold”. NeMo’s own tutorial list highlights LCS merge as a first-class concern for buffered transducer inference. citeturn9view0

**Unclear tensor expectations for `conformer_stream_step`** (raw waveform vs features and exact shapes)
Mitigation: implement a tiny internal “shape probe” that logs the tensor shapes on the first chunk; gate streaming enablement on passing a one-chunk dry-run. The method’s signature and cache semantics are documented, but the exact expected representation still needs validation in *your* model instance. citeturn12view1

### Operational risks

**“Streaming enabled” but fallback happens silently**
Mitigation: make streaming activation a hard boolean that is only `True` if the streaming path is successfully initialised; expose it in `/status` and in startup logs. Your audit already flags the current silent fallback. fileciteturn0file2

**Session cleanup leaks under websocket disconnect**
Mitigation: unify cleanup on disconnect/exception paths (stop audio capture + stop streaming drains + clear session state). This is ranked as your highest-risk stability gap. fileciteturn0file2

### Dependency risks

**CUDA wheel index mixing / resolution surprises**
Mitigation: always lock with `uv.lock`, deploy with `uv sync --frozen`, and keep index strategy consistent with your README’s CUDA install pattern. fileciteturn0file0

**cuda-python introduces instability**
Mitigation: keep it optional; record whether it is active in status; benchmark with and without. Its PyPI description indicates it’s now a structured metapackage, but frameworks can still hit edge cases when enabling CUDA graphs. citeturn16search10turn17search0

## Patch sketch for streaming integration

This is function/class-level guidance that maps directly to the failure you have today: `ChunkedRNNTInfer` import fails and triggers offline fallback. fileciteturn0file2

### Replace brittle helper import with a documented streaming engine

In your model layer (where you currently try to import `ChunkedRNNTInfer` and then fall back), implement:

- `class NemoConformerStreamingEngine:`
  - `__init__(self, model, *, chunk_ms, shift_ms, left_chunks, enable_cuda_graphs: bool)`
  - `start_session(self) -> StreamingState`
  - `push_chunk(self, state, pcm_f32: np.ndarray) -> tuple[StreamingState, str | None]`
  - `finalise(self, state) -> str`

Where `StreamingState` stores the caches and hypotheses described in NeMo’s `conformer_stream_step` signature. citeturn12view1

### Integration points in the daemon

- On websocket `start_session`:
  - allocate `StreamingState`
  - start audio capture
  - start stream-processing task if you do async chunk consumption

- On audio chunk callback / queue consumer:
  - call `push_chunk`
  - update `StreamingState`
  - **do not** change websocket protocol by default:
    - either store interim text internally
    - or emit optional “interim” events behind a feature flag (default off)

- On websocket `stop_session`:
  - call `finalise`
  - return final transcript using existing message types
  - ensure cleanup always runs (even on exceptions) — your audit shows this is currently not guaranteed. fileciteturn0file2

### Operational truth signals

Given your helper currently starts the daemon with `--no-streaming` by default, you should either:
- keep “offline by default” as a reliability-first choice, but add a separate `stt start --streaming` profile, or
- switch defaults after streaming correctness is validated. fileciteturn0file2turn0file0

Either way, add a startup log line like:
- `streaming=requested|disabled`
- `streaming_engine=active|inactive(reason=...)`
- `cuda_graphs=active|inactive(reason=...)`

This eliminates the current situation where the system appears configured for streaming while actually running offline. fileciteturn0file2