# Closing the Streaming WER Gap on Parakeet TDT in NeMo

## Executive summary

Your “streaming” path is not suffering from ordinary model accuracy drift; it is suffering from *systematic boundary loss* (especially at utterance end) that is characteristic of **buffered/overlapped transducer decoding with delayed emission** when the pipeline is **not properly drained at the end** (or when its merge policy discards the boundary region). The diagnostic signature is that padding trailing silence improves WER but does not fully close the gap—because padding is acting as a crude “drain” mechanism, not a true fix. fileciteturn15file3L120-L320

Two additional factors amplify the gap for **TDT (Token-and-Duration Transducer)**:

1. **TDT’s duration-driven skipping** changes the relationship between “frames processed” and “tokens emitted,” which makes RNNT-derived heuristics like `tokens_per_chunk ≈ chunk_secs / stride` and fixed `delay` more likely to be mis-calibrated at chunk boundaries. citeturn3search8  
2. There is evidence that **TDT streaming support has had correctness bugs and version-dependent behavior** in NeMo’s streaming/buffered tooling (including explicit release notes calling out “TDT streaming inference fix,” and NVIDIA acknowledging a streaming bug and behavioral constraints in public discussions). citeturn19view0turn2search0

A practical path to a final transcript WER target of **<0.20** (≤2× offline on your bench set) is achievable by combining:

- a **correct end-of-utterance drain/finalize** (root-cause fix for systematic truncation),  
- **NeMo’s recommended Parakeet streaming configuration** (notably substantial left context), and  
- either **LCS-style hypothesis stitching** or a **stream-then-seal** architecture (partial results via streaming; final result via offline `transcribe()` on the utterance segment). citeturn22search0turn1search0

If you want *true* streaming that stays near offline quality without a second pass, the strongest forward-looking bet is to adopt a **streaming-trained / cache-aware streaming model** rather than forcing an offline-optimized checkpoint through a buffered decoder path that was not trained to be chunk-invariant. NeMo’s own documentation explicitly warns that streaming evaluation with an offline-trained model will degrade unless you use very large chunks. citeturn1search18

## Measurement recap and the failure mode you’re seeing

Your internal bench confirms a large accuracy gap between offline `model.transcribe()` and chunked frame-based decoding on identical audio, on the same machine and GPU. The key qualitative symptom is consistent **truncation at utterance end (and sometimes start)** rather than diffuse substitution errors, which strongly indicates a decoding/merge/finalize issue rather than “the model is worse in streaming.” fileciteturn15file3L180-L320

Your repository’s recorded A/B results (bench set of 8 short dictation clips) show:

- **Offline** average WER around **0.07–0.14**,  
- **Streaming** WER often **0.35–0.69** depending on helper and tuning,  
- Tail padding improves streaming WER (e.g., ~0.535 → ~0.410 when adding up to 0.6s) but remains far worse than offline. fileciteturn15file3L180-L360

This is a classic “the last words never make it out of the decoder” situation. In other words: the model *knows* the words; your streaming pipeline is failing to **emit** them.

## Root causes in buffered transducer streaming that produce boundary truncation

Buffered transducer streaming (RNNT/TDT) typically works by repeatedly decoding **overlapped windows** (chunk + context), then **stitching** outputs across windows. NeMo even calls out “Buffered Transducer inference with LCS Merge” as a dedicated concept/tutorial area—an explicit signal that merging/stitching is not incidental; it is essential. citeturn9view0

The most likely root causes given your symptom pattern are:

### Delayed emission without an end-of-utterance drain

Most buffered streaming decoders intentionally delay output so each emitted token has sufficient right context. Tokens near the end of the utterance therefore sit “in the delay buffer” and are only released after additional frames arrive. If the pipeline stops as soon as the last real audio frame is processed, **those delayed tokens never get emitted**.

Your own experiments provide strong behavioral evidence for this: adding tail padding improves WER, meaning you are effectively giving the pipeline extra frames to flush delayed tokens. fileciteturn15file3L300-L360

**Root-cause fix (not a workaround):** implement an explicit *drain* step in your finalize logic—feed enough “non-speech / blank” frames (or reduced-delay final decoding) to force emission of any delayed hypotheses, without requiring real-time waiting.

### Merge policy discarding chunk boundary regions

Many “middle-token” or “keep-only-the-stable-center” merge strategies discard boundary regions to avoid duplicated text from overlapping windows. This works in the middle of a stream because a boundary region of window *N* becomes the center of window *N+1*. At the start/end of an utterance, there is no prior/next window to “rescue” discarded regions, so you drop boundary tokens.

Your results—mostly good internal content but missing ends—match this pattern extremely well. fileciteturn15file3L180-L320

