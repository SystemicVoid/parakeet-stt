# Deep Research: Closing the Streaming WER Gap on Parakeet TDT

## Purpose

This document is a research brief for investigating root-cause solutions to the streaming transcription quality gap observed with NVIDIA's Parakeet TDT model in a local dictation daemon. We need actionable technical findings — not workarounds — that address the fundamental reasons streaming inference produces dramatically worse output than offline inference on identical audio.

Secondary goal: identify improvements that benefit both the offline and streaming transcription paths (latency, accuracy, robustness).

---

## Problem Statement

### Measured Gap

On an 8-sample bench dataset of real dictation recordings (4–8 seconds each), using the same `nvidia/parakeet-tdt-0.6b-v3` model on the same GPU:

| Path | Helper Class | Avg WER | Notes |
|---|---|---|---|
| Offline | `model.transcribe()` | **0.071–0.142** | Complete, accurate output |
| Streaming (RNNT) | `FrameBatchChunkedRNNT` | **0.608** | Severe truncation |
| Streaming (TDT, stateful=True) | `BatchedFrameASRTDT` | **0.686** | Worse than RNNT helper |
| Streaming (TDT, stateful=False) | `BatchedFrameASRTDT` | **0.346–0.488** | Best streaming result, still 3–5x worse |

### Failure Mode

Streaming outputs are **consistently truncated at utterance end** (and occasionally at the start). This is not a random accuracy degradation — it is systematic boundary loss. The model produces good internal content but drops tokens near chunk/utterance edges.

### What We Already Tried (Eliminated as Sufficient Fixes)

1. **Switched from `FrameBatchChunkedRNNT` to `BatchedFrameASRTDT`** — TDT-specific helper. Improved WER from ~0.6 to ~0.45 but truncation persists.
2. **`stateful_decoding=False`** — measurably better (~0.45 vs ~0.69) but still truncated.
3. **Grid sweep of `chunk_secs` × `right_context_secs`** — best found: 2.4s chunk / 1.6s right context (total buffer 4.0s). Marginal improvement.
4. **`tdt_search_boundary` sweep** (4, 8, 12) — no meaningful improvement.
5. **Delay token experiments** — default mid-delay (38 tokens) was best; alternatives regressed.
6. **Tail silence padding** (`PARAKEET_STREAMING_TAIL_PAD_SECS` 0.0–0.6s) — improved WER from 0.535→0.410 but adds latency and remains far worse than offline.
7. **`pad_to_frame_len` toggle** — must stay enabled for TDT; disabling raises shape mismatches in `BatchedFeatureFrameBufferer`.
8. **Patched `_get_batch_preds`** to normalize decoder output via `_coerce_rnnt_texts` — fixed runtime errors but didn't improve quality.

---

## Technical Context

### Model

- **Name**: `nvidia/parakeet-tdt-0.6b-v3`
- **Architecture**: Encoder (FastConformer) + Decoder (TDT — Token-and-Duration Transducer)
- **Key difference from standard RNNT**: TDT predicts both tokens AND durations, allowing variable-length token emission per encoder frame. This fundamentally changes how streaming chunked decoding should work compared to standard RNNT models.
- **HuggingFace card**: `https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3`
- **Attention**: Modified to `rel_pos_local_attn` with `att_context_size=[256, 256]` (per HF card guidance)

### Runtime Stack

- **NeMo**: 2.5.3 (`nemo-toolkit[asr]>=2.5.3`)
- **PyTorch**: 2.9.1+cu128
- **GPU**: NVIDIA RTX 5060 Ti (16GB VRAM), CUDA 13.0 driver, CUDA 12.8 runtime
- **Python**: 3.11+ via uv

### Offline Path (Working Well)

```python
# Direct numpy array transcription — single model.transcribe() call
outputs = model.transcribe([audio_array], batch_size=1, verbose=False)
text = extract_text(outputs[0])
```

This calls the model's full encoder → decoder → greedy/beam search pipeline on the complete utterance. No chunking, no frame iteration. The model sees all context at once.

### Streaming Path (Degraded)

