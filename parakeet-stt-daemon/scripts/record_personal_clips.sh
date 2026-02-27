#!/usr/bin/env bash
set -euo pipefail

manifest=""
output_dir=""
sample_rate="16000"
force_overwrite="0"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest)
      manifest="$2"
      shift 2
      ;;
    --output-dir)
      output_dir="$2"
      shift 2
      ;;
    --sample-rate)
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
  record_personal_clips.sh --manifest <manifest.jsonl> --output-dir <dir> [--sample-rate 16000] [--force]

Description:
  Interactive recorder for personal benchmark clips. For each manifest entry, press Enter to start recording
  and press Enter again to stop. Files are stored as <sample_id>.wav under --output-dir.
USAGE
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$manifest" || -z "$output_dir" ]]; then
  echo "Missing required arguments. Use --help for usage." >&2
  exit 2
fi

if [[ ! -f "$manifest" ]]; then
  echo "Manifest not found: $manifest" >&2
  exit 1
fi

if ! command -v pw-record >/dev/null 2>&1; then
  echo "pw-record is required but was not found in PATH." >&2
  exit 1
fi

mkdir -p "$output_dir"

while IFS=$'\t' read -r sample_id reference; do
  [[ -z "$sample_id" ]] && continue
  out_path="${output_dir}/${sample_id}.wav"

  if [[ -f "$out_path" && "$force_overwrite" != "1" ]]; then
    echo "[skip] $sample_id exists: $out_path (use --force to overwrite)"
    continue
  fi

  echo
  echo "Sample: $sample_id"
  echo "Prompt: $reference"
  read -r -p "Press Enter to start recording..."

  pw-record --rate "$sample_rate" --channels 1 --format s16 "$out_path" &
  recorder_pid=$!
  read -r -p "Recording... Press Enter to stop."
  kill -INT "$recorder_pid" >/dev/null 2>&1 || true
  wait "$recorder_pid" 2>/dev/null || true

  echo "[saved] $out_path"
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
