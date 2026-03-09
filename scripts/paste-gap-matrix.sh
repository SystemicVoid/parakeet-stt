#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
RUN_ROOT="${PARAKEET_PASTE_GAP_RUN_ROOT:-/tmp/parakeet-paste-gap}"
CURRENT_RUN_FILE="${RUN_ROOT}/current-run"
PTT_LOG="/tmp/parakeet-ptt.log"
DAEMON_LOG="/tmp/parakeet-daemon.log"
GHOSTTY_SINK="/tmp/parakeet-ghostty-sink.txt"

usage() {
    cat <<'EOF'
Usage:
  scripts/paste-gap-matrix.sh start --backend <auto|uinput|ydotool> [--label LABEL] [--attempts N]
  scripts/paste-gap-matrix.sh inject-only --backend <auto|uinput|ydotool> [--shortcut <auto|ctrl-v|ctrl-shift-v>] [--label LABEL] [--attempts N] [--prefix TEXT] [--interval-ms N]
  scripts/paste-gap-matrix.sh stop [--run-dir DIR]
  scripts/paste-gap-matrix.sh diag [--run-dir DIR]
  scripts/paste-gap-matrix.sh summarize [--run-dir DIR]
  scripts/paste-gap-matrix.sh current

Commands:
  start      Freeze baseline metadata, clear runtime artifacts, and start STT with one backend.
  inject-only Run repeated test-injection diagnostics for one backend without ASR/hotkey flow.
  stop       Stop STT, archive runtime artifacts, and summarize injector report evidence.
  diag       Run stt diag-injector and capture the control log in the run directory.
  summarize  Rebuild parsed TSV/summary outputs from archived artifacts.
  current    Print the active run directory tracked in /tmp.

Notes:
  - Artifacts are stored under /tmp/parakeet-paste-gap by default.
  - start seeds operator-observation TSV templates; fill them in during the manual Ghostty run.
  - stop/summarize extract fields from 'injector subprocess report' lines in the archived ptt log.
EOF
}

die() {
    echo "paste-gap-matrix: $*" >&2
    exit 1
}

require_repo() {
    [[ -f "${REPO_ROOT}/scripts/stt-helper.sh" ]] || die "repo root not detected at ${REPO_ROOT}"
}

normalize_named_value() {
    local raw="$1"
    local expected_key="$2"
    case "${raw}" in
        "${expected_key}="*)
            printf '%s\n' "${raw#*=}"
            ;;
        *)
            printf '%s\n' "${raw}"
            ;;
    esac
}

validate_backend() {
    case "${1}" in
        auto | uinput | ydotool) ;;
        *)
            die "backend must be one of: auto|uinput|ydotool"
            ;;
    esac
}

validate_shortcut() {
    case "${1}" in
        auto | ctrl-v | ctrl-shift-v) ;;
        *)
            die "shortcut must be one of: auto|ctrl-v|ctrl-shift-v"
            ;;
    esac
}

validate_attempts() {
    [[ "${1}" =~ ^[0-9]+$ ]] || die "attempt count must be an integer"
    (( "$1" >= 1 )) || die "attempt count must be >= 1"
}

ensure_run_root() {
    mkdir -p "${RUN_ROOT}"
}

write_current_run() {
    ensure_run_root
    printf '%s\n' "$1" >"${CURRENT_RUN_FILE}"
}

clear_current_run() {
    rm -f "${CURRENT_RUN_FILE}"
}