```python
# 1. Create helper
helper = BatchedFrameASRTDT(
    asr_model=model,
    frame_len=2.4,           # chunk_secs
    total_buffer=4.0,        # chunk_secs + right_context_secs
    batch_size=1,
    max_steps_per_timestep=5,
    stateful_decoding=False,
)

# 2. At finalize time, feed all accumulated audio at once
combined = np.concatenate(all_chunks)
# Optional delay padding
delay_pad_samples = int(delay * model_stride_secs * sample_rate)
combined = np.pad(combined, (0, delay_pad_samples))

# 3. Create frame iterator and transcribe
frame_reader = AudioFeatureIterator(
    combined, helper.frame_len, helper.raw_preprocessor,
    helper.asr_model.device, pad_to_frame_len=True
)
helper.set_frame_reader(frame_reader)
result = helper.transcribe(tokens_per_chunk, delay)
```

Key observation: **we are NOT doing true real-time streaming**. We accumulate all audio, then feed it through the chunked helper at finalize time. The chunked helper processes it frame-by-frame sequentially, but the full audio is available. This means the quality gap is entirely due to the chunked decoding approach, not due to missing future context.

### Token/Delay Computation

```python
model_stride = window_stride * subsampling_factor  # from model config
tokens_per_chunk = ceil(chunk_secs / model_stride)
delay = ceil((chunk_secs + (total_buffer - chunk_secs) / 2) / model_stride)
```

### NeMo Streaming Helper Class Hierarchy

From `nemo.collections.asr.parts.utils.streaming_utils`:
- `FrameBatchASR` — base class for frame-based batched ASR
- `FrameBatchChunkedRNNT(FrameBatchASR)` — RNNT-specific chunked streaming
- `BatchedFrameASRTDT(FrameBatchChunkedRNNT)` — TDT-specific extension

The helper internally:
1. Breaks audio into fixed-length frames via `AudioFeatureIterator`
2. Preprocesses each frame through the model's feature extractor
3. Runs encoder on each frame (with buffered context)
4. Runs `rnnt_decoder_predictions_tensor` per frame
5. Concatenates per-frame text predictions

---

## Research Questions

### Q1: Root Cause of TDT Streaming Truncation in NeMo

**Why does `BatchedFrameASRTDT` systematically truncate utterance boundaries?**

Specific angles to investigate:

- **TDT duration prediction vs chunked decoding**: TDT models predict durations (how many encoder frames to skip after emitting a token). When processing in fixed-size chunks, does the duration prediction at chunk boundaries cause the decoder to "skip over" tokens that would fall in the next chunk? Is the duration prediction being incorrectly interpreted at frame edges?

- **`tokens_per_chunk` and `delay` semantics for TDT**: These parameters control how many tokens the decoder emits per chunk and how many chunks the output is delayed. Are the standard RNNT formulas (`ceil(chunk_secs / model_stride)`) correct for TDT, or does TDT need different computation due to its variable-rate emission?

- **Frame boundary token loss**: When `BatchedFrameASRTDT._get_batch_preds` processes each batch, are tokens at the boundary of each frame being dropped because the decoder state doesn't carry over correctly between frames?

- **NeMo's own streaming evaluation**: Does NVIDIA's published work on Parakeet TDT include streaming evaluation results? What WER gap do they observe? What configuration do they use?

- **Known issues**: Are there open NeMo GitHub issues about TDT streaming quality with `BatchedFrameASRTDT`?

### Q2: Correct NeMo API for TDT Streaming

**Is `BatchedFrameASRTDT` the right approach, or is there a better NeMo API?**

- **`conformer_stream_step()`**: The existing GPU stack research report mentions this as an alternative to the helper classes. How does it work? Is it applicable to TDT models? What's its relationship to the `FrameBatchASR` helper hierarchy?

- **`model.transcribe()` with streaming config**: Can the standard `model.transcribe()` be configured for chunked/streaming operation via model config changes rather than using the helper classes? NeMo models have `cfg.decoding` settings — can these enable a streaming-compatible decode that preserves quality?

- **`FrameBatchMultiTaskAED`**: NeMo has other streaming helper variants. Are any of these better suited to TDT?