**Root-cause fix:** switch to a merge policy that is explicitly designed to preserve boundaries (e.g., LCS-based merge or hypothesis stitching that keeps the best suffix/prefix), and/or ensure the final window’s boundary region is not discarded (drain + “final window keep-right-edge” semantics).

### Context configuration mismatch against Parakeet’s recommended streaming parameters

The Parakeet-TDT v3 model card explicitly recommends streaming with a script and parameters that include **substantial left context** (example: `left_context_secs=10.0`, `right_context_secs=2.0`, `chunk_secs=2`). citeturn22search0

If your effective left context is much smaller (or implicitly constrained by how your helper constructs buffers), you will see instability at the start and punctuation/timing instability near the end. This won’t by itself explain *hard truncation*, but it will amplify it and keep WER elevated even after you fix draining.

## Why TDT makes chunked decoding more fragile than RNNT

TDT is not “RNNT but faster”; it changes the inference dynamics:

- TDT jointly predicts **tokens and durations**, and during inference it can use predicted durations to **skip encoder frames**, making it more efficient than frame-by-frame transducer decoding. citeturn3search8

This has two practical implications for your investigation:

### RNNT-derived `tokens_per_chunk` and `delay` heuristics can be wrong for TDT

If `tokens_per_chunk` is computed from frame stride as if emission rate were tightly coupled to frames, it can under-allocate the number of tokens that must be emitted for a chunk—especially when duration prediction allows bursts of emissions that don’t map 1:1 to frames.

That can manifest as:
- tokens not emitted in the chunk they “belong” to, and
- tokens being pushed into the delayed/boundary zone, which your merge step may then discard at utterance end.

This is consistent with your observation that tuning chunk/right-context helps only marginally, and that padding helps more (because it gives “extra runway” for the delayed emissions to surface). fileciteturn15file3L240-L360

### Streaming support has been explicitly called out as needing fixes

NeMo release notes explicitly list “**TDT streaming inference fix**” and a “Tdt buffered inference fix” as part of the 2.4.0 release highlights/changelog. citeturn19view0

Separately, public discussion around Parakeet TDT streaming indicates NVIDIA has acknowledged **a bug when using TDT chunked streaming inference**, with fixes landing in NeMo’s main branch, and notes about decoding strategy differences between offline and streaming. citeturn2search0

Even if your environment is newer than 2.4.0, this history matters because it implies:
- the streaming stack is under active correction, and
- regressions/behavior changes across versions are plausible.

## Engineering recommendations that can realistically hit <0.20 streaming WER

This section prioritizes changes that address *fundamentals* (state/merge/drain) rather than “tuning harder.”

### Implement a real finalize drain instead of tail padding

Your tail padding experiment is already acting as a proxy drain and confirms the hypothesis. fileciteturn15file3L300-L360

A correct finalize should do one of the following:

- **Drain-by-frames:** after the last real audio frame, run the decoder for *N additional frames* of “blank” (silence features), where N is sufficient to cover the configured delay/right-context so that any pending tokens become emit-eligible.  
- **Drain-by-policy:** on the final window, reduce or disable the “middle-only” discard rule and keep the right edge (or do a final full-window decode and merge it appropriately).

In your case, because you are not doing true real-time streaming during finalize (you already have the full utterance), you can drain **without adding user-perceived latency**—it’s just extra compute, and your offline path is already fast. fileciteturn15file3L70-L160

### Use the model-card streaming path and its context defaults as your baseline

The Parakeet-TDT v3 model card explicitly instructs users to run streaming via NeMo’s script for streaming inference and provides a concrete configuration including **right context, chunk size, and large left context**. citeturn22search0

Treat this as an “official baseline” for correctness, then adapt it into your daemon:

- start with the model card’s `chunk_secs`, `right_context_secs`, and especially `left_context_secs`,  
- reproduce its decode strategy and any implied merge logic,  
- then re-run your bench set and compare.

This is not an external workaround; it’s aligning with the vendor’s intended usage.

### Prefer LCS-based stitching over naive concatenation

Your repo already lists “LCS-based merge helper” as a next avenue, and NeMo highlights LCS merge as a first-class buffered transducer technique. fileciteturn15file3L360-L390 citeturn9view0

Why this matters here: LCS-style merging is much less likely to “drop the boundary” because it attempts to align overlapping hypotheses and keep the consistent sequence, rather than discarding fixed boundary regions.

For short dictation utterances (3–15 seconds), the compute overhead is typically negligible relative to the GPU forward pass.

### Make stream-then-seal your highest-confidence path to <0.20 WER

If your product requirement is “streaming UX + near-offline final accuracy,” the most robust architecture is:

