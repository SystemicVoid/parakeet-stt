set shell := ["bash", "-euo", "pipefail", "-c"]

repo_root := justfile_directory()
daemon_dir := justfile_directory() + "/parakeet-stt-daemon"
personal_dir := "bench_audio/personal"
manifest_path := personal_dir + "/manifest.jsonl"
offline_baseline := personal_dir + "/baseline.json"
stream_baseline := personal_dir + "/baseline-stream-seal.json"
stream_runtime_flags := "--bench-runtime stream-seal --stream-chunk-secs 2.4 --stream-right-context-secs 1.6 --stream-left-context-secs 10.0 --stream-batch-size 32 --stream-max-tail-trim-secs 0.35"
unified_flags := "--bench-offline --bench-manifest bench_audio/personal --bench-append-legacy --bench-tier all"
status_url := "http://127.0.0.1:8765/status"

# Show available commands.
default:
    @just --list

# Build local Rust binaries for the current host CPU.
build:
    @bash -lc 'cd "{{repo_root}}/parakeet-ptt" && rustflags="${PARAKEET_PTT_RUSTFLAGS:--C target-cpu=native}" && echo ">>> building parakeet-ptt release binaries with RUSTFLAGS=${rustflags}" && RUSTFLAGS="${rustflags}" cargo build --release --bins'

# Overlay helper shortcuts.
start mode="auto" adaptive_width="false":
    @bash -lc 'cd "{{repo_root}}" && mode="{{mode}}" && adaptive_width="{{adaptive_width}}" && case "$mode" in auto|layer-shell|fallback-window|disabled) ;; *) echo "mode must be one of: auto|layer-shell|fallback-window|disabled" >&2; exit 2 ;; esac && case "${adaptive_width,,}" in true|false) ;; *) echo "adaptive_width must be true or false" >&2; exit 2 ;; esac && export PARAKEET_ROOT="{{repo_root}}" && export PARAKEET_OVERLAY_MODE="$mode" && source scripts/stt-helper.sh && stt restart --overlay-enabled true --overlay-adaptive-width "$adaptive_width"'

start-sound mode="auto" adaptive_width="false" sound_path="sounds/completion.ogg":
    @bash -lc 'cd "{{repo_root}}" && mode="{{mode}}" && adaptive_width="{{adaptive_width}}" && sound_path="{{sound_path}}" && case "$mode" in auto|layer-shell|fallback-window|disabled) ;; *) echo "mode must be one of: auto|layer-shell|fallback-window|disabled" >&2; exit 2 ;; esac && case "${adaptive_width,,}" in true|false) ;; *) echo "adaptive_width must be true or false" >&2; exit 2 ;; esac && if [[ ! -f "$sound_path" ]]; then echo "sound file not found: $sound_path" >&2; exit 2; fi && export PARAKEET_ROOT="{{repo_root}}" && export PARAKEET_OVERLAY_MODE="$mode" && source scripts/stt-helper.sh && stt restart --completion-sound true --completion-sound-path "$sound_path" --completion-sound-volume 100 --overlay-enabled true --overlay-adaptive-width "$adaptive_width"'

# Start with adaptive width enabled (opt-in).
start-adaptive mode="auto":
    @just start "{{mode}}" "true"

stop:
    @bash -lc 'cd "{{repo_root}}" && export PARAKEET_ROOT="{{repo_root}}" && source scripts/stt-helper.sh && stt stop'

status:
    @bash -lc 'cd "{{repo_root}}" && export PARAKEET_ROOT="{{repo_root}}" && source scripts/stt-helper.sh && stt status && echo && echo ">>> daemon status overlay fields" && if payload="$(curl -fsS "{{status_url}}" 2>/dev/null)"; then printf "%s" "$payload" | python3 -m json.tool | rg "streaming_enabled|overlay_events_enabled|overlay_events_emitted|overlay_events_dropped" || true; else echo "(daemon status unavailable)"; fi && echo && echo ">>> overlay processes" && pattern="(^|/)parakeet-overlay($| )" && matches="$(pgrep -af "$pattern" 2>/dev/null || true)" && if [[ -n "$matches" ]]; then printf "%s\n" "$matches"; else echo "(none)"; fi'

