# STT Evaluation System - Technical Report

## Executive Summary

The Parakeet-STT project has developed a comprehensive evaluation (evals) framework for measuring and monitoring speech-to-text transcription quality. The system evolved through multiple iterations, ultimately establishing a **Stream-Then-Seal** architecture that achieves parity between offline and streaming transcription quality while maintaining sub-200ms finalize latency.

---

## 1. Evaluation Architecture Overview

The evaluation system consists of several interconnected components:

### Core Components

| Component | Location | Purpose |
|-----------|----------|---------|
| **Benchmark Harness** | `parakeet-stt-daemon/check_model.py` | Main evaluation engine with WER computation, command matching, and regression testing |
| **Unit Tests** | `parakeet-stt-daemon/tests/test_offline_benchmark_harness.py` | Regression coverage for benchmark parsing and metrics |
| **Dataset** | `parakeet-stt-daemon/bench_audio/personal/` | Personal eval corpus with 87+ command audio clips |
| **Workflow** | `justfile` | Unified CLI for running evals and calibration |

### Two-Tier Evaluation Model

1. **Offline Mode**: Full audio processed in a single `transcribe_samples()` call
2. **Stream-Seal Mode**: Chunked streaming transcription with final offline "seal" pass for quality

---

## 2. Evaluation Metrics

The system tracks multiple metrics (defined in `check_model.py:99-148`):

### Primary Quality Metrics

| Metric | Description | Tier Thresholds (daily) |
|--------|-------------|------------------------|
| `weighted_wer` | Weighted Word Error Rate | ≤ 0.20 |
| `command_exact_match_rate_strict` | Exact match for commands (strict) | ≥ 0.70 |
| `command_exact_match_rate_normalized` | Exact match for commands (normalized) | ≥ 0.70 |
| `command_intent_slot_match_rate` | Intent + slot matching | — |
| `critical_token_recall` | Recall of domain-critical tokens | ≥ 0.94 |
| `punctuation_f1` | F1 score for punctuation marks | ≥ 0.70 |
| `terminal_punctuation_accuracy` | Accuracy of terminal punctuation (.?!) | ≥ 0.85 |

### Performance Metrics

| Metric | Description | Threshold |
|--------|-------------|-----------|
| `warm_finalize_p95_ms` | P95 finalize latency (warm) | ≤ 180ms |

### Regression Detection

The system supports **drift detection** with configurable delta thresholds:
- `max_weighted_wer_delta`: 0.03
- `max_command_exact_match_drop`: 0.05
- `max_critical_token_recall_drop`: 0.03
- `max_punctuation_f1_drop`: 0.08
- `max_warm_p95_finalize_ms_delta`: 40ms

---

## 3. Technical Implementation Details

### WER Computation

Located in `check_model.py:425-534`:

```python
def compute_normalized_wer(reference: str, hypothesis: str) -> float:
    """Standard Levenshtein distance-based WER with tokenization."""
    # Tokenizes on word boundaries, computes edit distance
```

The system uses:
- **Tokenization**: `\w+` regex pattern (Unicode-aware)
- **Normalization**: Case folding, punctuation stripping, whitespace normalization
- **Weighting**: Per-sample WER aggregated with weighted average by audio duration

### Command Matching

Command matching includes multiple matching strategies (`check_model.py:473-519`):

1. **Strict exact match**: Character-for-character identical
2. **Normalized match**: Lowercase, strip articles ("a", "an", "the"), remove filler tokens ("and", "can", "please", etc.)
3. **Intent-slot matching**: Parses commands into intent + slots (e.g., "git commit" → intent="commit", slots={})

Special handling includes:
- **Smart quotes normalization**: Converts curly quotes to straight (`"`, `'`)
- **Synonym mapping**: "begin" → "start", "browse" → "open", etc.

### Punctuation Metrics

`compute_punctuation_metrics()` (`check_model.py:579-`):
- Extracts punctuation tokens via `[,?!;:]` regex
- Computes precision/recall/F1 for punctuation preservation
- Validates terminal punctuation (`.?!`) separately

### Critical Token Tracking

Each benchmark sample can define `critical_tokens` - domain-specific words that must be transcribed correctly:
- **Use case**: Command recognition ("stt", "start", "--paste")
- **Metric**: `compute_critical_token_recall()` - recall only (false positives less harmful than misses)

---

## 4. Dataset Structure

### Legacy Benchmark Corpus

Location: `parakeet-stt-daemon/bench_audio/`
- `transcripts.txt` - Numbered transcript references
- `sample_01.wav` ... `sample_08.wav` - 8 canonical audio samples
- Used for regression testing and quick smoke validation

### Personal Eval Corpus

Location: `parakeet-stt-daemon/bench_audio/personal/`

