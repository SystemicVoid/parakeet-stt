#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
GIT_DIR="$(git -C "${REPO_ROOT}" rev-parse --git-dir)"
STATE_FILE="${GIT_DIR}/harness-maintenance.state"
LOG_FILE="${GIT_DIR}/harness-maintenance.log"
DEFAULT_THRESHOLD=10

usage() {
    cat <<'EOF'
Usage:
  scripts/harness-maintenance.sh check [--threshold N]
  scripts/harness-maintenance.sh run
  scripts/harness-maintenance.sh mark

Commands:
  check     Warn when maintenance audits are due (non-blocking, exits 0).
  run       Run maintenance audits (deptry + cargo-udeps), then record current HEAD.
  mark      Record current HEAD as audited without running checks.
EOF
}

log_line() {
    local message="$1"
    printf '%s %s\n' "$(date -Iseconds)" "${message}" >>"${LOG_FILE}"
}

head_sha() {
    git -C "${REPO_ROOT}" rev-parse HEAD
}

read_last_sha() {
    if [[ -f "${STATE_FILE}" ]]; then
        sed -n '1p' "${STATE_FILE}"
        return 0
    fi
    return 1
}

write_state() {
    local sha="$1"
    printf '%s\n' "${sha}" >"${STATE_FILE}"
    log_line "state updated: ${sha}"
}

commit_distance_since() {
    local last_sha="$1"
    if ! git -C "${REPO_ROOT}" cat-file -e "${last_sha}^{commit}" 2>/dev/null; then
        echo "-1"
        return
    fi
    git -C "${REPO_ROOT}" rev-list --count "${last_sha}..HEAD"
}

parse_threshold_arg() {
    local threshold="${DEFAULT_THRESHOLD}"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --threshold)
                if [[ $# -lt 2 ]]; then
                    echo "Missing value for --threshold" >&2
                    exit 2
                fi
                threshold="$2"
                shift 2
                ;;
            *)
                echo "Unknown argument: $1" >&2
                exit 2
                ;;
        esac
    done
    if ! [[ "${threshold}" =~ ^[0-9]+$ ]]; then
        echo "Threshold must be an integer: ${threshold}" >&2
        exit 2
    fi
    printf '%s\n' "${threshold}"
}

run_checks() {
    echo "Running harness maintenance audits..."
    (
        cd "${REPO_ROOT}/parakeet-stt-daemon"
        uv run deptry .
    )
    if ! rustup toolchain list | grep -q '^nightly'; then
        echo "harness-maintenance: nightly toolchain is required for cargo-udeps." >&2
        echo "Install with: rustup toolchain install nightly" >&2
        return 1
    fi
    if ! cargo udeps --help >/dev/null 2>&1; then
        echo "harness-maintenance: cargo-udeps is not installed." >&2
        echo "Install with: cargo install cargo-udeps" >&2
        return 1
    fi
    (
        cd "${REPO_ROOT}"
        cargo +nightly udeps --manifest-path parakeet-ptt/Cargo.toml --all-targets
    )
}

check_due() {
    local threshold="$1"
    local current_sha
    current_sha="$(head_sha)"
    local last_sha
    if ! last_sha="$(read_last_sha)"; then
        echo "harness-maintenance: audit has never been recorded; run 'scripts/harness-maintenance.sh run'."
        log_line "check due: no state (head=${current_sha}, threshold=${threshold})"
        return
    fi

    local distance
    distance="$(commit_distance_since "${last_sha}")"
    if [[ "${distance}" -lt 0 ]]; then
        echo "harness-maintenance: last recorded commit is missing (${last_sha}); run 'scripts/harness-maintenance.sh run'."
        log_line "check due: missing state commit=${last_sha} (head=${current_sha})"
        return
    fi

    if [[ "${distance}" -ge "${threshold}" ]]; then
        echo "harness-maintenance: ${distance} commits since last audit (${last_sha}); run 'scripts/harness-maintenance.sh run'."
        log_line "check due: commits_since=${distance} threshold=${threshold} last=${last_sha} head=${current_sha}"
    fi
}

main() {
    local command="${1:-check}"
    shift || true

    case "${command}" in
        check)
            local threshold
            threshold="$(parse_threshold_arg "$@")"
            check_due "${threshold}"
            ;;
        run)
            if [[ $# -gt 0 ]]; then
                echo "Unexpected arguments for 'run': $*" >&2
                exit 2
            fi
            run_checks
            write_state "$(head_sha)"
            echo "harness-maintenance: audits passed and state updated."
            ;;
        mark)
            if [[ $# -gt 0 ]]; then
                echo "Unexpected arguments for 'mark': $*" >&2
                exit 2
            fi
            write_state "$(head_sha)"
            echo "harness-maintenance: state updated without running audits."
            ;;
        help|-h|--help)
            usage
            ;;
        *)
            echo "Unknown command: ${command}" >&2
            usage >&2
            exit 2
            ;;
    esac
}

main "$@"