- Streaming decoder produces partial hypotheses (possibly imperfect).  
- Endpoint detected (utterance end).  
- Final result is produced by offline `model.transcribe()` on the segmented utterance audio.

Your own measurements show offline finalize latency around **tens of milliseconds**, easily within the <200ms post-utterance budget you stated, while streaming finalize is currently slower and less accurate. fileciteturn15file3L70-L160

This approach is used widely in production streaming ASR systems because it is *structurally* resistant to boundary/merge errors: the final pass sees the whole segment and cannot “forget the last words” due to a delayed emission buffer.

It’s also forward-compatible: once the streaming path is fixed, the second pass can be disabled or reserved for low-confidence cases.

### Consider switching to streaming-trained / cache-aware streaming models if “one-pass streaming accuracy” is the goal

NeMo documentation explicitly notes that evaluating streaming on a model trained offline (full context) will degrade unless chunk sizes are very large (which defeats low-latency streaming). citeturn1search18

Separately, NeMo releases have introduced or highlighted streaming model families (e.g., “MT-Parakeet Streaming Models release” in later releases), suggesting NVIDIA is differentiating “offline checkpoint” vs “streaming checkpoint” as first-class artifacts. citeturn3search3

If you need a single-pass streaming transcript that stays within ~2× offline WER, **a streaming-trained checkpoint** is often the only scalable answer.

## Version strategy and why upgrading NeMo is likely part of the solution

Two independent signals indicate NeMo version matters for your exact problem:

- NeMo release notes explicitly call out **“TDT streaming inference fix.”** citeturn19view0  
- NVIDIA has issued security guidance recommending upgrading NeMo Framework to **2.6.1 or later**. citeturn17search6

On the packaging timeline, PyPI shows:

- `nemo-toolkit` 2.5.3 (your current baseline) released in 2025, and
- `nemo-toolkit` 2.6.2 released **Feb 6, 2026**. citeturn3search7

Given that:
- your issue is plausibly within “streaming correctness bugs + stitching behavior,” and
- upstream has repeatedly shipped streaming-related fixes,

a controlled upgrade to **NeMo 2.6.2** is justified even before performance tuning—especially since your request is explicitly *root-cause solutions*, and a known-bug category is a root cause. citeturn3search7turn19view0

A practical upgrade stance:

- **Target:** NeMo 2.6.2 (or at minimum 2.6.1 for security guidance), keep your Python 3.11 stack. citeturn3search7turn17search6  
- **Validate:** re-run your bench suite with (a) the model-card streaming script parameters, and (b) finalize drain.  
- **Only then:** revisit fine-grained knobs (chunk size, right-context, boundary search, max-steps).

## Cross-path improvements that strengthen both offline and streaming

These are not speculative “try turning knobs”; they are changes that improve robustness and/or latency across both modes in your documented system.

### Prefer in-memory transcription and avoid temp WAV roundtrips

Your state document notes that the daemon has moved toward in-memory `np.ndarray` transcription for offline finalize and warmup, with regression coverage and temp-wav fallback retained. This reduces filesystem overhead and is a direct latency win for finalization. fileciteturn15file3L25-L60

This also pairs well with stream-then-seal, because your “seal” pass becomes extremely cheap.

### Use NeMo’s officially documented streaming APIs where possible

Your deep research report emphasizes using NeMo’s documented streaming step API (e.g., `conformer_stream_step`) rather than brittle internal helper import paths, and NeMo’s public docs describe this cache-aware streaming mechanism and state requirements. fileciteturn12file0L1-L60 citeturn1search0

Even if you keep your current helper-based approach, migrating toward the documented API surface reduces the chance that a helper class silently changes behavior across NeMo versions.

### Make “streaming truth” observable and testable

Your internal reports already show that misreporting “streaming enabled” while falling back to offline is a risk, and you have added tests around streaming truth paths. Keeping this discipline (hard status signals, regression tests around helper activation/fallback) prevents you from debugging phantom regressions later. fileciteturn15file3L1-L120

In practice: if “streaming mode” can silently become “offline mode,” you’ll waste weeks tuning parameters on the wrong pipeline. (Computers are incredible at being confidently wrong. So are humans, but computers do it faster.)

## What this research could not confirm directly

You requested NeMo source-level root-cause grounding (e.g., the exact boundary-drop point in `BatchedFrameASRTDT`/`FrameBatchChunkedRNNT`). Public release notes and NVIDIA statements strongly indicate TDT streaming inference has had concrete fixes and behavioral caveats, but the exact code path that drops boundary tokens could not be quoted directly here due to limited direct access to the specific NeMo source file views for the relevant helper classes in this session. citeturn19view0turn2search0