**Manifest Format** (`manifest.jsonl`):
```json
{
  "sample_id": "cmd_001",
  "audio_path": "personal/audio/cmd_001.wav",
  "reference": "Good, we have seventy percent...",
  "tier": "daily",
  "domain": "command",
  "critical_tokens": ["good", "have", "seventy", "percent"],
  "source": "curated_core"
}
```

**Tiers**:
- `daily`: Full quality gates with regression checks
- `smoke`: Quick sanity checks (faster, looser thresholds)
- `weekly`: Strictest quality bar (lower WER, higher match rates)

---

## 5. Workflow Commands

Defined in `justfile:24-100`:

```bash
# Run evaluation (existing dataset only)
just eval                          # compare offline + stream-seal
just eval offline                  # offline benchmark gate
just eval stream                   # stream-seal benchmark gate
just eval compare                  # side-by-side metrics

# Calibration ( establish baseline)
just eval calibrate-offline        # refresh offline baseline
just eval calibrate-stream         # refresh stream baseline
just eval calibrate-both          # refresh both baselines

# Dataset maintenance
just eval-dataset candidates       # build eval candidates from Codex history
just eval-dataset materialize      # generate manifest from candidates
just eval-dataset record           # record audio clips
```

### Stream Runtime Configuration

```bash
--bench-runtime stream-seal \
--stream-chunk-secs 2.4 \
--stream-right-context-secs 1.6 \
--stream-left-context-secs 10.0 \
--stream-batch-size 32 \
--stream-max-tail-trim-secs 0.35
```

---

## 6. Key Technical Decisions

### Stream-Then-Seal Architecture

The most significant decision was implementing **Stream-Then-Seal** mode:

> **Problem**: Pure streaming (helper-only finalize) produced WER of ~0.50-0.58 vs offline ~0.09 due to:
> - End-of-utterance drain missing (buffered tokens not emitted)
> - TDT burst emission exceeding `tokens_per_chunk` limits
> - `stateful_decoding` inheritance bug in NeMo's `BatchedFrameASRTDT`

> **Solution**: Use streaming for real-time partial results, then run offline `transcribe_samples()` for final committed transcript.

**Results**:
- Offline WER: 0.0938
- Stream-Seal WER: 0.0813 (actually slightly better!)
- Finalize latency: 135ms (50ms improvement over offline)

### Tail-Trim Evolution

1. **Initial**: No trim → WER 0.535
2. **Waveform padding**: 0.2s → WER 0.483, 0.6s → WER 0.410
3. **Final**: VAD-based trim (Silero VAD v6) behind opt-in flag

### NeMo Upgrade Path

- Upgraded from NeMo 2.5.3 → 2.6.2 for security and streaming fixes
- Required `cuda-python>=13,<14` compatibility fix
- Benchmark gate: WER ≤ 0.12, P95 infer ≤ 300ms, P95 finalize ≤ 300ms

---

## 7. Test Coverage

Unit tests in `test_offline_benchmark_harness.py`:

- Transcript parsing (`test_parse_benchmark_transcripts_extracts_numbered_entries`)
- Manifest validation (`test_collect_benchmark_cases_validates_transcript_audio_parity`)
- Tier filtering (`test_parse_benchmark_manifest_filters_tier_and_normalizes_tokens`)
- WER computation
- Command matching (strict, normalized, intent-slot)
- Critical token recall
- Punctuation metrics

Regression tests for stream-seal tail-loss: `test_streaming_tail_loss_stream_seal_finalize_path` (commit `c2942f2`)

---

## 8. Git History (Evaluation-Related Commits)

| Commit | Description |
|--------|-------------|
| `c2942f2` | test(benchmark): add stream-seal tail-loss regression coverage |
| `2c20ea1` | fix(benchmark): preserve sort keyword in command matching |
| `02a1f97` | feat(benchmark): split command match into strict normalized and intent |
| `330d7e3` | fix(streaming): prevent tail-loss in stream-seal finalize path |
| `5669204` | chore(workflow): make just eval run-only and self-documenting |
| `aa080ee` | feat(benchmark): support unified manifest+legacy corpus runs |
| `9114d5a` | docs(workflow): streamline personal eval guidance |
| `3c3abbe` | chore(workflow): add just recipes for personal eval loop |
| `314816e` | feat(benchmark): add stream-seal runtime and punctuation gates |

---

## 9. References

### Code
- Benchmark harness: `parakeet-stt-daemon/check_model.py`
- Unit tests: `parakeet-stt-daemon/tests/test_offline_benchmark_harness.py`
- Workflow: `justfile` (lines 24-100)
- Configuration profiles: `check_model.py:99-148`

### Documentation
- **Harness Engineering Playbook**: `docs/engineering/harness-engineering-playbook.md`
- **State of STT Daemon**: `docs/archive/STATE-OF-STT-DAEMON-SERVER-2026-02-23.md` (contains detailed SA action item results)