logs:
    @bash -lc 'cd "{{repo_root}}" && export PARAKEET_ROOT="{{repo_root}}" && source scripts/stt-helper.sh && stt logs both'

logs-overlay:
    @cd "{{repo_root}}"
    tail -f /tmp/parakeet-ptt.log /tmp/parakeet-daemon.log | rg -i "overlay|interim|session_ended|final_result|spawn|replay|disconnect"

overlay-kill:
    @bash -lc 'cd "{{repo_root}}" && pattern="(^|/)parakeet-overlay($| )" && echo ">>> before" && before="$(pgrep -af "$pattern" 2>/dev/null || true)" && if [[ -n "$before" ]]; then printf "%s\n" "$before"; else echo "(none)"; fi && (pkill -f "$pattern" || true) && sleep 0.4 && echo ">>> after" && after="$(pgrep -af "$pattern" 2>/dev/null || true)" && if [[ -n "$after" ]]; then printf "%s\n" "$after"; else echo "(none; trigger PTT event to respawn)"; fi'

runbook:
    @printf '%s\n' \
      "1) Start session: just start" \
      "2) Quick utterance: tap hotkey briefly; expect one final injection and clean auto-hide." \
      "3) Long dictation: hold hotkey for sustained speech; expect rolling interim tail + one final injection." \
      "4) Abort mid-session: release early or abort; expect session_ended=abort and no injected final text." \
      "5) Overlay crash mid-session: just overlay-kill; next overlay event should respawn and final injection still succeeds." \
      "6) Daemon reconnect: kill daemon process, restart helper stack, verify client reconnect + subsequent final injection." \
      "7) Mixed-version compatibility: run protocol decode check (cargo test decode_server_message_mixed_version_stream_tolerates_unknown_between_known_messages)." \
      "8) Promotion gate: just phase6-promotion 3" \
      "9) Stop helper stack: just stop"

phase6-contract:
    @bash -lc 'cd "{{repo_root}}" && echo ">>> daemon overlay contract suite" && cd parakeet-stt-daemon && uv run pytest tests/test_overlay_event_stream.py && echo ">>> ptt mixed-version + overlay fault-isolation suite" && cd ../parakeet-ptt && cargo test decode_server_message_mixed_version_stream_tolerates_unknown_between_known_messages && cargo test mixed_stream_enqueues_exactly_one_final_result && cargo test overlay_crash_restart_replays_current_state_and_preserves_final_injection && cargo test repeated_overlay_failures_remain_non_fatal_to_final_injection'

phase6-promotion runs="3":
    @bash -lc 'cd "{{repo_root}}" && runs="{{runs}}" && if ! [[ "$runs" =~ ^[0-9]+$ ]] || (( runs < 3 )); then echo "runs must be an integer >= 3" >&2; exit 1; fi && outfile="/tmp/parakeet-overlay-phase6-gate-$(date +%Y%m%d-%H%M%S).log" && { echo ">>> phase6 promotion gate (${runs} clean runs required)"; for run in $(seq 1 "$runs"); do echo ""; echo "=== reliability run ${run}/${runs} ==="; just phase6-contract; done; echo ""; echo "=== stream/seal regression gate ==="; just eval compare; echo ""; echo "artifact=$outfile"; } 2>&1 | tee "$outfile"'