- **Cache-aware streaming**: Does NeMo support cache-aware (look-ahead) streaming for FastConformer encoders? If so, how should the model be configured for it, and does it require re-exporting or fine-tuning the model?

- **NeMo 2.6.x streaming improvements**: Has the NeMo team improved TDT streaming support in versions newer than 2.5.3? Are there release notes or PRs that address the quality gap?

### Q3: Alternative Streaming Architectures That Preserve Quality

**What patterns exist for near-offline-quality streaming with transducer models?**

- **Stream-then-seal / two-pass decoding**: Use chunked streaming for low-latency partial results, then run a full offline decode for the final result. This gives streaming UX with offline accuracy. What's the implementation pattern? What's the latency cost of the final offline pass?

- **Overlapping chunk decoding with LCS merge**: Process overlapping audio chunks independently (each with full context), then merge transcripts using longest common subsequence alignment. Published implementations? Optimal overlap ratio?

- **Sliding window with hypothesis stitching**: Process audio in sliding windows with overlap, keep running hypotheses, merge at utterance end. How does this compare to NeMo's built-in buffered approach?

- **Re-scoring / second-pass correction**: Use streaming output as draft, then rescore with a language model or the same acoustic model in offline mode. Can this be done cheaply?

- **Endpoint-triggered offline finalize**: Don't use streaming decoding at all for final results — use VAD/endpointing to detect utterance boundaries, then run offline `model.transcribe()` on each segment. What's the typical latency? How do production systems (e.g., Whisper-based streaming) handle this?

### Q4: GPU Stack Version Impact

**Would upgrading NeMo/PyTorch improve streaming quality?**

- **NeMo 2.6.x**: What streaming-related changes were made? Any TDT-specific fixes? Any new helper classes or APIs?
- **NeMo 2.7.x / latest**: Same questions for the latest release.
- **PyTorch 2.10.x**: Any inference improvements relevant to streaming ASR (e.g., better CUDA graph support, torch.compile for ASR models)?
- **`cuda-python` package**: Currently missing, NeMo warns about reduced CUDA graph decode optimization. What's the actual impact on inference speed? Would it help streaming more than offline?

### Q5: Improvements Benefiting Both Paths

**What changes would improve both offline and streaming transcription quality and latency?**