### Research
- **Streaming WER Gap**: `docs/archive/deep-research-streaming-quality.md`
- **TDT Streaming Research**: `docs/archive/gemini-TDT Streaming WER Gap Research.md`

---

## 10. Future Work: Mechanistic Interpretability

*Moving from "Black Box Engineering" to "Mechanistic Science"*

### 10.1 Current Gap: Engineering Without Causality

Our current evaluation system measures **what** fails:
- We know streaming produces WER ~0.53 vs offline ~0.09
- We know Stream-Then-Seal fixes it (WER ~0.08)
- We hypothesize root causes: "end-of-utterance drain missing" and "TDT burst emission"

**The problem**: These are symptoms, not mechanisms. We fixed the symptom (via Seal pass) but don't understand *why* streaming fails at the model internal level.

### 10.2 Proposed: Logit Lens on Critical Failures

When `critical_token_recall` drops, capture the model's internal state:

**Experiment Design:**
1. Hook into the model's final layer for every critical token mismatch
2. Save Top-10 logits and their probabilities at the moment of failure
3. Compute **critical_token_entropy**: measure whether the model was confident-and-wrong (scary) vs uncertain-and-wrong (fixable)

**Hypothesis:** "git commit" failures may show the correct token in Top-5 but suppressed by a bias - suggesting a targeted intervention is possible.

**Metric to add:**
```python
def compute_critical_token_entropy(logits: np.ndarray, vocab: list[str], target_token: str) -> float:
    """Compute entropy of distribution when target token is present/absent."""
    probs = softmax(logits)
    # ... measure uncertainty
```

### 10.3 Proposed: Seal vs Stream Attention Pattern Analysis

**Mechanistic Question:** What does the Offline (Seal) pass attend to that Streaming misses?

**Experiment (RTX 5060 Ti-friendly):**
1. Load a single failure case (Stream fails, Seal succeeds)
2. Extract **Attention Patterns** from middle layers for both passes
3. Compute: `attention_delta = Seal_attention - Stream_attention`

**Expected Finding:** Seal likely attends to "future" tokens (right context) that Stream cannot see due to chunk boundaries.

**Actionable Outcome:** Tune `stream-right-context-secs` based on *attention head analysis* rather than trial-and-error grid sweeps.

### 10.4 Proposed: TDT Duration Head Diagnosis

TDT (Token-and-Duration Transducer) models have a **duration head** that predicts how long each token spans.

**Experiment:**
1. Hook duration head outputs during streaming
2. Plot predicted duration distributions for "burst" frames
3. Question: Are duration predictions saturated? Is a specific induction head triggering bursts?

### 10.5 Implementation Roadmap

**Step 1: Add Inspection Tools**
```bash
cd parakeet-stt-daemon
uv add nnsight   # Clean Pythonic hooking for custom architectures
uv add plotly   # Visualization for attention/logits
```

**Step 2: Diagnostic Test File**
Create `tests/diagnostics/test_mechanistic_failure.py`:
```python
def test_logit_rank_on_failure():
    """For each critical token mismatch, output Top-10 logits."""
    # 1. Load failure case from personal eval corpus
    # 2. Run with hooks enabled
    # 3. Print: "Target 'git': rank=N, prob=X.XXe-N"
```

**Step 3: VRAM Management (RTX 5060 Ti - 16GB)**
- Store activations only for *specific timestamps* where errors occur
- Do not store full-sequence attention maps unless CPU offloading

### 10.6 From "Toyota Camry" to "Formula 1 Telemetry"

Current state: **Reliable eval system** - measures performance, detects regressions, guides engineering decisions.

Target state: **Mechanistic telemetry** - understands *why* failures happen, enables targeted interventions instead of workarounds.

**Homework (Immediate Next Steps):**
1. Pick one failure case from Personal Eval Corpus (e.g., `cmd_073` or `cmd_087`)
2. Write diagnostic script outputting **Logit Rank** of correct token at failure point
3. Answer: Was the correct word in Top-10? If so, how far down?

---

## Key Takeaways for Technical Blog

1. **Parity Achieved**: Stream-seal now matches offline quality (WER ~0.08) while reducing latency by ~50ms
2. **Comprehensive Metrics**: 8+ quality metrics covering WER, command matching, punctuation, critical tokens, and latency
3. **Regression Protection**: Baseline calibration + drift detection prevents silent quality degradation
4. **Evidence-Based Evolution**: Each architectural decision (Stream-Then-Seal, VAD trim, NeMo upgrade) was driven by benchmark evidence, not speculation
5. **Next Frontier**: Mechanistic interpretability - understanding *why* failures happen at the model internal level
