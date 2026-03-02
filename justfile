set shell := ["bash", "-euo", "pipefail", "-c"]

daemon_dir := justfile_directory() + "/parakeet-stt-daemon"
overlay_justfile := justfile_directory() + "/justfile.overlay-dev"
personal_dir := "bench_audio/personal"
manifest_path := personal_dir + "/manifest.jsonl"
offline_baseline := personal_dir + "/baseline.json"
stream_baseline := personal_dir + "/baseline-stream-seal.json"
stream_runtime_flags := "--bench-runtime stream-seal --stream-chunk-secs 2.4 --stream-right-context-secs 1.6 --stream-left-context-secs 10.0 --stream-batch-size 32 --stream-max-tail-trim-secs 0.35"
unified_flags := "--bench-offline --bench-manifest bench_audio/personal --bench-append-legacy --bench-tier all"

# Show available commands.
default:
    @just --list

# Overlay dev shortcuts (delegates to justfile.overlay-dev).
start mode="layer-shell" adaptive_width="false":
    @just --justfile "{{overlay_justfile}}" start "{{mode}}" "{{adaptive_width}}"

# Start with adaptive width enabled (opt-in).
start-adaptive mode="layer-shell":
    @just --justfile "{{overlay_justfile}}" start "{{mode}}" "true"

stop:
    @just --justfile "{{overlay_justfile}}" stop

status:
    @just --justfile "{{overlay_justfile}}" status

logs:
    @just --justfile "{{overlay_justfile}}" logs

logs-overlay:
    @just --justfile "{{overlay_justfile}}" logs-overlay

overlay-kill:
    @just --justfile "{{overlay_justfile}}" overlay-kill

runbook:
    @just --justfile "{{overlay_justfile}}" runbook

phase6-contract:
    @just --justfile "{{overlay_justfile}}" phase6-contract

phase6-promotion runs="3":
    @just --justfile "{{overlay_justfile}}" phase6-promotion "{{runs}}"

soak-perf duration_secs="600" sample_secs="1":
    @just --justfile "{{overlay_justfile}}" soak-perf "{{duration_secs}}" "{{sample_secs}}"

overlay-doctor:
    @just --justfile "{{overlay_justfile}}" doctor

# Unified personal STT eval runner (uses existing dataset only).
# Usage:
#   just eval                          # run offline + stream and print comparison
#   just eval offline                  # run offline gate
#   just eval stream                   # run stream-seal gate
#   just eval compare                  # run both and print side-by-side metrics
#   just eval calibrate-offline        # refresh offline baseline
#   just eval calibrate-stream         # refresh stream baseline
#   just eval calibrate-both           # refresh both baselines
eval action="compare":
    @case "{{action}}" in \
      offline|run-offline) just _eval-run-offline ;; \
      stream|stream-seal|run-stream) just _eval-run-stream ;; \
      compare|run-both) just _eval-compare ;; \
      calibrate-offline) just _eval-calibrate-offline ;; \
      calibrate-stream) just _eval-calibrate-stream ;; \
      calibrate-both) just _eval-calibrate-offline && just _eval-calibrate-stream ;; \
      help) \
        printf '%s\n' \
          "Unified STT eval runner (existing dataset only)" \
          "" \
          "Commands:" \
          "  just eval                      # compare (offline + stream-seal)" \
          "  just eval offline" \
          "  just eval stream" \
          "  just eval compare" \
          "  just eval calibrate-offline" \
          "  just eval calibrate-stream" \
          "  just eval calibrate-both" \
          "" \
          "Dataset setup (rare): just eval-dataset help" \
          "This runner never regenerates prompts or re-records audio."; \
        ;; \
      *) echo "Invalid action '{{action}}'. Run: just eval help" >&2; exit 2 ;; \
    esac

# Dataset maintenance (run only when intentionally updating corpus content).
eval-dataset action="help":
    @case "{{action}}" in \
      candidates) just _eval-candidates ;; \
      materialize) just _eval-materialize ;; \
      record) just _eval-record ;; \
      help) \
        printf '%s\n' \
          "Dataset maintenance commands" \
          "" \
          "  just eval-dataset candidates" \
          "  just eval-dataset materialize" \
          "  just eval-dataset record"; \
        ;; \
      *) echo "Invalid action '{{action}}'. Run: just eval-dataset help" >&2; exit 2 ;; \
    esac

[private]
_eval-candidates:
    cd {{daemon_dir}} && uv run python scripts/build_personal_eval_candidates.py --output {{personal_dir}}/candidates.tsv

[private]
_eval-materialize:
    cd {{daemon_dir}} && uv run python scripts/materialize_personal_manifest.py --input {{personal_dir}}/candidates.tsv --output {{manifest_path}} --prompts-output {{personal_dir}}/prompts.tsv

[private]
_eval-record:
    cd {{daemon_dir}} && bash scripts/record_personal_clips.sh --manifest {{manifest_path}} --output-dir {{personal_dir}}/audio

[private]
_eval-calibrate-offline:
    cd {{daemon_dir}} && uv run python check_model.py {{unified_flags}} --bench-runtime offline --calibrate-baseline --baseline-output {{offline_baseline}} --bench-output {{personal_dir}}/latest-offline-calibration.json

[private]
_eval-run-offline:
    cd {{daemon_dir}} && uv run python check_model.py {{unified_flags}} --bench-runtime offline --baseline {{offline_baseline}} --bench-output {{personal_dir}}/latest-offline.json

[private]
_eval-calibrate-stream:
    cd {{daemon_dir}} && uv run python check_model.py {{unified_flags}} {{stream_runtime_flags}} --calibrate-baseline --baseline-output {{stream_baseline}} --bench-output {{personal_dir}}/latest-stream-calibration.json

[private]
_eval-run-stream:
    cd {{daemon_dir}} && uv run python check_model.py {{unified_flags}} {{stream_runtime_flags}} --baseline {{stream_baseline}} --bench-output {{personal_dir}}/latest-stream.json

[private]
_eval-compare:
    just _eval-run-offline
    just _eval-run-stream
    cd {{daemon_dir}} && python3 -c 'import json; from pathlib import Path; offline=json.loads(Path("bench_audio/personal/latest-offline.json").read_text(encoding="utf-8")); stream=json.loads(Path("bench_audio/personal/latest-stream.json").read_text(encoding="utf-8")); keys=("weighted_wer","command_exact_match_rate_strict","command_exact_match_rate_normalized","command_intent_slot_match_rate","critical_token_recall","punctuation_f1","terminal_punctuation_accuracy","warm_finalize_p95_ms"); metric=lambda r,k: float(r["aggregate"]["warm_finalize_ms"]["p95"]) if k=="warm_finalize_p95_ms" else float(r["aggregate"][k]); print("metric\toffline\tstream_seal\tdelta(stream-offline)"); [print(f"{k}\t{metric(offline,k):.6f}\t{metric(stream,k):.6f}\t{(metric(stream,k)-metric(offline,k)):.6f}") for k in keys]'