- **Voice Activity Detection (VAD) integration**: Using a lightweight VAD (Silero, WebRTC VAD, NeMo's built-in VAD) to trim silence before transcription. Impact on both latency and accuracy?

- **Attention window optimization**: The model uses `rel_pos_local_attn` with `att_context_size=[256, 256]`. Are there better settings for dictation-length utterances (typically 3–15 seconds)?

- **`torch.compile()` for NeMo ASR models**: Can the encoder/decoder be compiled for faster inference? Known compatibility issues?

- **CUDA graphs for inference**: NeMo mentions CUDA graph support when `cuda-python` is present. What's the speedup? Is it safe for streaming (stateful) decoding?

- **Quantization**: INT8/FP16 quantization of the Parakeet model for faster inference. NeMo support? TensorRT integration?

- **Batch inference for offline**: When processing a single utterance, is `batch_size=1` optimal? Would batching silence-separated segments help?

- **Better endpointing**: Currently using simple RMS-based tail silence trimming (`-40dB floor, 50ms windows`). What's the state of the art for local endpointing that could improve both latency (earlier cutoff) and accuracy (cleaner utterance boundaries)?

---

## Constraints

- **Local-only**: No cloud ASR APIs. All inference must run locally on a single RTX 5060 Ti (16GB VRAM).
- **Single-user dictation**: Not multi-speaker, not telephony. Utterances are typically 3–15 seconds from a desktop microphone.
- **Latency budget**: Offline finalize currently takes 46–71ms. Streaming finalize takes 135–210ms. Target: final result within 200ms of utterance end for either path.
- **Model**: `nvidia/parakeet-tdt-0.6b-v3` is the current model. Open to alternatives if they're materially better, but switching models is a larger decision.
- **Python/NeMo stack**: The daemon is Python-based using NeMo. Solutions should work within this ecosystem or have clear integration paths.

---

## Success Criteria for Research Output

1. **Root cause explanation**: A clear technical explanation of why TDT chunked decoding loses tokens at boundaries, grounded in NeMo source code or published analysis.
2. **Recommended approach**: A specific, implementable recommendation for how to achieve streaming transcription with WER within 2x of offline (target: < 0.20 WER on our bench set vs current 0.07–0.14 offline).
3. **Code references**: Links to NeMo source files, example scripts, published evaluation results, or working open-source implementations that demonstrate the recommended approach.
4. **Stack version recommendation**: Whether upgrading NeMo/PyTorch is necessary or helpful for the recommended approach, with specific version targets.
5. **Both-path improvements**: At least 2–3 concrete improvements that reduce latency or improve accuracy for both offline and streaming paths.

---

---

## Research Synthesis (2026-02-25)

This section records findings from two independent deep research passes (GPT and Gemini) run against the brief above. Raw reports archived at `research-archive/`. Findings are classified by confidence and converted to action items.

### Confirmed Root Causes

All three root causes below were independently identified across both reports and are grounded in NeMo source code, release history, and behavioral evidence from our own experiments.

#### RC1 — End-of-utterance delayed emission without an explicit drain (High Confidence)

The core failure. Buffered transducer decoders intentionally delay token emission so each token has sufficient right-context coverage before being committed. Tokens near the end of an utterance sit in the delay buffer and are only released when additional frames arrive after them. When the pipeline stops at the last real audio frame, those buffered tokens are never emitted — they are silently discarded.

Our tail-padding experiment directly confirms this: each additional 0.2s of silence (0.0 → 0.2 → 0.4 → 0.6s) monotonically improved streaming WER (0.535 → 0.483 → 0.454 → 0.410). The padding is acting as a crude proxy drain — feeding the extra frames needed to flush the buffer. This is behavioral proof of delayed emission without drain, not a tuning result.

**The root-cause fix is not more padding**. It is an explicit drain step in the finalize path: after the last real audio frame, feed N frames of blank/silence features sufficient to cover the configured delay window, forcing all pending tokens to become emit-eligible before collecting the hypothesis. Because we are not doing true real-time streaming at finalize time (the full audio is already in memory), this adds no user-perceived latency — it is extra GPU compute only.

Drain frame count: `N = ceil(delay × model_stride_secs × sample_rate)` — the same quantity already computed as `delay_pad_samples` in the current finalize path. The difference is that the current code computes it but only applies it as NumPy zero-padding to the waveform, not as a proper decoder drain pass. For the `conformer_stream_step()` path, drain should be implemented by running additional cache-aware steps on silence features until the hypothesis stabilizes.

#### RC2 — `tokens_per_chunk` overflow for TDT burst emission (High Confidence)

The formula `tokens_per_chunk = ceil(chunk_secs / model_stride)` was designed for standard RNNT's 1:1 frame-to-token emission rate. TDT's joint network simultaneously predicts tokens AND duration skips, enabling it to bypass 4–8 encoder frames per emission and then emit several tokens in rapid succession (amplified by FastEmit regularization). This creates dense bursts that can generate significantly more tokens within a single chunk's time window than the RNNT formula allows.

When a burst of speech falls at a chunk boundary and the burst generates more tokens than `tokens_per_chunk` permits, the excess tokens are silently clipped from the logit tensor in the `BatchedFrameASRTDT` inference loop. These clipped tokens are the missing final words.

This explains why grid-sweeping `chunk_secs` and `right_context_secs` only produced marginal improvements: the overflow is a function of emission density relative to the formula, not buffer size.

#### RC3 — `stateful_decoding` inheritance bug in `BatchedFrameASRTDT` (Medium-High Confidence)

The Gemini report identifies a specific constructor bug in NeMo's `BatchedFrameASRTDT`. The class signature accepts `stateful_decoding` but the `super().__init__()` call does not pass it through to the `FrameBatchASR` base class. The base class therefore always runs in stateless mode regardless of what the caller specified.

This explains the paradox in our benchmark: `stateful_decoding=True` produced WER 0.686, *worse* than `stateful_decoding=False` at 0.346. If the flag is silently dropped at the base class level, the TDT-specific code paths triggered by `stateful_decoding=True` may interact destructively with a base class that never actually preserves state. Setting `stateful_decoding=False` at least keeps the TDT path self-consistent.

**This needs verification against the installed NeMo source** (`nemo/collections/asr/parts/utils/streaming_utils.py`). If confirmed, this is a NeMo bug with a one-line fix in the daemon (or a patch to NeMo itself).

#### RC4 — Left context starvation (Medium Confidence)

The Parakeet TDT v3 model card recommends `left_context_secs=10.0` as the streaming baseline. Our `left_context_secs=10.0` is already set in config, but `left_context_secs` is passed to `FrameBatchASR` which manages it through internal frame buffering. Whether the actual buffer faithfully holds 10 seconds of history — vs truncating it due to `total_buffer_secs` — needs verification. Left context starvation will not cause hard truncation, but it will amplify instability at chunk boundaries and degrade WER beyond what the drain fix alone recovers.

### Recommended Architecture

Both reports converge on the same forward direction: abandon the `BatchedFrameASRTDT` / `FrameBatchChunkedRNNT` wrapper hierarchy and replace with one of the following two patterns, or both together.

#### Path A — Stream-Then-Seal (Highest Confidence, Lowest Risk)

Use `model.transcribe([audio_array])` for the final committed transcript, and use the streaming path only for partial/draft display:

1. Streaming (`conformer_stream_step()` loop): greedy decoding, produces partial hypotheses during dictation for real-time display.
2. On endpointing (VAD detects utterance end): run `model.transcribe([full_audio])` as the seal pass.

Offline finalize latency is 46–71ms, within the 200ms target budget. The seal pass sees the full utterance with complete context and cannot drop boundary tokens. This is the implementation pattern used in production streaming ASR systems.

This pattern is forward-compatible: once the `conformer_stream_step()` path is fixed and its accuracy validated, the seal pass can be disabled or reserved for low-confidence cases.

#### Path B — Cache-Aware Streaming (Higher Ambition, Root Fix for Streaming Quality)

Replace `BatchedFrameASRTDT` with a direct `conformer_stream_step()` implementation that:
1. Allocates cache tensors at session start: `cache_last_channel`, `cache_last_time`, `cache_last_channel_len` via `model.encoder.get_initial_cache_state(batch_size=1)`.
2. Allocates pre-encode cache: `model.encoder.streaming_cfg.pre_encode_cache_size`.
3. On each chunk: concatenates `cache_pre_encode` to the incoming features along the time axis, runs `conformer_stream_step()`, updates all cache tensors.
4. On finalize: runs additional silence frames as drain until hypothesis stabilizes, then returns the final text.

This approach processes only the new audio delta per step (no overlapping chunk redundancy), maintains perfect acoustic context continuity across chunk boundaries, and eliminates the LCS merge complexity entirely. TDT duration skipping works correctly because the encoder never experiences a state flush.

NeMo's FastConformer models with `rel_pos_local_attn` attention explicitly support this API.

### Action Items (Priority Order)

| ID | Item | Priority | Effort | Basis |
|---|---|---|---|---|
| SA1 | Verify `stateful_decoding` inheritance bug in NeMo source | P0 | XS | RC3 — free diagnostic, changes interpretation of all prior results |
| SA2 | Implement explicit EOU drain in finalize (not zero-pad waveform) | P0 | S | RC1 — root cause fix, no version dependency |
| SA3 | Upgrade NeMo to 2.6.2 (minimum, security + TDT streaming fix) | P1 | M | Multiple — upstream has explicit TDT streaming fixes |
| SA4 | Derive tail padding from model config, not constant | P1 | XS | RC1 — `hop_ms × shift_frames × sample_rate / 1000` ≈ 320ms for Parakeet at 16kHz |
| SA5 | Implement Stream-Then-Seal via `model.transcribe()` seal pass | P1 | M | Path A — highest-confidence WER target, composable with current code |
| SA6 | Install `cuda-python` | P2 | XS | Enables CUDA graph decode optimization; NeMo warns on every startup |
| SA7 | Integrate Silero VAD v6 for neural endpointing | P2 | M | Both paths — replaces fragile RMS threshold, improves both latency and boundary accuracy |
| SA8 | Prototype `conformer_stream_step()` cache-aware loop | P2 | L | Path B — root fix for streaming quality; required for true real-time partials |
| SA9 | Evaluate NeMo 2.7.x for CUDA Graphs for Transducer + decoder memory leak fixes | P3 | M | Gemini — more ambitious upgrade target after 2.6.2 validation |
| SA10 | Investigate `tokens_per_chunk` formula for TDT — compare with TDT-specific derivation | P3 | S | RC2 — may need to multiply by max_steps_per_timestep or otherwise account for burst |

### Report Agreement and Divergence

**Strong agreement across both reports:**
- EOU drain is the root cause of truncation (tail pad as proxy confirms this)
- `tokens_per_chunk` math is wrong for TDT burst emission
- Stream-Then-Seal with offline seal pass is the reliable path to <0.20 WER
- NeMo version upgrade is justified (upstream has TDT streaming fixes)
- Neural VAD is needed to replace RMS-based endpointing
- `conformer_stream_step()` is the correct long-term API

**Divergence and resolution:**
- *LCS merge*: GPT recommends LCS for chunk boundary stitching; Gemini argues LCS is mathematically broken for TDT's non-linear frame skips. Resolution: LCS is better than naive concatenation for RNNT-style overlapping windows but is not a substitute for fixing the drain. In a Stream-Then-Seal architecture, LCS is not needed at all for the final result.
- *NeMo version target*: GPT recommends 2.6.2; Gemini recommends 2.7.x. Resolution: 2.6.2 is the conservative, security-justified minimum. Evaluate 2.7.x after 2.6.2 validates. The CUDA Graphs for Transducer in 2.7.x are compelling for the streaming loop latency.
- *FP8 quantization*: Gemini recommends TensorRT Model Optimizer for FP8. GPT does not mention it. Resolution: this is a latency optimization, not a correctness fix. Defer until the quality gap is closed. The RTX 5060 Ti supports FP8 natively (Ada Lovelace 4th-gen Tensor Cores) so this is worth validating eventually.

### Tail Padding Formula (Correctness)

The current tail padding (`PARAKEET_STREAMING_TAIL_PAD_SECS`) is a heuristic constant. The correct value is derived from the model config:

```
padding_samples = hop_length_ms × shift_frames × 2 × (sample_rate / 1000)
```

For Parakeet at 16kHz with 10ms hop and 16 shift frames: `10 × 16 × 2 × 1 = 320ms = 5120 samples`.

This should be computed from `model.preprocessor.featurizer.hop_length` and `model.encoder.streaming_cfg` rather than set as a constant, ensuring correctness across model variants.

---

## Reference Links for Research

- NeMo ASR streaming utilities: `nemo/collections/asr/parts/utils/streaming_utils.py`
- NeMo ASR models: `nemo/collections/asr/models/`
- Parakeet TDT model card: `https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3`
- NeMo GitHub: `https://github.com/NVIDIA/NeMo`
- NeMo streaming tutorial/notebook: `https://github.com/NVIDIA/NeMo/blob/main/tutorials/asr/Streaming_ASR.ipynb`
- TDT paper: "Token-and-Duration Transducer for Speech Recognition" (NVIDIA, 2023)
- NeMo releases/changelog: `https://github.com/NVIDIA/NeMo/releases`
- FastConformer paper: "Fast Conformer with Linearly Scalable Attention for Efficient Speech Recognition"
- Parakeet TDT v3 blog/release notes from NVIDIA

---

## Existing Research in This Repo

- `deep-research-report.md` — GPU stack audit, dependency compatibility matrix, migration plan. Recommends `conformer_stream_step()` as alternative to helper classes.
- `deep-research-report-offlinevsstreaming.md` — UX/architecture analysis of streaming vs offline, stream→seal pattern, partial result protocol design.
- `STATE-OF-STT-DAEMON-SERVER-2026-02-23.md` — Full daemon state document with streaming validation logs, benchmark results, and tuning sweep data.
