#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
project_root="$(cd "$script_dir/.." && pwd)"

manifest="${project_root}/bench_audio/personal/manifest.jsonl"
output_dir="${project_root}/bench_audio/personal/audio"
sample_rate="16000"
force_overwrite="0"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest)
      if [[ $# -lt 2 ]]; then
        echo "Missing value for --manifest" >&2
        exit 2
      fi
      manifest="$2"
      shift 2
      ;;
    --output-dir)
      if [[ $# -lt 2 ]]; then
        echo "Missing value for --output-dir" >&2
        exit 2
      fi
      output_dir="$2"
      shift 2
      ;;
    --sample-rate)
      if [[ $# -lt 2 ]]; then
        echo "Missing value for --sample-rate" >&2
        exit 2
      fi
      sample_rate="$2"
      shift 2
      ;;
    --force)
      force_overwrite="1"
      shift
      ;;
    -h|--help)
      cat <<'USAGE'
Usage:
  record_personal_clips.sh [--manifest <manifest.jsonl|dir>] [--output-dir <dir>] [--sample-rate 16000] [--force]

Description:
  Interactive recorder for personal benchmark clips. Files are stored as <sample_id>.wav
  under --output-dir.

Defaults:
  --manifest   bench_audio/personal/manifest.jsonl
  --output-dir bench_audio/personal/audio

Controls:
  s or Enter  start/stop/save current sample
  r           re-record current sample (overwrites current file)
  n / p       next / previous sample
  q           quit session
USAGE
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ -d "$manifest" ]]; then
  manifest="${manifest%/}/manifest.jsonl"
fi

if [[ ! -f "$manifest" ]]; then
  echo "Manifest not found: $manifest" >&2
  exit 1
fi

if ! command -v pw-record >/dev/null 2>&1; then
  echo "pw-record is required but was not found in PATH." >&2
  exit 1
fi

if [[ ! -t 0 || ! -t 1 ]]; then
  echo "This recorder requires an interactive TTY terminal." >&2
  exit 1
fi

mkdir -p "$output_dir"
echo "[record] manifest=$manifest"
echo "[record] output_dir=$output_dir"

declare -a sample_ids=()
declare -a prompts=()
while IFS=$'\t' read -r sample_id reference; do
  [[ -z "$sample_id" ]] && continue
  sample_ids+=("$sample_id")
  prompts+=("$reference")
done < <(
  python3 - "$manifest" <<'PY'
import json
import sys
from pathlib import Path

manifest = Path(sys.argv[1])
for raw in manifest.read_text(encoding="utf-8").splitlines():
    line = raw.strip()
    if not line or line.startswith("#"):
        continue
    payload = json.loads(line)
    sample_id = str(payload.get("sample_id", "")).strip()
    reference = str(payload.get("reference", "")).strip()
    if sample_id and reference:
        print(f"{sample_id}\t{reference}")
PY
)

total="${#sample_ids[@]}"
if [[ "$total" -eq 0 ]]; then
  echo "No samples found in manifest: $manifest" >&2
  exit 1
fi

format_duration() {
  local seconds="$1"
  printf "%02d:%02d" "$((seconds / 60))" "$((seconds % 60))"
}

recorded_count() {
  local count=0
  local sid=""
  for sid in "${sample_ids[@]}"; do
    if [[ -f "${output_dir}/${sid}.wav" ]]; then
      ((count += 1))
    fi
  done
  echo "$count"
}

current_index=0
if [[ "$force_overwrite" != "1" ]]; then
  current_index="$total"
  for i in "${!sample_ids[@]}"; do
    if [[ ! -f "${output_dir}/${sample_ids[$i]}.wav" ]]; then
      current_index="$i"
      break
    fi
  done
fi

recorder_pid=""
recorder_tmp_path=""
recorder_out_path=""
recorder_sample_id=""
last_message=""
quit_requested="0"

cleanup() {
  if [[ -n "$recorder_pid" ]]; then
    kill -INT "$recorder_pid" >/dev/null 2>&1 || true
    wait "$recorder_pid" 2>/dev/null || true
  fi
  if [[ -n "$recorder_tmp_path" && -f "$recorder_tmp_path" ]]; then
    rm -f "$recorder_tmp_path"
  fi
}

trap cleanup EXIT INT TERM

