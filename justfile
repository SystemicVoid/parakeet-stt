set shell := ["bash", "-euo", "pipefail", "-c"]

daemon_dir := justfile_directory() + "/parakeet-stt-daemon"
personal_dir := "bench_audio/personal"
manifest_path := personal_dir + "/manifest.jsonl"
offline_baseline := personal_dir + "/baseline.json"
stream_baseline := personal_dir + "/baseline-stream-seal.json"

# Show available commands.
default:
    @just --list

# Build candidate prompts from Codex user prompt history.
eval-candidates:
    cd {{daemon_dir}} && uv run python scripts/build_personal_eval_candidates.py --output {{personal_dir}}/candidates.tsv

# Materialize reviewed candidates into manifest + prompts list.
eval-materialize:
    cd {{daemon_dir}} && uv run python scripts/materialize_personal_manifest.py --input {{personal_dir}}/candidates.tsv --output {{manifest_path}} --prompts-output {{personal_dir}}/prompts.tsv --tier daily

# Record prompts interactively (TUI controls shown in-script).
eval-record:
    cd {{daemon_dir}} && bash scripts/record_personal_clips.sh --manifest {{manifest_path}} --output-dir {{personal_dir}}/audio

# Calibrate offline baseline from current recordings.
eval-calibrate-offline:
    cd {{daemon_dir}} && uv run python check_model.py --bench-offline --bench-runtime offline --bench-manifest {{personal_dir}} --bench-tier daily --calibrate-baseline --baseline-output {{offline_baseline}} --bench-output {{personal_dir}}/latest-offline-calibration.json

# Run offline daily gate against offline baseline.
eval-daily-offline:
    cd {{daemon_dir}} && uv run python check_model.py --bench-offline --bench-runtime offline --bench-manifest {{personal_dir}} --bench-tier daily --baseline {{offline_baseline}} --bench-output {{personal_dir}}/latest-offline-daily.json

# Calibrate stream+seal baseline (daemon-like finalize path).
eval-calibrate-stream:
    cd {{daemon_dir}} && uv run python check_model.py --bench-offline --bench-runtime stream-seal --stream-chunk-secs 2.4 --stream-right-context-secs 1.6 --stream-left-context-secs 10.0 --stream-batch-size 32 --bench-manifest {{personal_dir}} --bench-tier daily --calibrate-baseline --baseline-output {{stream_baseline}} --bench-output {{personal_dir}}/latest-stream-seal-calibration.json

# Run stream+seal daily gate against stream baseline.
eval-daily-stream:
    cd {{daemon_dir}} && uv run python check_model.py --bench-offline --bench-runtime stream-seal --stream-chunk-secs 2.4 --stream-right-context-secs 1.6 --stream-left-context-secs 10.0 --stream-batch-size 32 --bench-manifest {{personal_dir}} --bench-tier daily --baseline {{stream_baseline}} --bench-output {{personal_dir}}/latest-stream-seal-daily.json

# Run both daily gates and print a compact side-by-side metric summary.
eval-compare:
    just eval-daily-offline
    just eval-daily-stream
    cd {{daemon_dir}} && python3 -c 'import json; from pathlib import Path; offline=json.loads(Path("bench_audio/personal/latest-offline-daily.json").read_text(encoding="utf-8")); stream=json.loads(Path("bench_audio/personal/latest-stream-seal-daily.json").read_text(encoding="utf-8")); keys=("weighted_wer","command_exact_match_rate","critical_token_recall","punctuation_f1","terminal_punctuation_accuracy","warm_finalize_p95_ms"); metric=lambda r,k: float(r["aggregate"]["warm_finalize_ms"]["p95"]) if k=="warm_finalize_p95_ms" else float(r["aggregate"][k]); print("metric\toffline\tstream_seal\tdelta(stream-offline)"); [print(f"{k}\t{metric(offline,k):.6f}\t{metric(stream,k):.6f}\t{(metric(stream,k)-metric(offline,k)):.6f}") for k in keys]'