soak-perf duration_secs="600" sample_secs="1":
    @bash -lc 'cd "{{repo_root}}" && duration="{{duration_secs}}" && sample="{{sample_secs}}" && if ! [[ "$duration" =~ ^[0-9]+$ ]] || (( duration < 600 )); then echo "duration_secs must be an integer >= 600" >&2; exit 1; fi && if ! [[ "$sample" =~ ^[0-9]+$ ]] || (( sample < 1 )); then echo "sample_secs must be an integer >= 1" >&2; exit 1; fi && outfile="/tmp/parakeet-overlay-soak-$(date +%Y%m%d-%H%M%S).tsv" && echo -e "ts\tproc\tpid\tcpu_pct\trss_kb" > "$outfile" && echo ">>> sampling for ${duration}s (interval=${sample}s) -> $outfile" && end=$((SECONDS + duration)) && while (( SECONDS < end )); do ts="$(date -Is)"; for proc in parakeet-ptt parakeet-overlay; do rows="$(ps -C "$proc" -o pid=,pcpu=,rss= 2>/dev/null || true)"; if [[ -z "$rows" ]]; then continue; fi; while read -r pid cpu rss; do [[ -z "$pid" ]] && continue; printf "%s\t%s\t%s\t%s\t%s\n" "$ts" "$proc" "$pid" "$cpu" "$rss" >> "$outfile"; done <<< "$rows"; done; sleep "$sample"; done && echo ">>> soak summary" && for proc in parakeet-ptt parakeet-overlay; do proc_file="$(mktemp)"; awk -F "\t" -v proc="$proc" "\$2 == proc { print \$4 \"\t\" \$5 \"\t\" \$3 }" "$outfile" > "$proc_file"; if [[ ! -s "$proc_file" ]]; then echo "ERROR: missing samples for $proc" >&2; echo "artifact=$outfile"; rm -f "$proc_file"; exit 1; fi; samples="$(wc -l < "$proc_file" | tr -d " ")"; pids="$(awk -F "\t" "{print \$3}" "$proc_file" | sort -u | paste -sd, -)"; cpu_avg="$(awk -F "\t" "{sum += \$1; count += 1} END {printf \"%.2f\", (count > 0 ? sum / count : 0)}" "$proc_file")"; cpu_p95="$(awk -F "\t" "{print \$1}" "$proc_file" | sort -n | awk "{vals[NR]=\$1} END {idx=int((NR * 95 + 99) / 100); if (idx < 1) idx = 1; if (idx > NR) idx = NR; printf \"%.2f\", vals[idx]}")"; cpu_max="$(awk -F "\t" "BEGIN{max=0} {if (\$1 > max) max = \$1} END {printf \"%.2f\", max}" "$proc_file")"; rss_avg_mib="$(awk -F "\t" "{sum += \$2; count += 1} END {printf \"%.1f\", ((count > 0 ? sum / count : 0) / 1024)}" "$proc_file")"; rss_p95_mib="$(awk -F "\t" "{print \$2}" "$proc_file" | sort -n | awk "{vals[NR]=\$1} END {idx=int((NR * 95 + 99) / 100); if (idx < 1) idx = 1; if (idx > NR) idx = NR; printf \"%.1f\", vals[idx] / 1024}")"; rss_max_mib="$(awk -F "\t" "BEGIN{max=0} {if (\$2 > max) max = \$2} END {printf \"%.1f\", max / 1024}" "$proc_file")"; printf "%s: samples=%s pids=%s cpu_avg=%s%% cpu_p95=%s%% cpu_max=%s%% rss_avg=%sMiB rss_p95=%sMiB rss_max=%sMiB\n" "$proc" "$samples" "$pids" "$cpu_avg" "$cpu_p95" "$cpu_max" "$rss_avg_mib" "$rss_p95_mib" "$rss_max_mib"; rm -f "$proc_file"; done && echo "artifact=$outfile"'

overlay-doctor:
    @cd "{{repo_root}}"
    @echo "repo_root=$(pwd -P)"
    @echo "branch=$(git branch --show-current)"
    @echo "rev=$(git rev-parse --short HEAD)"

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