render_screen() {
  local index="$1"
  local info="$2"
  local sid="${sample_ids[$index]}"
  local prompt="${prompts[$index]}"
  local out_path="${output_dir}/${sid}.wav"

  clear
  echo "Personal STT Recorder"
  echo "Manifest: $manifest"
  echo "Output:   $output_dir"
  echo "Progress: $(recorded_count)/$total recorded"
  echo
  echo "Sample $((index + 1))/$total: $sid"
  if [[ -f "$out_path" ]]; then
    local bytes
    bytes="$(stat -c%s "$out_path" 2>/dev/null || echo 0)"
    echo "Status: recorded (${bytes} bytes)"
  else
    echo "Status: pending"
  fi
  echo
  echo "Prompt:"
  echo "$prompt"
  echo
  echo "Controls: [s/Enter] record  [r] rerecord  [n] next  [p] previous  [q] quit"
  if [[ -n "$info" ]]; then
    echo
    echo "Info: $info"
  fi
}

start_recording() {
  local index="$1"
  recorder_sample_id="${sample_ids[$index]}"
  recorder_out_path="${output_dir}/${recorder_sample_id}.wav"
  recorder_tmp_path="${recorder_out_path}.tmp"
  rm -f "$recorder_tmp_path"
  pw-record --rate "$sample_rate" --channels 1 --format s16 "$recorder_tmp_path" &
  recorder_pid="$!"
}

stop_recording() {
  local save="$1"
  if [[ -n "$recorder_pid" ]]; then
    kill -INT "$recorder_pid" >/dev/null 2>&1 || true
    wait "$recorder_pid" 2>/dev/null || true
  fi
  recorder_pid=""

  if [[ "$save" == "1" ]]; then
    if [[ -f "$recorder_tmp_path" && -s "$recorder_tmp_path" ]]; then
      mv -f "$recorder_tmp_path" "$recorder_out_path"
      last_message="Saved $recorder_sample_id.wav"
    else
      rm -f "$recorder_tmp_path"
      last_message="No audio captured for $recorder_sample_id; nothing saved."
      return 1
    fi
  else
    rm -f "$recorder_tmp_path"
    last_message="Discarded recording for $recorder_sample_id."
    return 1
  fi
  return 0
}

record_current_sample() {
  local index="$1"
  local allow_overwrite="$2"
  local sid="${sample_ids[$index]}"
  local out_path="${output_dir}/${sid}.wav"

  if [[ -f "$out_path" && "$allow_overwrite" != "1" && "$force_overwrite" != "1" ]]; then
    last_message="$sid already exists. Press 'r' to re-record or 'n' for next."
    return 1
  fi

  start_recording "$index"
  local started_at
  started_at="$(date +%s)"
  echo
  echo "Recording $sid..."
  echo "Press [s] or [Enter] to stop and save, [d] to discard."
  while true; do
    local now elapsed key
    now="$(date +%s)"
    elapsed="$((now - started_at))"
    printf "\rElapsed: %s  " "$(format_duration "$elapsed")"
    key=""
    if read -rsn1 -t 0.2 key; then
      case "$key" in
        s|$'\n')
          printf "\n"
          if stop_recording 1; then
            return 0
          fi
          return 1
          ;;
        d)
          printf "\n"
          stop_recording 0 || true
          return 1
          ;;
        q)
          printf "\n"
          stop_recording 0 || true
          quit_requested="1"
          last_message="Exited during recording."
          return 1
          ;;
      esac
    fi
  done
}

while true; do
  if (( current_index < 0 )); then
    current_index=0
  fi
  if (( current_index >= total )); then
    break
  fi

  render_screen "$current_index" "$last_message"
  last_message=""

  key=""
  read -rsn1 key
  case "$key" in
    s|$'\n')
      if record_current_sample "$current_index" 0; then
        ((current_index += 1))
      fi
      ;;
    r)
      if record_current_sample "$current_index" 1; then
        ((current_index += 1))
      fi
      ;;
    n)
      if (( current_index < total - 1 )); then
        ((current_index += 1))
      else
        last_message="Already at the last sample."
      fi
      ;;
    p)
      if (( current_index > 0 )); then
        current_index=$((current_index - 1))
      else
        last_message="Already at the first sample."
      fi
      ;;
    q)
      quit_requested="1"
      break
      ;;
    *)
      last_message="Unknown key. Use s, r, n, p, or q."
      ;;
  esac

  if [[ "$quit_requested" == "1" ]]; then
    break
  fi
done

echo
echo "Session finished: $(recorded_count)/$total recorded."
echo "Audio directory: $output_dir"
