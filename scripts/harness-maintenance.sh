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
  scripts/harness-maintenance.sh code-shape
  scripts/harness-maintenance.sh run
  scripts/harness-maintenance.sh mark

Commands:
  check     Warn when maintenance audits are due (non-blocking, exits 0).
  code-shape
            Report oversized/mixed-responsibility surfaces (warn-only).
  run       Run maintenance audits (code-shape + deptry + cargo-udeps), then record current HEAD.
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

code_shape_find_files() {
    find "${REPO_ROOT}" \
        \( \
            -type d \( \
                -name ".git" -o \
                -name ".venv" -o \
                -name "venv" -o \
                -name "target" -o \
                -name ".ruff_cache" -o \
                -name ".pytest_cache" -o \
                -name ".mypy_cache" -o \
                -name ".ty" -o \
                -name "node_modules" -o \
                -name "__pycache__" -o \
                -name ".cache" -o \
                -name "vendor" \
            \) -o \
            -path "${REPO_ROOT}/docs/archive" \
        \) -prune -o \
        -type f \( -name "*.rs" -o -name "*.py" -o -name "*.sh" \) -print
}

code_shape_category() {
    local relative_path="$1"

    case "${relative_path}" in
        */tests/*|tests/*|test_*.py|*_test.py|*_test.rs|*/test_*.py)
            printf '%s|%s\n' "tests" "700"
            ;;
        scripts/*.sh)
            printf '%s|%s\n' "shell" "800"
            ;;
        parakeet-ptt/src/*.rs)
            printf '%s|%s\n' "production-rust" "900"
            ;;
        parakeet-stt-daemon/src/*.py|parakeet-stt-daemon/check_model.py|parakeet-stt-daemon/check_model_lib/*.py|parakeet-stt-daemon/scripts/*.py)
            printf '%s|%s\n' "production-python" "700"
            ;;
        *)
            return 1
            ;;
    esac
}

code_shape_action() {
    local relative_path="$1"
    local category="$2"

    case "${relative_path}" in
        parakeet-ptt/src/app.rs)
            echo "Client runtime owner: keep extracting deep Session/Overlay/Injection policy Modules with focused tests."
            ;;
        parakeet-ptt/src/overlay_renderer.rs)
            echo "Overlay owner: prefer pure/data-only layout, glyph, animation, or frame-math extractions before Wayland runtime work."
            ;;
        parakeet-ptt/src/injector.rs|parakeet-ptt/src/injector_runtime.rs)
            echo "Injection owner: isolate report, telemetry, subprocess, uinput, or route policy seams without changing #26 timeout policy."
            ;;
        scripts/stt-helper.sh)
            echo "Helper owner: validate source-of-truth rows and env wiring before any split."
            ;;
        parakeet-stt-daemon/src/parakeet_stt_daemon/session_orchestrator.py)
            echo "Daemon owner: extract finalization or Runtime truth seams only when tests need narrower setup."
            ;;
        *)
            case "${category}" in
                production-rust)
                    echo "Rust owner: apply the deletion test; deepen Module/Interface seams before splitting by LOC."
                    ;;
                production-python)
                    echo "Python owner: extract cohesive policy seams only when they improve Locality or test setup."
                    ;;
                shell)
                    echo "Shell owner: audit validation/source-of-truth boundaries before splitting functions."
                    ;;
                tests)
                    echo "Test owner: move reusable fixtures/helpers out before splitting scenario coverage."
                    ;;
                *)
                    echo "Owner: inspect for mixed responsibilities and add a focused follow-up before refactoring."
                    ;;
            esac
            ;;
    esac
}

code_shape_findings() {
    local file
    while IFS= read -r file; do
        local relative_path="${file#"${REPO_ROOT}/"}"
        local category_line
        if ! category_line="$(code_shape_category "${relative_path}")"; then
            continue
        fi

        local category
        local threshold
        IFS="|" read -r category threshold <<<"${category_line}"

        local loc
        loc="$(wc -l <"${file}")"
        loc="${loc//[[:space:]]/}"
        if (( loc < threshold )); then
            continue
        fi

        local action
        action="$(code_shape_action "${relative_path}" "${category}")"
        printf '%s\t%s\t%s\t%s\t%s\n' "${category}" "${loc}" "${threshold}" "${relative_path}" "${action}"
    done < <(code_shape_find_files)
}

run_code_shape_audit() {
    echo "Running code-shape audit (warn-only)..."
    echo "Category | LOC | Threshold | File | Suggested owner/action"
    echo "--- | ---: | ---: | --- | ---"

    local finding_count=0
    while IFS=$'\t' read -r category loc threshold relative_path action; do
        printf '%s | %s | %s | %s | %s\n' "${category}" "${loc}" "${threshold}" "${relative_path}" "${action}"
        finding_count=$((finding_count + 1))
    done < <(code_shape_findings | sort -t $'\t' -k1,1 -k2,2nr -k4,4)

    if (( finding_count == 0 )); then
        echo "No code-shape findings exceeded current warn-only thresholds."
    fi

    log_line "code-shape audit completed: findings=${finding_count}"
}

run_checks() {
    echo "Running harness maintenance audits..."
    run_code_shape_audit
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
        code-shape)
            if [[ $# -gt 0 ]]; then
                echo "Unexpected arguments for 'code-shape': $*" >&2
                exit 2
            fi
            run_code_shape_audit
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