resolve_run_dir() {
    if [[ $# -gt 0 && -n "${1}" ]]; then
        printf '%s\n' "$1"
        return 0
    fi
    [[ -f "${CURRENT_RUN_FILE}" ]] || die "no active run; pass --run-dir explicitly or start a run first"
    sed -n '1p' "${CURRENT_RUN_FILE}"
}

run_stt() {
    export PARAKEET_ROOT="${REPO_ROOT}"
    # shellcheck disable=SC1091
    source "${REPO_ROOT}/scripts/stt-helper.sh"
    stt "$@"
}

seed_operator_observations() {
    local path="$1"
    local attempts="$2"
    local utterance_prefix="${3:-raw}"
    {
        printf 'attempt_index,utterance,visible_paste,sink_captured,notes\n'
        local index
        for index in $(seq 1 "${attempts}"); do
            printf '%s,%s %s,,,\n' "${index}" "${utterance_prefix}" "${index}"
        done
    } >"${path}"
}

seed_diag_observations() {
    local path="$1"
    {
        printf 'backend,visible_paste,notes\n'
        printf 'auto,,\n'
        printf 'uinput,,\n'
        printf 'ydotool,,\n'
    } >"${path}"
}

clear_runtime_artifacts() {
    : >"${PTT_LOG}"
    : >"${DAEMON_LOG}"
    : >"${GHOSTTY_SINK}"
}

archive_artifact() {
    local source_path="$1"
    local dest_path="$2"
    if [[ -f "${source_path}" ]]; then
        cp "${source_path}" "${dest_path}"
    else
        printf 'missing: %s\n' "${source_path}" >"${dest_path}.missing"
    fi
}

write_run_metadata() {
    local run_dir="$1"
    local backend="$2"
    local label="$3"
    local attempts="$4"
    local started_at="$5"
    local git_sha
    git_sha="$(git -C "${REPO_ROOT}" rev-parse HEAD)"
    local git_short
    git_short="$(git -C "${REPO_ROOT}" rev-parse --short HEAD)"
    local git_status
    git_status="$(git -C "${REPO_ROOT}" status --short)"

    printf '%s\n' "${git_sha}" >"${run_dir}/git-head.txt"
    printf '%s\n' "${git_status}" >"${run_dir}/git-status.txt"
    {
        printf 'backend=%s\n' "${backend}"
        printf 'label=%s\n' "${label}"
        printf 'attempts=%s\n' "${attempts}"
        printf 'started_at_utc=%s\n' "${started_at}"
        printf 'git_sha=%s\n' "${git_sha}"
        printf 'git_short=%s\n' "${git_short}"
        if [[ -n "${git_status}" ]]; then
            printf 'git_dirty=true\n'
        else
            printf 'git_dirty=false\n'
        fi
    } >"${run_dir}/run-meta.env"
}

start_run() {
    local backend=""
    local label="ghostty"
    local attempts="10"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --backend)
                [[ $# -ge 2 ]] || die "missing value for --backend"
                backend="$(normalize_named_value "$2" "backend")"
                shift 2
                ;;
            --label)
                [[ $# -ge 2 ]] || die "missing value for --label"
                label="$(normalize_named_value "$2" "label")"
                shift 2
                ;;
            --attempts)
                [[ $# -ge 2 ]] || die "missing value for --attempts"
                attempts="$(normalize_named_value "$2" "attempts")"
                shift 2
                ;;
            *)
                die "unknown start argument: $1"
                ;;
        esac
    done

    [[ -n "${backend}" ]] || die "start requires --backend"
    validate_backend "${backend}"
    validate_attempts "${attempts}"
    ensure_run_root

    local started_at
    started_at="$(date -u +%Y%m%dT%H%M%SZ)"
    local run_dir="${RUN_ROOT}/${started_at}-${backend}-${label}"
    mkdir -p "${run_dir}/artifacts"

    write_run_metadata "${run_dir}" "${backend}" "${label}" "${attempts}" "${started_at}"
    seed_operator_observations "${run_dir}/operator-observations.tsv" "${attempts}"
    seed_diag_observations "${run_dir}/diag-observations.tsv"

    run_stt stop >/dev/null 2>&1 || true
    clear_runtime_artifacts
    run_stt start --paste --paste-key-backend "${backend}" --paste-backend-failure-policy error
    write_current_run "${run_dir}"

    cat <<EOF
Run started.

run_dir=${run_dir}
backend=${backend}
attempts=${attempts}
git_sha=$(sed -n '1p' "${run_dir}/git-head.txt")
git_dirty=$(sed -n 's/^git_dirty=//p' "${run_dir}/run-meta.env")

Next:
1. Focus the Ghostty sink target.
2. Perform ${attempts} raw PTT utterances.
3. Mark visible results in:
   ${run_dir}/operator-observations.tsv
4. Finish with:
   scripts/paste-gap-matrix.sh stop
5. Run the control:
   scripts/paste-gap-matrix.sh diag
EOF
}

inject_only_run() {
    local backend=""
    local shortcut="ctrl-shift-v"
    local label="ghostty-inject-only"
    local attempts="20"
    local text_prefix="PG"
    local interval_ms="150"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --backend)
                [[ $# -ge 2 ]] || die "missing value for --backend"
                backend="$(normalize_named_value "$2" "backend")"
                shift 2
                ;;
            --shortcut)
                [[ $# -ge 2 ]] || die "missing value for --shortcut"
                shortcut="$(normalize_named_value "$2" "shortcut")"
                shift 2
                ;;
            --label)
                [[ $# -ge 2 ]] || die "missing value for --label"
                label="$(normalize_named_value "$2" "label")"
                shift 2
                ;;
            --attempts)
                [[ $# -ge 2 ]] || die "missing value for --attempts"
                attempts="$(normalize_named_value "$2" "attempts")"
                shift 2
                ;;
            --prefix)
                [[ $# -ge 2 ]] || die "missing value for --prefix"
                text_prefix="$(normalize_named_value "$2" "prefix")"
                shift 2
                ;;
            --interval-ms)
                [[ $# -ge 2 ]] || die "missing value for --interval-ms"
                interval_ms="$(normalize_named_value "$2" "interval_ms")"
                shift 2
                ;;
            *)
                die "unknown inject-only argument: $1"
                ;;
        esac
    done

    [[ -n "${backend}" ]] || die "inject-only requires --backend"
    validate_backend "${backend}"
    validate_shortcut "${shortcut}"
    validate_attempts "${attempts}"
    [[ "${interval_ms}" =~ ^[0-9]+$ ]] || die "interval-ms must be an integer"
    ensure_run_root

    local started_at
    started_at="$(date -u +%Y%m%dT%H%M%SZ)"
    local run_dir="${RUN_ROOT}/${started_at}-${backend}-${label}"
    mkdir -p "${run_dir}/artifacts"

    write_run_metadata "${run_dir}" "${backend}" "${label}" "${attempts}" "${started_at}"
    {
        printf 'mode=inject_only\n'
        printf 'shortcut=%s\n' "${shortcut}"
        printf 'text_prefix=%s\n' "${text_prefix}"
        printf 'interval_ms=%s\n' "${interval_ms}"
    } >>"${run_dir}/run-meta.env"
    seed_operator_observations "${run_dir}/operator-observations.tsv" "${attempts}" "${text_prefix}"
    seed_diag_observations "${run_dir}/diag-observations.tsv"

    run_stt stop >/dev/null 2>&1 || true
    clear_runtime_artifacts
    run_stt diag-injector \
        --backend "${backend}" \
        --attempts "${attempts}" \
        --shortcut "${shortcut}" \
        --prefix "${text_prefix}" \
        --interval-ms "${interval_ms}" \
        2>&1 | tee "${run_dir}/artifacts/diag-injector.log"
    archive_artifact "${PTT_LOG}" "${run_dir}/artifacts/parakeet-ptt.log"
    archive_artifact "${DAEMON_LOG}" "${run_dir}/artifacts/parakeet-daemon.log"
    archive_artifact "${GHOSTTY_SINK}" "${run_dir}/artifacts/parakeet-ghostty-sink.txt"
    summarize_run --run-dir "${run_dir}"
    write_current_run "${run_dir}"

    cat <<EOF
Inject-only run finished.

run_dir=${run_dir}
backend=${backend}
shortcut=${shortcut}
attempts=${attempts}
text_prefix=${text_prefix}
interval_ms=${interval_ms}

Next:
1. Mark visible/sink observations in:
   ${run_dir}/operator-observations.tsv
2. Rebuild joined summaries after edits:
   scripts/paste-gap-matrix.sh summarize --run-dir "${run_dir}"
EOF
}

stop_run() {
    local run_dir_arg=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --run-dir)
                [[ $# -ge 2 ]] || die "missing value for --run-dir"
                run_dir_arg="$(normalize_named_value "$2" "run_dir")"
                shift 2
                ;;
            *)
                die "unknown stop argument: $1"
                ;;
        esac
    done

    local run_dir
    run_dir="$(resolve_run_dir "${run_dir_arg}")"
    [[ -d "${run_dir}" ]] || die "run directory does not exist: ${run_dir}"

    run_stt stop >/dev/null 2>&1 || true
    archive_artifact "${PTT_LOG}" "${run_dir}/artifacts/parakeet-ptt.log"
    archive_artifact "${DAEMON_LOG}" "${run_dir}/artifacts/parakeet-daemon.log"
    archive_artifact "${GHOSTTY_SINK}" "${run_dir}/artifacts/parakeet-ghostty-sink.txt"
    summarize_run --run-dir "${run_dir}"
    printf 'Archived run to %s\n' "${run_dir}"
}

diag_run() {
    local run_dir_arg=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --run-dir)
                [[ $# -ge 2 ]] || die "missing value for --run-dir"
                run_dir_arg="$(normalize_named_value "$2" "run_dir")"
                shift 2
                ;;
            *)
                die "unknown diag argument: $1"
                ;;
        esac
    done

    local run_dir
    run_dir="$(resolve_run_dir "${run_dir_arg}")"
    [[ -d "${run_dir}" ]] || die "run directory does not exist: ${run_dir}"
    run_stt diag-injector 2>&1 | tee "${run_dir}/artifacts/diag-injector.log"
    printf 'Fill visual results in %s\n' "${run_dir}/diag-observations.tsv"
}

summarize_run() {
    local run_dir_arg=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --run-dir)
                [[ $# -ge 2 ]] || die "missing value for --run-dir"
                run_dir_arg="$(normalize_named_value "$2" "run_dir")"
                shift 2
                ;;
            *)
                die "unknown summarize argument: $1"
                ;;
        esac
    done

    local run_dir
    run_dir="$(resolve_run_dir "${run_dir_arg}")"
    [[ -d "${run_dir}" ]] || die "run directory does not exist: ${run_dir}"

    python3 - "${run_dir}" <<'PY'
from __future__ import annotations

import csv
import json
import re
import sys
from collections import Counter
from pathlib import Path

run_dir = Path(sys.argv[1])
artifacts = run_dir / "artifacts"
ptt_log = artifacts / "parakeet-ptt.log"
diag_log = artifacts / "diag-injector.log"
sink_file = artifacts / "parakeet-ghostty-sink.txt"
meta_path = run_dir / "run-meta.env"
operator_path = run_dir / "operator-observations.tsv"
diag_path = run_dir / "diag-observations.tsv"

meta = {}
if meta_path.exists():
    for raw_line in meta_path.read_text(encoding="utf-8").splitlines():
        if "=" not in raw_line:
            continue
        key, value = raw_line.split("=", 1)
        meta[key] = value

fields = [
    "session",
    "origin",
    "child_session",
    "child_origin",
    "trace_id",
    "outcome",
    "clipboard_ready",
    "post_clipboard_matches",
    "parent_focus_app",
    "child_focus_before_app",
    "child_focus_after_app",
    "child_focus_source_selected",
    "route_focus_source",
    "route_class",
    "route_primary",
    "route_adaptive_fallback",
    "route_reason",
    "backend_attempt_count",
    "backend_warning_tags",
    "backend_exit_statuses",
    "backend_stderr_excerpts",
    "backend_attempts",
    "elapsed_ms_total",
]

pattern = re.compile(r'([A-Za-z0-9_]+)=("([^"\\]|\\.)*"|\S+)')
ansi_pattern = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")


def parse_line(line: str) -> dict[str, str]:
    parsed: dict[str, str] = {}
    cleaned_line = ansi_pattern.sub("", line)
    for key, raw_value, _ in pattern.findall(cleaned_line):
        value = raw_value
        if value.startswith('"') and value.endswith('"'):
            value = bytes(value[1:-1], "utf-8").decode("unicode_escape")
        parsed[key] = value
    return parsed


def scalar(value: object) -> str:
    if value is None:
        return ""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def summarize_backend_attempts_from_json(
    attempts: list[dict[str, object]],
) -> tuple[str, str, str, str]:
    summaries: list[str] = []
    warning_tags: set[str] = set()
    exit_statuses: list[str] = []
    stderr_excerpts: list[str] = []
    for attempt in attempts:
        route_attempt_name = scalar(attempt.get("route_attempt_name")) or "unknown"
        backend = scalar(attempt.get("backend")) or "unknown"
        status = scalar(attempt.get("status")) or "unknown"
        summary = f"{route_attempt_name}:{backend}:{status}"
        duration = attempt.get("duration_ms")
        if duration is not None and scalar(duration):
            summary += f":{scalar(duration)}ms"
        exit_status = scalar(attempt.get("exit_status"))
        if exit_status:
            summary += f":exit={exit_status}"
            exit_statuses.append(f"{backend}:{exit_status}")
        for tag in attempt.get("warning_tags") or []:
            tag_s = scalar(tag)
            if tag_s:
                warning_tags.add(tag_s)
        backend_config = scalar(attempt.get("backend_config"))
        if backend_config:
            summary += f":cfg={backend_config}"
        stderr_excerpt = scalar(attempt.get("stderr_excerpt"))
        if stderr_excerpt:
            summary += f":stderr={stderr_excerpt}"
            stderr_excerpts.append(f"{backend}:{stderr_excerpt}")
        error = scalar(attempt.get("error"))
        if error:
            summary += f":{error}"
        summaries.append(summary)

    return (
        " | ".join(summaries),
        ",".join(sorted(warning_tags)) if warning_tags else "none",
        " | ".join(exit_statuses),
        " | ".join(stderr_excerpts),
    )


rows: list[dict[str, str]] = []
seen_rows: set[tuple[str, ...]] = set()


def append_row(raw_row: dict[str, str]) -> None:
    row = {field: raw_row.get(field, "") for field in fields}
    row_key = tuple(row.get(field, "") for field in fields)
    if row_key in seen_rows:
        return
    seen_rows.add(row_key)
    rows.append(row)


if ptt_log.exists():
    for line in ptt_log.read_text(encoding="utf-8", errors="replace").splitlines():
        if "injector subprocess report" not in line:
            continue
        append_row(parse_line(line))


def parse_child_reports_from_text(text: str, default_origin: str) -> list[dict[str, str]]:
    parsed_rows: list[dict[str, str]] = []
    marker = "PARAKEET_INJECT_REPORT "
    for raw_line in text.splitlines():
        cleaned_line = ansi_pattern.sub("", raw_line)
        idx = cleaned_line.find(marker)
        if idx == -1:
            continue
        payload = cleaned_line[idx + len(marker) :].strip()
        if not payload:
            continue
        try:
            report = json.loads(payload)
        except json.JSONDecodeError:
            continue
        backend_attempts = report.get("backend_attempts")
        attempts = backend_attempts if isinstance(backend_attempts, list) else []
        attempts_summary, warning_summary, exit_summary, stderr_summary = (
            summarize_backend_attempts_from_json(attempts)
        )
        parent_focus = report.get("parent_focus")
        parent_focus_snapshot = parent_focus.get("snapshot") if isinstance(parent_focus, dict) else {}
        child_focus_before = (
            report.get("child_focus_before")
            if isinstance(report.get("child_focus_before"), dict)
            else {}
        )
        child_focus_after = (
            report.get("child_focus_after")
            if isinstance(report.get("child_focus_after"), dict)
            else {}
        )
        parsed_rows.append(
            {
                "session": scalar(report.get("session_id")),
                "origin": scalar(report.get("origin")) or default_origin,
                "child_session": scalar(report.get("session_id")),
                "child_origin": scalar(report.get("origin")) or default_origin,
                "trace_id": scalar(report.get("trace_id")),
                "outcome": scalar(report.get("outcome")),
                "clipboard_ready": scalar(report.get("clipboard_ready")),
                "post_clipboard_matches": scalar(report.get("post_clipboard_matches")),
                "parent_focus_app": scalar(parent_focus_snapshot.get("app_name")),
                "child_focus_before_app": scalar(child_focus_before.get("app_name")),
                "child_focus_after_app": scalar(child_focus_after.get("app_name")),
                "child_focus_source_selected": scalar(report.get("child_focus_source_selected")),
                "route_focus_source": scalar(report.get("route_focus_source")),
                "route_class": scalar(report.get("route_class")),
                "route_primary": scalar(report.get("route_primary")),
                "route_adaptive_fallback": scalar(report.get("route_adaptive_fallback")),
                "route_reason": scalar(report.get("route_reason")),
                "backend_attempt_count": scalar(len(attempts)),
                "backend_warning_tags": warning_summary,
                "backend_exit_statuses": exit_summary,
                "backend_stderr_excerpts": stderr_summary,
                "backend_attempts": attempts_summary,
                "elapsed_ms_total": scalar(report.get("elapsed_ms_total")),
            }
        )
    return parsed_rows


for source_path, default_origin in ((ptt_log, "raw_final_result"), (diag_log, "test_injection")):
    if not source_path.exists():
        continue
    text = source_path.read_text(encoding="utf-8", errors="replace")
    for parsed in parse_child_reports_from_text(text, default_origin):
        append_row(parsed)


all_tsv = run_dir / "injector-subprocess-report.tsv"
raw_tsv = run_dir / "injector-subprocess-report.raw.tsv"

with all_tsv.open("w", encoding="utf-8", newline="") as handle:
    writer = csv.DictWriter(handle, fieldnames=fields, delimiter="\t")
    writer.writeheader()
    for row in rows:
        writer.writerow({field: row.get(field, "") for field in fields})

raw_rows = [row for row in rows if row.get("origin") == "raw_final_result"]
with raw_tsv.open("w", encoding="utf-8", newline="") as handle:
    writer = csv.DictWriter(handle, fieldnames=fields, delimiter="\t")
    writer.writeheader()
    for row in raw_rows:
        writer.writerow({field: row.get(field, "") for field in fields})


def count_values(key: str) -> Counter[str]:
    counter: Counter[str] = Counter()
    for row in rows:
        counter[row.get(key, "<missing>") or "<missing>"] += 1
    return counter


def read_tabular(path: Path) -> list[dict[str, str]]:
    if not path.exists():
        return []
    with path.open("r", encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle)
        return list(reader)


operator_rows = read_tabular(operator_path)
diag_rows = read_tabular(diag_path)

joined_path = run_dir / "raw-observation-joined.tsv"
joined_written = False
joined_source = raw_rows if raw_rows else rows
if joined_source and operator_rows and len(joined_source) == len(operator_rows):
    with joined_path.open("w", encoding="utf-8", newline="") as handle:
        fieldnames = fields + ["utterance", "visible_paste", "sink_captured", "notes"]
        writer = csv.DictWriter(handle, fieldnames=fieldnames, delimiter="\t")
        writer.writeheader()
        for report_row, operator_row in zip(joined_source, operator_rows):
            joined = {field: report_row.get(field, "") for field in fields}
            joined.update(
                {
                    "utterance": operator_row.get("utterance", ""),
                    "visible_paste": operator_row.get("visible_paste", ""),
                    "sink_captured": operator_row.get("sink_captured", ""),
                    "notes": operator_row.get("notes", ""),
                }
            )
            writer.writerow(joined)
    joined_written = True

sink_lines: list[str] = []
if sink_file.exists():
    sink_lines = [line for line in sink_file.read_text(encoding="utf-8", errors="replace").splitlines() if line.strip()]

summary_lines = [
    f"run_dir={run_dir}",
    f"backend={meta.get('backend', '<unknown>')}",
    f"label={meta.get('label', '<unknown>')}",
    f"mode={meta.get('mode', 'ptt_flow')}",
    f"shortcut={meta.get('shortcut', '<auto>')}",
    f"attempts_planned={meta.get('attempts', '<unknown>')}",
    f"git_sha={meta.get('git_sha', '<unknown>')}",
    f"git_dirty={meta.get('git_dirty', '<unknown>')}",
    f"injector_reports_total={len(rows)}",
    f"injector_reports_raw_final_result={len(raw_rows)}",
    f"injector_reports_test_injection={sum(1 for row in rows if row.get('origin') == 'test_injection')}",
    f"ghostty_sink_nonempty_lines={len(sink_lines)}",
    f"operator_observation_rows={len(operator_rows)}",
    f"operator_visible_rows_filled={sum(1 for row in operator_rows if row.get('visible_paste', '').strip())}",
    f"diag_observation_rows={len(diag_rows)}",
    f"diag_visible_rows_filled={sum(1 for row in diag_rows if row.get('visible_paste', '').strip())}",
    f"joined_raw_observations={'yes' if joined_written else 'no'}",
]

for counter_name, counter in (
    ("origins", count_values("origin")),
    ("route_primary", count_values("route_primary")),
    ("route_class", count_values("route_class")),
    ("backend_attempts", count_values("backend_attempts")),
    ("backend_warning_tags", count_values("backend_warning_tags")),
    ("parent_focus_app", count_values("parent_focus_app")),
    ("child_focus_before_app", count_values("child_focus_before_app")),
    ("clipboard_ready", count_values("clipboard_ready")),
    ("post_clipboard_matches", count_values("post_clipboard_matches")),
):
    summary_lines.append(f"{counter_name}:")
    for key, value in sorted(counter.items()):
        summary_lines.append(f"  {key}={value}")

if sink_lines:
    summary_lines.append("ghostty_sink_preview:")
    for line in sink_lines[:10]:
        summary_lines.append(f"  {line}")

(run_dir / "summary.txt").write_text("\n".join(summary_lines) + "\n", encoding="utf-8")
PY

    printf 'Summarized run in %s\n' "${run_dir}"
}

print_current() {
    resolve_run_dir
}

main() {
    require_repo
    local command="${1:-help}"
    shift || true
    case "${command}" in
        start)
            start_run "$@"
            ;;
        inject-only)
            inject_only_run "$@"
            ;;
        stop)
            stop_run "$@"
            ;;
        diag)
            diag_run "$@"
            ;;
        summarize)
            summarize_run "$@"
            ;;
        current)
            print_current
            ;;
        help | -h | --help)
            usage
            ;;
        *)
            die "unknown command: ${command}"
            ;;
    esac
}

main "$@"
