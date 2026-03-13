#!/usr/bin/env bash
# Parakeet STT helper (tmux-based start/stop). Source this file, then run: stt start

# Resolve repo root at load time so it works even when sourced via symlinks.
__STT_HELPER_PATH="${BASH_SOURCE[0]}"
if command -v readlink >/dev/null 2>&1; then
    __STT_HELPER_PATH="$(readlink -f "$__STT_HELPER_PATH" 2>/dev/null || echo "$__STT_HELPER_PATH")"
fi
__STT_REPO_ROOT_DEFAULT="${PARAKEET_ROOT:-$(cd "$(dirname "$__STT_HELPER_PATH")/.." && pwd)}"

stt() {
    local REPO_ROOT="${PARAKEET_ROOT:-$__STT_REPO_ROOT_DEFAULT}"
    local DAEMON_DIR="$REPO_ROOT/parakeet-stt-daemon"
    local CLIENT_DIR="$REPO_ROOT/parakeet-ptt"
    local LOG_CLIENT="/tmp/parakeet-ptt.log"
    local LOG_DAEMON="/tmp/parakeet-daemon.log"
    local LOG_LLM="/tmp/parakeet-llama-server.log"
    local CLIENT_PID_FILE="/tmp/parakeet-ptt.pid"
    local DAEMON_PID_FILE="/tmp/parakeet-daemon.pid"
    local LLM_PID_FILE="/tmp/parakeet-llama-server.pid"
    local PORT_FILE="/tmp/parakeet-daemon.port"
    local LLM_PORT_FILE="/tmp/parakeet-llama-server.port"
    local TMUX_SESSION="parakeet-stt"
    local TMUX_WINDOW="run"
    local LLM_TMUX_SESSION="parakeet-llm"
    local LLM_TMUX_WINDOW="server"
    local LOCAL_ENV_FILE=""
    local LOCAL_SHELL_FILE=""
    local skip_local_overrides="${_STT_SKIP_LOCAL_OVERRIDES:-0}"

    # Fall back if REPO_ROOT failed to resolve (e.g., unusual sourcing path).
    if [ -z "$REPO_ROOT" ] || [ "$REPO_ROOT" = "/" ]; then
        local helper_path="${BASH_SOURCE[0]:-$__STT_HELPER_PATH}"
        if command -v readlink >/dev/null 2>&1; then
            helper_path="$(readlink -f "$helper_path" 2>/dev/null || echo "$helper_path")"
        fi
        REPO_ROOT="$(cd "$(dirname "$helper_path")/.." && pwd 2>/dev/null)"
    fi
    # Final guard: ensure the repo path actually has the expected subdirs.
    if [ ! -d "$REPO_ROOT/parakeet-stt-daemon" ] || [ ! -d "$REPO_ROOT/parakeet-ptt" ]; then
        echo "stt helper: could not locate repo root (REPO_ROOT='$REPO_ROOT'). Set PARAKEET_ROOT explicitly."
        return 1
    fi
    DAEMON_DIR="$REPO_ROOT/parakeet-stt-daemon"
    CLIENT_DIR="$REPO_ROOT/parakeet-ptt"

    if [ "$skip_local_overrides" != "1" ]; then
        LOCAL_ENV_FILE="$REPO_ROOT/.parakeet-stt.local.env"
        if [ -f "$LOCAL_ENV_FILE" ]; then
            set -a
            . "$LOCAL_ENV_FILE" || {
                local rc=$?
                set +a
                echo "stt helper: failed to load $LOCAL_ENV_FILE"
                return "$rc"
            }
            set +a
        fi

        LOCAL_SHELL_FILE="$REPO_ROOT/.parakeet-stt.local.sh"
        if [ -f "$LOCAL_SHELL_FILE" ]; then
            . "$LOCAL_SHELL_FILE" || {
                local rc=$?
                echo "stt helper: failed to load $LOCAL_SHELL_FILE"
                return "$rc"
            }
        fi
    fi

    local HOST="${PARAKEET_HOST:-127.0.0.1}"
    local PORT="${PARAKEET_PORT:-8765}"
    local DEFAULT_ENDPOINT="ws://${HOST}:${PORT}/ws"
    local default_injection_mode="${PARAKEET_INJECTION_MODE:-paste}"
    local default_paste_backend_failure_policy="${PARAKEET_PASTE_BACKEND_FAILURE_POLICY:-copy-only}"
    local default_uinput_dwell_ms="${PARAKEET_UINPUT_DWELL_MS:-18}"
    local default_paste_seat="${PARAKEET_PASTE_SEAT:-}"
    local default_paste_write_primary="${PARAKEET_PASTE_WRITE_PRIMARY:-false}"
    local default_completion_sound="${PARAKEET_COMPLETION_SOUND:-true}"
    local default_completion_sound_path="${PARAKEET_COMPLETION_SOUND_PATH:-}"
    local default_completion_sound_volume="${PARAKEET_COMPLETION_SOUND_VOLUME:-100}"
    local default_overlay_enabled="${PARAKEET_OVERLAY_ENABLED:-false}"
    local default_overlay_adaptive_width="${PARAKEET_OVERLAY_ADAPTIVE_WIDTH:-true}"
    local default_llm_pre_modifier_key="${PARAKEET_LLM_PRE_MODIFIER_KEY:-KEY_SHIFT}"
    local default_llm_server_host="${PARAKEET_LLM_SERVER_HOST:-127.0.0.1}"
    local default_llm_server_port="${PARAKEET_LLM_SERVER_PORT:-8080}"
    local default_llm_server_bin="${PARAKEET_LLM_SERVER_BIN:-llama-server}"
    local default_llm_server_model_path="${PARAKEET_LLM_SERVER_MODEL_PATH:-}"
    local default_llm_server_model_alias="${PARAKEET_LLM_SERVER_MODEL_ALIAS:-${PARAKEET_LLM_MODEL:-local}}"
    local default_llm_server_ctx_size="${PARAKEET_LLM_SERVER_CTX_SIZE:-16384}"
    local default_llm_server_gpu_layers="${PARAKEET_LLM_SERVER_GPU_LAYERS:-99}"
    local default_llm_server_parallel="${PARAKEET_LLM_SERVER_PARALLEL:-2}"
    local default_llm_server_metrics="${PARAKEET_LLM_SERVER_METRICS:-true}"
    local default_llm_server_extra_args="${PARAKEET_LLM_SERVER_EXTRA_ARGS:-}"
    local default_llm_base_url="${PARAKEET_LLM_BASE_URL:-http://${default_llm_server_host}:${default_llm_server_port}/v1}"
    local default_llm_model="${PARAKEET_LLM_MODEL:-$default_llm_server_model_alias}"
    local default_llm_timeout_seconds="${PARAKEET_LLM_TIMEOUT_SECONDS:-20}"
    local default_llm_max_tokens="${PARAKEET_LLM_MAX_TOKENS:-512}"
    local default_llm_temperature="${PARAKEET_LLM_TEMPERATURE:-0.7}"
    local default_llm_system_prompt="${PARAKEET_LLM_SYSTEM_PROMPT:-You are a concise assistant. Return only the final answer text for direct insertion.}"
    local default_llm_overlay_stream="${PARAKEET_LLM_OVERLAY_STREAM:-true}"
    local default_daemon_streaming_enabled="false"
    local default_daemon_chunk_secs="2.4"
    local default_daemon_right_context_secs="1.6"
    local default_daemon_left_context_secs="10.0"
    local default_daemon_batch_size="32"
    local default_client_ready_timeout_seconds="${PARAKEET_CLIENT_READY_TIMEOUT_SECONDS:-30}"
    # Keep helper builds portable by default; users can opt in to host-specific flags.
    local default_ptt_rustflags="${PARAKEET_PTT_RUSTFLAGS:-}"
    local default_ptt_runner_preference="${PARAKEET_PTT_RUNNER_PREFERENCE:-cargo}"
    local -a start_option_rows=(
        "injection-mode|injection_mode|default_injection_mode|PARAKEET_INJECTION_MODE|Injection mode|<mode>|paste|always|paste"
        "paste-backend-failure-policy|paste_backend_failure_policy|default_paste_backend_failure_policy|PARAKEET_PASTE_BACKEND_FAILURE_POLICY|Stable controls|<v>|copy-only|always|copy-only"
        "uinput-dwell-ms|uinput_dwell_ms|default_uinput_dwell_ms|PARAKEET_UINPUT_DWELL_MS|Stable controls|<n>|18|always|18"
        "paste-seat|paste_seat|default_paste_seat|PARAKEET_PASTE_SEAT|Stable controls|<v>|<unset>|nonempty|"
        "paste-write-primary|paste_write_primary|default_paste_write_primary|PARAKEET_PASTE_WRITE_PRIMARY|Stable controls|<v>|false|always|false"
        "completion-sound|completion_sound|default_completion_sound|PARAKEET_COMPLETION_SOUND|Stable controls|<v>|true|always|true"
        "completion-sound-path|completion_sound_path|default_completion_sound_path|PARAKEET_COMPLETION_SOUND_PATH|Stable controls|<path>|<system default>|nonempty|"
        "completion-sound-volume|completion_sound_volume|default_completion_sound_volume|PARAKEET_COMPLETION_SOUND_VOLUME|Stable controls|<n>|100|always|100"
        "overlay-enabled|overlay_enabled|default_overlay_enabled|PARAKEET_OVERLAY_ENABLED|Stable controls|<v>|false|always|false"
        "overlay-adaptive-width|overlay_adaptive_width|default_overlay_adaptive_width|PARAKEET_OVERLAY_ADAPTIVE_WIDTH|Stable controls|<v>|true|always|true"
        "llm-pre-modifier-key|llm_pre_modifier_key|default_llm_pre_modifier_key|PARAKEET_LLM_PRE_MODIFIER_KEY|Stable controls|<key>|KEY_SHIFT|always|KEY_SHIFT"
        "llm-base-url|llm_base_url|default_llm_base_url|PARAKEET_LLM_BASE_URL|Stable controls|<url>|http://127.0.0.1:8080/v1|always|http://127.0.0.1:8080/v1"
        "llm-model|llm_model|default_llm_model|PARAKEET_LLM_MODEL|Stable controls|<name>|local|always|local"
        "llm-timeout-seconds|llm_timeout_seconds|default_llm_timeout_seconds|PARAKEET_LLM_TIMEOUT_SECONDS|Stable controls|<n>|20|always|20"
        "llm-max-tokens|llm_max_tokens|default_llm_max_tokens|PARAKEET_LLM_MAX_TOKENS|Stable controls|<n>|512|always|512"
        "llm-temperature|llm_temperature|default_llm_temperature|PARAKEET_LLM_TEMPERATURE|Stable controls|<n>|0.7|always|0.7"
        "llm-system-prompt|llm_system_prompt|default_llm_system_prompt|PARAKEET_LLM_SYSTEM_PROMPT|Stable controls|<text>|<assistant prompt>|nonempty|"
        "llm-overlay-stream|llm_overlay_stream|default_llm_overlay_stream|PARAKEET_LLM_OVERLAY_STREAM|Stable controls|<v>|true|always|true"
    )

    export RUST_LOG="${RUST_LOG:-info}"

    local cmd="${1:-start}"
    shift || true
    if [ "$cmd" = "stream" ]; then
        cmd="start"
        set -- streaming "$@"
    elif [ "$cmd" = "off" ]; then
        cmd="start"
        set -- offline "$@"
    elif [ "$cmd" = "on" ]; then
        cmd="start"
        set -- streaming "$@"
    fi

    _apply_launch_profile_defaults() {
        local profile="$1"
        if [ "$profile" = "stream-seal" ]; then
            # Keep "stt" ergonomic for daily use: online stream+seal with overlay,
            # but non-adaptive width so rendering remains predictable.
            if [ -z "${PARAKEET_OVERLAY_ENABLED+x}" ]; then
                default_overlay_enabled="true"
            fi
            if [ -z "${PARAKEET_OVERLAY_ADAPTIVE_WIDTH+x}" ]; then
                default_overlay_adaptive_width="false"
            fi
            return 0
        fi

        if [ "$profile" = "offline" ]; then
            # "stt off" favors fastest accurate offline dictation with no overlay.
            if [ -z "${PARAKEET_OVERLAY_ENABLED+x}" ]; then
                default_overlay_enabled="false"
            fi
            if [ -z "${PARAKEET_OVERLAY_ADAPTIVE_WIDTH+x}" ]; then
                default_overlay_adaptive_width="false"
            fi
        fi
    }

    _start_option_exists() {
        local target="$1"
        local row opt_name
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name _ <<<"$row"
            if [ "$opt_name" = "$target" ]; then
                return 0
            fi
        done
        return 1
    }

    _set_start_option_value() {
        local target="$1"
        local value="$2"
        local row opt_name var_name
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name var_name _ <<<"$row"
            if [ "$opt_name" = "$target" ]; then
                printf -v "$var_name" "%s" "$value"
                return 0
            fi
        done
        return 1
    }

    _load_start_vars_from_defaults() {
        local row var_name default_var_name
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r _ var_name default_var_name _ <<<"$row"
            printf -v "$var_name" "%s" "${!default_var_name}"
        done
    }

    _print_start_option_group() {
        local group_name="$1"
        local row opt_name default_var_name env_name option_group metavar empty_display
        local default_value option_display
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name _ default_var_name env_name option_group metavar empty_display _ <<<"$row"
            [ "$option_group" = "$group_name" ] || continue
            default_value="${!default_var_name}"
            if [ -z "$default_value" ]; then
                default_value="$empty_display"
            fi
            option_display="--$opt_name $metavar"
            printf "  %-35s (default: %s, env: %s)\n" "$option_display" "$default_value" "$env_name"
        done
    }

    _print_start_option_names() {
        local row opt_name
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name _ <<<"$row"
            printf "%s\n" "$opt_name"
        done
    }

    _build_ptt_args() {
        local -n out_ref="$1"
        local include_endpoint="${2:-yes}"
        local row opt_name var_name include_policy
        out_ref=()
        if [ "$include_endpoint" = "yes" ]; then
            out_ref+=(--endpoint "$DEFAULT_ENDPOINT")
        fi
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name var_name _ _ _ _ _ include_policy _ <<<"$row"
            if [ "$include_policy" = "nonempty" ] && [ -z "${!var_name}" ]; then
                continue
            fi
            out_ref+=("--$opt_name" "${!var_name}")
        done
    }

    _build_start_cli_args() {
        local -n out_ref="$1"
        local launch_profile="$2"
        local row opt_name var_name include_policy
        out_ref=()
        case "$launch_profile" in
            offline)
                out_ref+=(offline)
                ;;
            *)
                out_ref+=(streaming)
                ;;
        esac
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name var_name _ _ _ _ _ include_policy _ <<<"$row"
            if [ "$include_policy" = "nonempty" ] && [ -z "${!var_name}" ]; then
                continue
            fi
            out_ref+=("--$opt_name" "${!var_name}")
        done
    }

    _args_to_shell_words() {
        local -n in_ref="$1"
        local out
        printf -v out "%q " "${in_ref[@]}"
        printf "%s" "${out% }"
    }

    _ptt_binary_supports_start_flags() {
        local binary="$1"
        local help_text row opt_name
        help_text="$("$binary" --help 2>&1)" || return 1
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name _ <<<"$row"
            if ! grep -Fq -- "--$opt_name" <<<"$help_text"; then
                return 1
            fi
        done
        return 0
    }

    _ptt_binary_supports_diag_injection_flags() {
        local binary="$1"
        local help_text
        help_text="$("$binary" --help 2>&1)" || return 1
        local required_flag
        for required_flag in \
            --test-injection \
            --test-injection-count \
            --test-injection-text-prefix \
            --test-injection-interval-ms \
            --test-injection-shortcut; do
            if ! grep -Fq -- "$required_flag" <<<"$help_text"; then
                return 1
            fi
        done
        return 0
    }

    _select_client_runner_mode() {
        local binary="$1"
        local runner_preference="$2"

        case "$runner_preference" in
            cargo)
                printf "cargo"
                return 0
                ;;
            release)
                if [ -x "$binary" ] && _ptt_binary_supports_start_flags "$binary"; then
                    printf "release"
                    return 0
                fi
                printf "cargo"
                return 0
                ;;
            *)
                # Defensive fallback for unknown values.
                printf "cargo"
                return 0
                ;;
        esac
    }

    _parse_start_options() {
        while [[ $# -gt 0 ]]; do
            case "$1" in
                --paste)
                    injection_mode="paste"
                    shift
                    ;;
                --copy-only)
                    injection_mode="copy-only"
                    shift
                    ;;
                --help|-h|help)
                    _print_help_start
                    return 2
                    ;;
                --*)
                    local opt_name="${1#--}"
                    if ! _start_option_exists "$opt_name"; then
                        echo "   - Unknown option for 'stt start': $1"
                        echo "   - Run 'stt start --help' to see all supported start options."
                        return 1
                    fi
                    if [[ $# -lt 2 ]]; then
                        echo "   - Missing value for $1"
                        return 1
                    fi
                    _set_start_option_value "$opt_name" "$2"
                    shift 2
                    ;;
                *)
                    echo "   - Unknown option for 'stt start': $1"
                    echo "   - Run 'stt start --help' to see all supported start options."
                    return 1
                    ;;
            esac
        done
        return 0
    }

    _pid_alive() {
        local pid_file="$1"
        [ -f "$pid_file" ] && ps -p "$(cat "$pid_file")" >/dev/null 2>&1
    }

    _socket_ready_once() {
        if command -v nc >/dev/null 2>&1; then
            nc -z "$HOST" "$PORT" 2>/dev/null
            return $?
        fi

        python3 - "$HOST" "$PORT" <<'PY' >/dev/null 2>&1
import socket, sys
host = sys.argv[1]
port = int(sys.argv[2])
s = socket.socket()
s.settimeout(0.5)
try:
    s.connect((host, port))
except Exception:
    sys.exit(1)
else:
    s.close()
    sys.exit(0)
PY
        return $?
    }

    _wait_for_socket() {
        local pid_file="$1"
        local tries="${2:-60}" # 30s with 0.5s sleep
        local ready=0
        for _ in $(seq 1 "$tries"); do
            if _socket_ready_once; then
                ready=1
                break
            fi
            if [ -n "$pid_file" ] && [ -f "$pid_file" ] && ! ps -p "$(cat "$pid_file")" >/dev/null 2>&1; then
                break
            fi
            sleep 0.5
        done
        [ "$ready" -eq 1 ]
    }

    _http_ok_once() {
        local url="$1"
        if command -v curl >/dev/null 2>&1; then
            curl -fsS --max-time 2 "$url" >/dev/null 2>&1
            return $?
        fi

        python3 - "$url" <<'PY' >/dev/null 2>&1
import sys
import urllib.request

req = urllib.request.Request(sys.argv[1], method="GET")
with urllib.request.urlopen(req, timeout=2) as response:
    sys.exit(0 if 200 <= response.status < 300 else 1)
PY
        return $?
    }

    _wait_for_http() {
        local url="$1"
        local pid_file="$2"
        local tries="${3:-60}"
        local ready=0
        for _ in $(seq 1 "$tries"); do
            if _http_ok_once "$url"; then
                ready=1
                break
            fi
            if [ -n "$pid_file" ] && [ -f "$pid_file" ] && ! ps -p "$(cat "$pid_file")" >/dev/null 2>&1; then
                break
            fi
            sleep 0.5
        done
        [ "$ready" -eq 1 ]
    }

    _wait_pid_alive() {
        local pid_file="$1"
        local tries="${2:-8}"
        for _ in $(seq 1 "$tries"); do
            if _pid_alive "$pid_file"; then
                return 0
            fi
            sleep 0.5
        done
        return 1
    }

    _client_build_in_progress() {
        [ -f "$LOG_CLIENT" ] || return 1
        grep -Fq "[helper] running cargo run --release --bin parakeet-ptt" "$LOG_CLIENT" || return 1
        grep -Eq 'Compiling |Finished `release` profile' "$LOG_CLIENT" || return 1
        grep -Fq 'Running `target/release/parakeet-ptt' "$LOG_CLIENT" && return 1
        return 0
    }

    _wait_for_client_ready() {
        local timeout_seconds="${1:-30}"
        local started_at="$SECONDS"
        local max_wait="$timeout_seconds"
        local pid

        while true; do
            pid=$(pgrep -n "[p]arakeet-ptt" || true)
            if [ -n "$pid" ]; then
                echo "$pid" >| "$CLIENT_PID_FILE"
                return 0
            fi

            if [ -f "$LOG_CLIENT" ] && grep -Eq "Starting hotkey loop; press Right Ctrl to talk|Hotkey listeners started for KEY_RIGHTCTRL|Connected to daemon" "$LOG_CLIENT"; then
                pid=$(pgrep -n "[p]arakeet-ptt" || true)
                if [ -n "$pid" ]; then
                    echo "$pid" >| "$CLIENT_PID_FILE"
                    return 0
                fi
            fi

            if [ $((SECONDS - started_at)) -ge "$max_wait" ]; then
                if _client_build_in_progress; then
                    max_wait=$((max_wait + timeout_seconds))
                    echo "   - Client compile still running; extending readiness wait to ${max_wait}s."
                    _log_client "client readiness wait extended to ${max_wait}s while cargo compile is active"
                else
                    return 1
                fi
            fi

            sleep 0.5
        done
    }

    _stop_pid() {
        local pid_file="$1"
        if ! _pid_alive "$pid_file"; then
            return 1
        fi

        local pid
        pid="$(cat "$pid_file")"
        kill -TERM "$pid" 2>/dev/null || true
        for _ in $(seq 1 10); do
            if ! ps -p "$pid" >/dev/null 2>&1; then
                return 0
            fi
            sleep 0.2
        done

        kill -KILL "$pid" 2>/dev/null || true
        ! ps -p "$pid" >/dev/null 2>&1
    }

    _port_owner() {
        local port="$1"
        if command -v lsof >/dev/null 2>&1; then
            lsof -iTCP:"$port" -sTCP:LISTEN -n -P 2>/dev/null | awk 'NR>1 {print $1 ":" $2; exit}'
        elif command -v ss >/dev/null 2>&1; then
            ss -ltnp "sport = :$port" 2>/dev/null | awk 'NR>1 {print $6}' | sed 's/users://;s/\"//g' | head -n1
        fi
    }

    _listener_pid() {
        local port="$1"
        local pid=""
        if command -v lsof >/dev/null 2>&1; then
            pid="$(lsof -tiTCP:"$port" -sTCP:LISTEN 2>/dev/null | head -n1)"
        elif command -v ss >/dev/null 2>&1; then
            pid="$(ss -ltnp "sport = :$port" 2>/dev/null | sed -n 's/.*pid=\([0-9]\+\).*/\1/p' | head -n1)"
        fi

        if [ -n "$pid" ]; then
            printf "%s" "$pid"
            return 0
        fi
        return 1
    }

    _refresh_daemon_pid_file_from_listener() {
        local listener_pid
        listener_pid="$(_listener_pid "$PORT")" || return 1
        printf "%s\n" "$listener_pid" >| "$DAEMON_PID_FILE"
        return 0
    }

    _refresh_pid_file_from_listener() {
        local port="$1"
        local pid_file="$2"
        local listener_pid
        listener_pid="$(_listener_pid "$port")" || return 1
        printf "%s\n" "$listener_pid" >| "$pid_file"
        return 0
    }

    _tmux_require() {
        if ! command -v tmux >/dev/null 2>&1; then
            echo "${1:-tmux is not installed; install it first (sudo apt install tmux).}"
            return 1
        fi
    }

    _tmux_has_session() {
        command -v tmux >/dev/null 2>&1 && tmux has-session -t "$1" 2>/dev/null
    }

    _tmux_kill_session() {
        if _tmux_has_session "$1"; then
            tmux kill-session -t "$1"
        fi
    }

    _llm_health_url() {
        printf "http://%s:%s/health" "$default_llm_server_host" "$default_llm_server_port"
    }

    _llm_api_base_url() {
        printf "http://%s:%s/v1" "$default_llm_server_host" "$default_llm_server_port"
    }

    _build_llm_server_args() {
        local -n out_ref="$1"
        out_ref=("$default_llm_server_bin")
        if [ -n "$default_llm_server_model_path" ]; then
            out_ref+=(-m "$default_llm_server_model_path")
        fi
        if [ -n "$default_llm_server_model_alias" ]; then
            out_ref+=(--alias "$default_llm_server_model_alias")
        fi
        out_ref+=(
            --host "$default_llm_server_host"
            --port "$default_llm_server_port"
            --ctx-size "$default_llm_server_ctx_size"
            --parallel "$default_llm_server_parallel"
        )
        if [ -n "$default_llm_server_gpu_layers" ]; then
            out_ref+=(--gpu-layers "$default_llm_server_gpu_layers")
        fi
        if [ "$default_llm_server_metrics" = "true" ]; then
            out_ref+=(--metrics)
        fi
        if [ -n "$default_llm_server_extra_args" ]; then
            eval "set -- $default_llm_server_extra_args"
            out_ref+=("$@")
        fi
    }

    _llm_validate_server_config() {
        _tmux_require "   - tmux is required for 'stt llm'. Install with: sudo apt install tmux" || return 1
        if ! command -v "$default_llm_server_bin" >/dev/null 2>&1; then
            echo "   - LLM server binary '$default_llm_server_bin' was not found in PATH."
            return 1
        fi
        if ! [[ "$default_llm_server_port" =~ ^[0-9]+$ ]] || [ "$default_llm_server_port" -lt 1 ] || [ "$default_llm_server_port" -gt 65535 ]; then
            echo "   - Invalid PARAKEET_LLM_SERVER_PORT='$default_llm_server_port'."
            return 1
        fi
        if [ -n "$default_llm_server_model_path" ] && [ ! -f "$default_llm_server_model_path" ]; then
            echo "   - LLM model path not found: $default_llm_server_model_path"
            return 1
        fi
        if [ -z "$default_llm_server_model_path" ] && [ -z "$default_llm_server_extra_args" ]; then
            echo "   - Set PARAKEET_LLM_SERVER_MODEL_PATH (recommended) or PARAKEET_LLM_SERVER_EXTRA_ARGS before 'stt llm start'."
            return 1
        fi
        if [ "$default_llm_base_url" != "$(_llm_api_base_url)" ]; then
            echo "   - PARAKEET_LLM_BASE_URL is '$default_llm_base_url' but managed llama-server expects '$(_llm_api_base_url)'."
            echo "   - Unset PARAKEET_LLM_BASE_URL or align PARAKEET_LLM_SERVER_HOST/PORT before using 'stt llm'."
            return 1
        fi
        return 0
    }

    _ensure_llm_server() {
        local llm_health_url owner
        local -a llm_cmd llm_tmux_cmd

        if ! _llm_validate_server_config; then
            return 1
        fi

        llm_health_url="$(_llm_health_url)"
        owner="$(_port_owner "$default_llm_server_port")"
        if [ -n "$owner" ] && ! grep -qi "llama" <<<"$owner"; then
            echo "   - LLM port $default_llm_server_port is in use by $owner."
            echo "   - Stop that process or set PARAKEET_LLM_SERVER_PORT to a free port."
            return 1
        fi

        if _http_ok_once "$llm_health_url"; then
            _refresh_pid_file_from_listener "$default_llm_server_port" "$LLM_PID_FILE" >/dev/null 2>&1 || true
            printf "%s:%s\n" "$default_llm_server_host" "$default_llm_server_port" >| "$LLM_PORT_FILE"
            echo "   - LLM server already healthy on $llm_health_url"
            return 0
        fi

        _tmux_kill_session "$LLM_TMUX_SESSION"
        if _pid_alive "$LLM_PID_FILE"; then
            _stop_pid "$LLM_PID_FILE" >/dev/null 2>&1 || true
        fi
        rm -f "$LLM_PID_FILE"

        _build_llm_server_args llm_cmd
        if [ "${#llm_cmd[@]}" -eq 0 ] || [ -z "${llm_cmd[0]}" ]; then
            echo "   - Internal error: managed llama command is empty."
            return 1
        fi

        llm_tmux_cmd=(
            env "LOG_LLM=$LOG_LLM" bash -lc
            'echo "[helper] exec $*" >> "$LOG_LLM"; exec "$@" >> "$LOG_LLM" 2>&1'
            _
            "${llm_cmd[@]}"
        )

        echo "--- LLM session start: $(date -Is) ---" >> "$LOG_LLM"
        echo "[$(date -Is)] [helper] managed llama-server start" >> "$LOG_LLM"

        tmux new-session -d -s "$LLM_TMUX_SESSION" -n "$LLM_TMUX_WINDOW" -c "$REPO_ROOT" \
            "$(_args_to_shell_words llm_tmux_cmd)"

        echo -n "   - Waiting for llama-server health..."
        if _wait_for_http "$llm_health_url" "$LLM_PID_FILE" 120; then
            _refresh_pid_file_from_listener "$default_llm_server_port" "$LLM_PID_FILE" >/dev/null 2>&1 || true
            printf "%s:%s\n" "$default_llm_server_host" "$default_llm_server_port" >| "$LLM_PORT_FILE"
            echo " OK"
            return 0
        fi

        echo " not ready; last llama log lines:"
        tail -n 80 "$LOG_LLM"
        return 1
    }

    _stop_llm_server() {
        local stopped=0
        if _tmux_has_session "$LLM_TMUX_SESSION"; then
            _tmux_kill_session "$LLM_TMUX_SESSION"
            stopped=1
        fi
        if _http_ok_once "$(_llm_health_url)"; then
            _refresh_pid_file_from_listener "$default_llm_server_port" "$LLM_PID_FILE" >/dev/null 2>&1 || true
        fi
        if _pid_alive "$LLM_PID_FILE"; then
            if _stop_pid "$LLM_PID_FILE"; then
                stopped=1
            fi
        fi
        rm -f "$LLM_PID_FILE" "$LLM_PORT_FILE"
        [ "$stopped" -eq 1 ]
    }

    _find_free_port() {
        local start="$1"
        local end=$((start + 10))
        local candidate owner
        for candidate in $(seq "$start" "$end"); do
            owner=$(_port_owner "$candidate")
            if [ -z "$owner" ]; then
                echo "$candidate"
                return 0
            fi
        done
        return 1
    }

    _resolve_port() {
        local env_port_set="${PARAKEET_PORT+set}"
        local owner
        owner=$(_port_owner "$PORT")
        if [ -n "$owner" ] && ! grep -qi "parakeet" <<<"$owner"; then
            if [ "$env_port_set" = "set" ]; then
                echo "   - Port $PORT is in use by $owner; stop it or set PARAKEET_PORT to a free port."
                return 1
            fi
            local next_port
            next_port=$(_find_free_port "$((PORT + 1))")
            if [ -z "$next_port" ]; then
                echo "   - Port $PORT is in use by $owner; no alternate port found near $PORT."
                return 1
            fi
            echo "   - Port $PORT busy (owner: $owner); switching to $next_port."
            PORT="$next_port"
        fi
        DEFAULT_ENDPOINT="ws://${HOST}:${PORT}/ws"
        export PARAKEET_HOST="$HOST"
        export PARAKEET_PORT="$PORT"
        return 0
    }

    _log_client() { echo "[$(date -Is)] $*" >> "$LOG_CLIENT"; }
    _log_daemon() { echo "[$(date -Is)] $*" >> "$LOG_DAEMON"; }
    _print_help_main() {
        cat <<EOF
Usage:
  stt start [options]
  stt stream [options]
  stt off [options]
  stt <command> [args]

Commands:
  start [options]        Start daemon + client (default command).
  stream [options]       Start daemon/client in stream+seal mode.
  off [options]          Start daemon/client in offline mode (overlay off).
  llm [args]             Start/stop/status the managed llama + STT stack.
  stop                   Stop daemon/client and remove pid/port files.
  restart [options]      Restart with the same options as start.
  status                 Show daemon/client/tmux status.
  logs [client|daemon|both]
                         Tail logs (default: both).
  show | attach          Attach to tmux session.
  tmux [attach|kill]     Attach/kill helper tmux session.
  check                  Run daemon health check.
  diag-injector [opts]   Run clipboard injector diagnostics.
  help [start|llm]       Show this help or command-specific help.
EOF
    }
    _print_help_start() {
        _apply_launch_profile_defaults "stream-seal"
        cat <<EOF
Usage:
  stt start [streaming|offline] [options]

Modes:
  (default) streaming    Launch daemon with stream+seal + overlay defaults.
  streaming|stream       Launch daemon with stream+seal enabled.
  offline|off            Launch daemon with streaming disabled.

Injection mode:
  --paste                              Alias for --injection-mode paste
  --copy-only                          Alias for --injection-mode copy-only
EOF
        _print_start_option_group "Injection mode"
        echo
        echo "Stable controls:"
        _print_start_option_group "Stable controls"
        cat <<EOF

Other environment overrides:
  PARAKEET_HOST=$HOST
  PARAKEET_PORT=$PORT
  PARAKEET_CLIENT_READY_TIMEOUT_SECONDS=$default_client_ready_timeout_seconds
  PARAKEET_PTT_RUSTFLAGS="$default_ptt_rustflags"
  PARAKEET_PTT_RUNNER_PREFERENCE=$default_ptt_runner_preference
  PARAKEET_OVERLAY_ENABLED=<true|false>
  PARAKEET_OVERLAY_MODE=<auto|layer-shell|fallback-window|disabled>
EOF
    }

    _print_help_llm() {
        cat <<EOF
Usage:
  stt llm [streaming|offline] [stt-start-options...]
  stt llm start [streaming|offline] [stt-start-options...]
  stt llm stop
  stt llm restart [streaming|offline] [stt-start-options...]
  stt llm status
  stt llm logs
  stt llm show

Behavior:
  stt llm               Start managed llama-server, then delegate to 'stt start'.
  stt llm stop          Stop both the STT stack and the managed llama-server.
  stt llm status        Show managed llama status, then normal STT status.
  stt llm logs          Tail /tmp/parakeet-llama-server.log.
  stt llm show          Attach to the llama tmux session.

Managed llama configuration (use shell env or $LOCAL_ENV_FILE):
  PARAKEET_LLM_SERVER_BIN=$default_llm_server_bin
  PARAKEET_LLM_SERVER_MODEL_PATH=<path-to-model.gguf>
  PARAKEET_LLM_SERVER_MODEL_ALIAS=$default_llm_server_model_alias
  PARAKEET_LLM_SERVER_HOST=$default_llm_server_host
  PARAKEET_LLM_SERVER_PORT=$default_llm_server_port
  PARAKEET_LLM_SERVER_CTX_SIZE=$default_llm_server_ctx_size
  PARAKEET_LLM_SERVER_GPU_LAYERS=$default_llm_server_gpu_layers
  PARAKEET_LLM_SERVER_PARALLEL=$default_llm_server_parallel
  PARAKEET_LLM_SERVER_METRICS=$default_llm_server_metrics
  PARAKEET_LLM_SERVER_EXTRA_ARGS=<shell-quoted extra args>

Resolved client wiring:
  PARAKEET_LLM_BASE_URL=$default_llm_base_url
  PARAKEET_LLM_MODEL=$default_llm_model

Notes:
  - Keep workstation-specific llama settings in the ignored repo-local files.
  - 'stt llm' refuses mismatched LLM_BASE_URL vs managed host/port to avoid split-brain config.
EOF
    }

    _declare_start_vars() {
        # Declare all start option local variables at once.
        local row var_name
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r _ var_name _ <<<"$row"
            local "$var_name"
        done
    }

    _build_client_cmd() {
        cat <<'CLIENTCMD'
                set -e
                runner_bin=""
                if [ "${RUNNER_MODE:-cargo}" = "release" ] && [ -x ./target/release/parakeet-ptt ]; then
                    runner_bin="./target/release/parakeet-ptt"
                else
                    echo "[helper] running cargo run --release --bin parakeet-ptt" >> "$LOG_CLIENT"
                fi

                eval "set -- $PTT_ARGS_SHELL"
                args=("$@")

                if [ -n "$runner_bin" ]; then
                    "$runner_bin" "${args[@]}" 2>&1 | tee -a "$LOG_CLIENT"
                else
                    RUSTFLAGS="${PTT_RUSTFLAGS}" cargo run --release --bin parakeet-ptt -- "${args[@]}" 2>&1 | tee -a "$LOG_CLIENT"
                fi
CLIENTCMD
    }

    case "$cmd" in
        help|--help|-h)
            case "${1:-}" in
                ""|all)
                    _print_help_main
                    ;;
                start)
                    _print_help_start
                    ;;
                llm)
                    _print_help_llm
                    ;;
                *)
                    echo "Unknown help topic: $1"
                    echo
                    _print_help_main
                    return 1
                    ;;
            esac
            ;;
        __start-option-names)
            _print_start_option_names
            ;;
        __start-args)
            local injection_mode paste_backend_failure_policy
            local uinput_dwell_ms paste_seat paste_write_primary
            local completion_sound completion_sound_path completion_sound_volume overlay_enabled overlay_adaptive_width
            local llm_pre_modifier_key llm_base_url llm_model llm_timeout_seconds llm_max_tokens llm_temperature llm_system_prompt llm_overlay_stream
            local -a ptt_args
            local launch_profile="stream-seal"
            if [ "${1:-}" = "stream" ] || [ "${1:-}" = "streaming" ] || [ "${1:-}" = "on" ]; then
                launch_profile="stream-seal"
                shift
            elif [ "${1:-}" = "offline" ] || [ "${1:-}" = "off" ]; then
                launch_profile="offline"
                shift
            fi
            _apply_launch_profile_defaults "$launch_profile"
            _load_start_vars_from_defaults

            local parse_status=0
            _parse_start_options "$@" || parse_status=$?
            if [ "$parse_status" -eq 2 ]; then
                return 0
            elif [ "$parse_status" -ne 0 ]; then
                return "$parse_status"
            fi

            _build_ptt_args ptt_args
            printf "%s\n" "${ptt_args[@]}"
            ;;
        __start-cli-args)
            local injection_mode paste_backend_failure_policy
            local uinput_dwell_ms paste_seat paste_write_primary
            local completion_sound completion_sound_path completion_sound_volume overlay_enabled overlay_adaptive_width
            local llm_pre_modifier_key llm_base_url llm_model llm_timeout_seconds llm_max_tokens llm_temperature llm_system_prompt llm_overlay_stream
            local -a start_cli_args
            local launch_profile="stream-seal"
            if [ "${1:-}" = "stream" ] || [ "${1:-}" = "streaming" ] || [ "${1:-}" = "on" ]; then
                launch_profile="stream-seal"
                shift
            elif [ "${1:-}" = "offline" ] || [ "${1:-}" = "off" ]; then
                launch_profile="offline"
                shift
            fi
            _apply_launch_profile_defaults "$launch_profile"
            _load_start_vars_from_defaults

            local parse_status=0
            _parse_start_options "$@" || parse_status=$?
            if [ "$parse_status" -eq 2 ]; then
                return 0
            elif [ "$parse_status" -ne 0 ]; then
                return "$parse_status"
            fi

            _build_start_cli_args start_cli_args "$launch_profile"
            printf "%s\n" "${start_cli_args[@]}"
            ;;
        start)
            local injection_mode paste_backend_failure_policy
            local uinput_dwell_ms paste_seat paste_write_primary
            local completion_sound completion_sound_path completion_sound_volume overlay_enabled overlay_adaptive_width
            local llm_pre_modifier_key llm_base_url llm_model llm_timeout_seconds llm_max_tokens llm_temperature llm_system_prompt llm_overlay_stream
            local launch_profile="stream-seal"
            if [ "${1:-}" = "stream" ] || [ "${1:-}" = "streaming" ] || [ "${1:-}" = "on" ]; then
                launch_profile="stream-seal"
                shift
            elif [ "${1:-}" = "offline" ] || [ "${1:-}" = "off" ]; then
                launch_profile="offline"
                shift
            fi
            local daemon_streaming_enabled="$default_daemon_streaming_enabled"
            local daemon_chunk_secs="$default_daemon_chunk_secs"
            local daemon_right_context_secs="$default_daemon_right_context_secs"
            local daemon_left_context_secs="$default_daemon_left_context_secs"
            local daemon_batch_size="$default_daemon_batch_size"
            local daemon_overlay_events_enabled="false"
            if [ "$launch_profile" = "stream-seal" ]; then
                daemon_streaming_enabled="true"
            fi
            local client_ready_timeout_seconds="$default_client_ready_timeout_seconds"
            local ptt_rustflags="$default_ptt_rustflags"
            local ptt_runner_preference="$default_ptt_runner_preference"
            _apply_launch_profile_defaults "$launch_profile"
            _load_start_vars_from_defaults

            local parse_status=0
            _parse_start_options "$@" || parse_status=$?
            if [ "$parse_status" -eq 2 ]; then
                return 0
            elif [ "$parse_status" -ne 0 ]; then
                return "$parse_status"
            fi

            # Keep daemon overlay event emission in lockstep with the user-facing
            # --overlay-enabled start control so one switch enables both paths.
            daemon_overlay_events_enabled="$overlay_enabled"

            if ! [[ "$client_ready_timeout_seconds" =~ ^[0-9]+$ ]] || [ "$client_ready_timeout_seconds" -lt 1 ]; then
                echo "   - Invalid PARAKEET_CLIENT_READY_TIMEOUT_SECONDS='$client_ready_timeout_seconds'; defaulting to 30."
                client_ready_timeout_seconds=30
            fi

            echo ">>> Starting Parakeet STT (detached tmux)..."
            echo "   - Injection mode: $injection_mode"
            echo "   - Paste backend failure policy: $paste_backend_failure_policy"
            echo "   - uinput dwell (ms): $uinput_dwell_ms"
            echo "   - Paste seat: ${paste_seat:-<default>}"
            echo "   - Paste write primary: $paste_write_primary"
            echo "   - Completion sound: $completion_sound"
            echo "   - Completion sound path: ${completion_sound_path:-<system default>}"
            echo "   - Completion sound volume: $completion_sound_volume"
            echo "   - Overlay enabled: $overlay_enabled"
            echo "   - Overlay adaptive width: $overlay_adaptive_width"
            echo "   - LLM pre-modifier key: $llm_pre_modifier_key"
            echo "   - LLM base URL: $llm_base_url"
            echo "   - LLM model: $llm_model"
            echo "   - LLM timeout (s): $llm_timeout_seconds"
            echo "   - LLM max tokens: $llm_max_tokens"
            echo "   - LLM temperature: $llm_temperature"
            echo "   - LLM overlay stream: $llm_overlay_stream"
            echo "   - Launch profile: $launch_profile"
            echo "   - Daemon streaming enabled: $daemon_streaming_enabled"
            echo "   - Daemon overlay events enabled: $daemon_overlay_events_enabled"
            echo "   - Daemon chunk/right/left/batch: ${daemon_chunk_secs}/${daemon_right_context_secs}/${daemon_left_context_secs}/${daemon_batch_size}"
            echo "   - Client ready timeout (s): $client_ready_timeout_seconds"
            echo "   - PTT runner preference: $ptt_runner_preference"
            echo "   - PTT RUSTFLAGS: $ptt_rustflags"
            echo "   - Overlay mode override: ${PARAKEET_OVERLAY_MODE:-auto}"

            if [ "$ptt_runner_preference" != "cargo" ] && [ "$ptt_runner_preference" != "release" ]; then
                echo "   - Invalid PARAKEET_PTT_RUNNER_PREFERENCE='$ptt_runner_preference'; defaulting to cargo."
                ptt_runner_preference="cargo"
            fi

            if ! _resolve_port; then
                return 1
            fi

            local daemon_reused=0
            if _pid_alive "$DAEMON_PID_FILE"; then
                if _socket_ready_once; then
                    _refresh_daemon_pid_file_from_listener >/dev/null 2>&1 || true
                    echo "   - Daemon already running (pid $(cat "$DAEMON_PID_FILE"))."
                    daemon_reused=1
                else
                    echo "   - Daemon pid $(cat "$DAEMON_PID_FILE") is stale (socket not ready); restarting."
                    _stop_pid "$DAEMON_PID_FILE" >/dev/null 2>&1 || true
                    rm -f "$DAEMON_PID_FILE"
                fi
            fi

            if [ "$daemon_reused" -ne 1 ]; then
                echo "   - Launching daemon..."
                _log_daemon "launch via stt helper (streaming=${daemon_streaming_enabled})"
                (
                    cd "$DAEMON_DIR" || exit 1
                    PARAKEET_STREAMING_ENABLED="$daemon_streaming_enabled" \
                    PARAKEET_CHUNK_SECS="$daemon_chunk_secs" \
                    PARAKEET_RIGHT_CONTEXT_SECS="$daemon_right_context_secs" \
                    PARAKEET_LEFT_CONTEXT_SECS="$daemon_left_context_secs" \
                    PARAKEET_BATCH_SIZE="$daemon_batch_size" \
                    PARAKEET_OVERLAY_EVENTS_ENABLED="$daemon_overlay_events_enabled" \
                    PARAKEET_HOST="$HOST" PARAKEET_PORT="$PORT" \
                    setsid uv run parakeet-stt-daemon --host "$HOST" --port "$PORT" </dev/null >> "$LOG_DAEMON" 2>&1 &
                    echo $! >| "$DAEMON_PID_FILE"
                )
            fi

            echo -n "   - Waiting for socket..."
            if _wait_for_socket "$DAEMON_PID_FILE" 60; then
                _refresh_daemon_pid_file_from_listener >/dev/null 2>&1 || true
                echo " OK"
                echo "${HOST}:${PORT}" >| "$PORT_FILE"
            else
                echo " not ready; last daemon log lines:"
                tail -n 80 "$LOG_DAEMON"
                return 1
            fi

            _tmux_require "   - tmux is required for the default start path. Install with: sudo apt install tmux" || return 1
            if [ ! -x "$CLIENT_DIR/target/release/parakeet-ptt" ] && ! command -v cargo >/dev/null 2>&1; then
                echo "   - Release binary missing and 'cargo' not found. Build the client first."
                return 1
            fi

            if pgrep -f "[p]arakeet-ptt" >/dev/null 2>&1; then
                echo "   - Stopping existing parakeet-ptt processes..."
                pkill -f "[p]arakeet-ptt" >/dev/null 2>&1 || true
            fi

            echo "--- Session Start: $(date) ---" >> "$LOG_CLIENT"
            _log_client "start client in tmux (mode: $injection_mode)"

            _tmux_kill_session "$TMUX_SESSION"

            local -a ptt_args
            _build_ptt_args ptt_args
            local ptt_args_shell
            ptt_args_shell="$(_args_to_shell_words ptt_args)"

            local runner_mode
            runner_mode="$(_select_client_runner_mode "$CLIENT_DIR/target/release/parakeet-ptt" "$ptt_runner_preference")"
            if [ "$ptt_runner_preference" = "release" ] && [ "$runner_mode" = "cargo" ] && [ -x "$CLIENT_DIR/target/release/parakeet-ptt" ]; then
                echo "[helper] release binary missing expected start flags; falling back to cargo run --release --bin parakeet-ptt" >> "$LOG_CLIENT"
            fi

            local client_cmd
            client_cmd="$(_build_client_cmd)"

            tmux new-session -d -s "$TMUX_SESSION" -n "$TMUX_WINDOW" -c "$CLIENT_DIR" \
                "LOG_CLIENT=\"$LOG_CLIENT\" RUNNER_MODE=\"$runner_mode\" PTT_RUSTFLAGS=\"$ptt_rustflags\" PTT_ARGS_SHELL=\"$ptt_args_shell\" RUST_LOG=\"$RUST_LOG\" PARAKEET_OVERLAY_MODE=\"${PARAKEET_OVERLAY_MODE:-}\" bash -lc '$client_cmd'"
            tmux split-window -t "$TMUX_SESSION:$TMUX_WINDOW" -v -c /tmp "bash -lc 'tail -f \"$LOG_DAEMON\" \"$LOG_CLIENT\"'"
            tmux select-layout -t "$TMUX_SESSION:$TMUX_WINDOW" even-vertical
            local primary_pane
            primary_pane="$(tmux list-panes -t "$TMUX_SESSION:$TMUX_WINDOW" -F '#{pane_id}' | head -n1)"
            if [ -n "$primary_pane" ]; then
                tmux select-pane -t "$primary_pane"
            fi

            if ! _wait_for_client_ready "$client_ready_timeout_seconds"; then
                if _client_build_in_progress; then
                    echo "   - Client compile still in progress after timeout window; recent client log:"
                else
                    echo "   - Client did not become ready; recent client log:"
                fi
                tail -n 120 "$LOG_CLIENT"
                return 1
            fi

            echo "   - Dictation ready (tmux session: $TMUX_SESSION)."
            echo "     Use 'stt show' to view panes; Ctrl+b d to detach."
            ;;
        llm)
            local llm_action="${1:-start}"
            case "$llm_action" in
                help|--help|-h)
                    _print_help_llm
                    ;;
                start|"")
                    [ "$llm_action" = "start" ] && shift
                    local llm_base_url
                    echo ">>> Starting managed llama + Parakeet STT..."
                    echo "   - LLM binary: $default_llm_server_bin"
                    echo "   - LLM model path: ${default_llm_server_model_path:-<unset>}"
                    echo "   - LLM model alias: $default_llm_server_model_alias"
                    echo "   - LLM API base URL: $(_llm_api_base_url)"
                    echo "   - LLM context/gpu/parallel: ${default_llm_server_ctx_size}/${default_llm_server_gpu_layers}/${default_llm_server_parallel}"
                    echo "   - LLM metrics: $default_llm_server_metrics"
                    if ! _ensure_llm_server; then
                        return 1
                    fi
                    if ! llm_base_url="$(_llm_api_base_url)"; then
                        echo "   - Failed to resolve managed LLM API base URL."
                        return 1
                    fi
                    if [ -z "$llm_base_url" ]; then
                        echo "   - Managed LLM API base URL is empty."
                        return 1
                    fi
                    export PARAKEET_LLM_BASE_URL="$llm_base_url"
                    export PARAKEET_LLM_MODEL="$default_llm_server_model_alias"
                    local _STT_SKIP_LOCAL_OVERRIDES=1
                    stt start "$@"
                    ;;
                stream|streaming|offline|off|on|--*)
                    local llm_base_url
                    echo ">>> Starting managed llama + Parakeet STT..."
                    echo "   - LLM binary: $default_llm_server_bin"
                    echo "   - LLM model path: ${default_llm_server_model_path:-<unset>}"
                    echo "   - LLM model alias: $default_llm_server_model_alias"
                    echo "   - LLM API base URL: $(_llm_api_base_url)"
                    if ! _ensure_llm_server; then
                        return 1
                    fi
                    if ! llm_base_url="$(_llm_api_base_url)"; then
                        echo "   - Failed to resolve managed LLM API base URL."
                        return 1
                    fi
                    if [ -z "$llm_base_url" ]; then
                        echo "   - Managed LLM API base URL is empty."
                        return 1
                    fi
                    export PARAKEET_LLM_BASE_URL="$llm_base_url"
                    export PARAKEET_LLM_MODEL="$default_llm_server_model_alias"
                    local _STT_SKIP_LOCAL_OVERRIDES=1
                    stt start "$llm_action" "$@"
                    ;;
                restart)
                    shift
                    stt llm stop >/dev/null 2>&1 || true
                    stt llm start "$@"
                    ;;
                stop)
                    shift
                    stt stop
                    echo ">>> Stopping managed llama-server..."
                    if _stop_llm_server; then
                        echo "   - LLM server stopped"
                    else
                        echo "   - LLM server not running"
                    fi
                    ;;
                status)
                    local llm_health_url
                    llm_health_url="$(_llm_health_url)"
                    echo ">>> Managed LLM:"
                    if _http_ok_once "$llm_health_url" && _refresh_pid_file_from_listener "$default_llm_server_port" "$LLM_PID_FILE" >/dev/null 2>&1; then
                        :
                    fi
                    if _pid_alive "$LLM_PID_FILE"; then
                        echo "   - LLM server running (pid $(cat "$LLM_PID_FILE"))"
                    else
                        echo "   - LLM server not running"
                    fi
                    echo "   - LLM API base URL: $(_llm_api_base_url)"
                    echo "   - LLM health URL: $llm_health_url"
                    echo "   - LLM model alias: $default_llm_server_model_alias"
                    if [ -f "$LLM_PORT_FILE" ]; then
                        echo "   - LLM port file: $(cat "$LLM_PORT_FILE")"
                    fi
                    if _tmux_has_session "$LLM_TMUX_SESSION"; then
                        echo "   - LLM tmux session: $LLM_TMUX_SESSION"
                    fi
                    stt status
                    ;;
                logs)
                    echo ">>> LLM Logs ($LOG_LLM):"
                    tail -f "$LOG_LLM"
                    ;;
                show|attach)
                    _tmux_require || return 1
                    if _tmux_has_session "$LLM_TMUX_SESSION"; then
                        echo "Attaching to llama tmux session '$LLM_TMUX_SESSION' (Ctrl+b d to detach)..."
                        tmux attach -t "$LLM_TMUX_SESSION"
                    else
                        echo "No llama tmux session '$LLM_TMUX_SESSION' found. Start with 'stt llm'."
                    fi
                    ;;
                *)
                    echo "Unknown llm subcommand: $llm_action"
                    echo
                    _print_help_llm
                    return 1
                    ;;
            esac
            ;;
        restart)
            local restart_mode=""
            if [ "${1:-}" = "stream" ] || [ "${1:-}" = "streaming" ] || [ "${1:-}" = "offline" ]; then
                restart_mode="$1"
                shift
            fi
            stt stop
            if [ -n "$restart_mode" ]; then
                stt start "$restart_mode" "$@"
            else
                stt start "$@"
            fi
            ;;
        stop)
            echo ">>> Stopping Parakeet..."
            if _pid_alive "$CLIENT_PID_FILE"; then
                kill -TERM "$(cat "$CLIENT_PID_FILE")" 2>/dev/null || true
            fi
            pkill -f "[p]arakeet-ptt" >/dev/null 2>&1 && echo "   - Client stopped"
            _tmux_kill_session "$TMUX_SESSION"

            if _socket_ready_once; then
                _refresh_daemon_pid_file_from_listener >/dev/null 2>&1 || true
            fi
            if _pid_alive "$DAEMON_PID_FILE"; then
                if _stop_pid "$DAEMON_PID_FILE"; then
                    echo "   - Daemon stopped"
                fi
            fi

            rm -f "$CLIENT_PID_FILE" "$DAEMON_PID_FILE" "$PORT_FILE"
            ;;
        logs)
            case "${1:-both}" in
                client)
                    echo ">>> Client Logs ($LOG_CLIENT):"
                    tail -f "$LOG_CLIENT"
                    ;;
                daemon)
                    echo ">>> Daemon Logs ($LOG_DAEMON):"
                    tail -f "$LOG_DAEMON"
                    ;;
                *)
                    echo ">>> Tailing client and daemon logs (Ctrl+C to stop)..."
                    tail -f "$LOG_CLIENT" "$LOG_DAEMON"
                    ;;
            esac
            ;;
        show|attach)
            _tmux_require || return 1
            if _tmux_has_session "$TMUX_SESSION"; then
                echo "Attaching to tmux session '$TMUX_SESSION' (Ctrl+b d to detach)..."
                tmux attach -t "$TMUX_SESSION"
            else
                echo "No tmux session '$TMUX_SESSION' found. Start with 'stt start'."
            fi
            ;;
        status)
            echo ">>> Status:"
            if _socket_ready_once && _refresh_daemon_pid_file_from_listener >/dev/null 2>&1; then
                :
            fi
            if _pid_alive "$DAEMON_PID_FILE"; then
                echo "   - Daemon running (pid $(cat "$DAEMON_PID_FILE"))"
            else
                echo "   - Daemon not running"
            fi
            if _pid_alive "$CLIENT_PID_FILE"; then
                echo "   - Client running (pid $(cat "$CLIENT_PID_FILE"))"
            else
                echo "   - Client not running"
            fi
            if [ -f "$PORT_FILE" ]; then
                echo "   - Endpoint: ws://$(cat "$PORT_FILE")/ws"
            else
                echo "   - Endpoint: $DEFAULT_ENDPOINT"
            fi
            if pgrep -af "[p]arakeet" >/dev/null; then
                echo "   - Matching processes:"
                pgrep -af "[p]arakeet" | sed 's/^/     /'
            fi
            if _tmux_has_session "$TMUX_SESSION"; then
                echo "   - tmux session: $TMUX_SESSION"
            fi
            ;;
        tmux)
            _tmux_require || return 1
            local action="${1:-attach}"
            if _tmux_has_session "$TMUX_SESSION"; then
                if [ "$action" = "kill" ]; then
                    tmux kill-session -t "$TMUX_SESSION"
                    echo "Killed tmux session '$TMUX_SESSION'."
                    return 0
                fi
                echo "Attaching to existing tmux session '$TMUX_SESSION'..."
                tmux attach -t "$TMUX_SESSION"
                return $?
            fi

            if pgrep -af "[p]arakeet-stt-daemon" >/dev/null || pgrep -af "[p]arakeet-ptt" >/dev/null; then
                echo "Warning: parakeet processes already running; use 'stt stop' first to avoid duplicates."
            fi

            if ! _resolve_port; then
                return 1
            fi

            echo "Creating tmux session '$TMUX_SESSION' (daemon | client | logs)..."
            echo "--- tmux session start: $(date -Is) ---" >> "$LOG_DAEMON"
            echo "--- tmux session start: $(date -Is) ---" >> "$LOG_CLIENT"
            echo "${HOST}:${PORT}" >| "$PORT_FILE"

            local daemon_streaming_enabled="$default_daemon_streaming_enabled"
            local injection_mode paste_backend_failure_policy
            local uinput_dwell_ms paste_seat paste_write_primary
            local completion_sound completion_sound_path completion_sound_volume overlay_enabled overlay_adaptive_width
            local llm_pre_modifier_key llm_base_url llm_model llm_timeout_seconds llm_max_tokens llm_temperature llm_system_prompt llm_overlay_stream
            local ptt_rustflags="$default_ptt_rustflags"
            local ptt_runner_preference="$default_ptt_runner_preference"
            _load_start_vars_from_defaults

            local daemon_overlay_events_enabled="$overlay_enabled"
            local daemon_cmd="RUST_LOG=\"$RUST_LOG\" UV_CACHE_DIR=\"$REPO_ROOT/.uv-cache\" PARAKEET_STREAMING_ENABLED=\"$daemon_streaming_enabled\" PARAKEET_OVERLAY_EVENTS_ENABLED=\"$daemon_overlay_events_enabled\" PARAKEET_HOST=\"$HOST\" PARAKEET_PORT=\"$PORT\" PARAKEET_SILENCE_FLOOR_DB=-60.0 uv run parakeet-stt-daemon --host \"$HOST\" --port \"$PORT\" >> \"$LOG_DAEMON\" 2>&1"

            if [ "$ptt_runner_preference" != "cargo" ] && [ "$ptt_runner_preference" != "release" ]; then
                echo "   - Invalid PARAKEET_PTT_RUNNER_PREFERENCE='$ptt_runner_preference'; defaulting to cargo."
                ptt_runner_preference="cargo"
            fi

            local -a ptt_args
            _build_ptt_args ptt_args
            local ptt_args_shell
            ptt_args_shell="$(_args_to_shell_words ptt_args)"

            local runner_mode
            runner_mode="$(_select_client_runner_mode "$CLIENT_DIR/target/release/parakeet-ptt" "$ptt_runner_preference")"
            if [ "$ptt_runner_preference" = "release" ] && [ "$runner_mode" = "cargo" ] && [ -x "$CLIENT_DIR/target/release/parakeet-ptt" ]; then
                echo "[helper] release binary missing expected start flags; falling back to cargo run --release --bin parakeet-ptt" >> "$LOG_CLIENT"
            fi

            local client_cmd='
                set -e
                runner_bin=""
                if [ "${RUNNER_MODE:-cargo}" = "release" ] && [ -x ./target/release/parakeet-ptt ]; then
                    runner_bin="./target/release/parakeet-ptt"
                else
                    echo "[helper] running cargo run --release --bin parakeet-ptt" >> "$LOG_CLIENT"
                fi

                eval "set -- $PTT_ARGS_SHELL"
                args=("$@")

                if [ -n "$runner_bin" ]; then
                    "$runner_bin" "${args[@]}" >> "$LOG_CLIENT" 2>&1
                else
                    RUSTFLAGS="${PTT_RUSTFLAGS}" cargo run --release --bin parakeet-ptt -- "${args[@]}" >> "$LOG_CLIENT" 2>&1
                fi
            '

            tmux new-session -d -s "$TMUX_SESSION" -n daemon -c "$DAEMON_DIR" "$daemon_cmd"
            tmux new-window -t "$TMUX_SESSION" -n client -c "$CLIENT_DIR" "LOG_CLIENT=\"$LOG_CLIENT\" RUNNER_MODE=\"$runner_mode\" PTT_RUSTFLAGS=\"$ptt_rustflags\" PTT_ARGS_SHELL=\"$ptt_args_shell\" RUST_LOG=\"$RUST_LOG\" PARAKEET_OVERLAY_MODE=\"${PARAKEET_OVERLAY_MODE:-}\" bash -lc '$client_cmd'"
            tmux new-window -t "$TMUX_SESSION" -n logs -c /tmp "tail -f \"$LOG_DAEMON\" \"$LOG_CLIENT\""
            tmux select-window -t "$TMUX_SESSION:daemon"
            tmux attach -t "$TMUX_SESSION"
            ;;
        check)
            echo ">>> Health check (daemon --check)..."
            (
                cd "$DAEMON_DIR" || exit 1
                PARAKEET_STREAMING_ENABLED="$default_daemon_streaming_enabled" UV_CACHE_DIR="$REPO_ROOT/.uv-cache" uv run parakeet-stt-daemon --check
            )
            ;;
        diag-injector)
            echo ">>> Clipboard injector diagnostics (test-injection)"
            (
                cd "$CLIENT_DIR" || exit 1
                set -e
                local attempts="1"
                local shortcut="auto"
                local text_prefix="Parakeet Test"
                local interval_ms="150"
                while [ "$#" -gt 0 ]; do
                    case "$1" in
                        --attempts)
                            [ "$#" -ge 2 ] || {
                                echo "diag-injector requires a value after --attempts" >&2
                                exit 2
                            }
                            attempts="$2"
                            shift 2
                            ;;
                        --shortcut)
                            [ "$#" -ge 2 ] || {
                                echo "diag-injector requires a value after --shortcut" >&2
                                exit 2
                            }
                            shortcut="$2"
                            shift 2
                            ;;
                        --prefix)
                            [ "$#" -ge 2 ] || {
                                echo "diag-injector requires a value after --prefix" >&2
                                exit 2
                            }
                            text_prefix="$2"
                            shift 2
                            ;;
                        --interval-ms)
                            [ "$#" -ge 2 ] || {
                                echo "diag-injector requires a value after --interval-ms" >&2
                                exit 2
                            }
                            interval_ms="$2"
                            shift 2
                            ;;
                        *)
                            echo "Unknown diag-injector argument: $1" >&2
                            exit 2
                            ;;
                    esac
                done

                case "$shortcut" in
                    auto|ctrl-v|ctrl-shift-v) ;;
                    *)
                        echo "diag-injector shortcut must be one of: auto|ctrl-v|ctrl-shift-v" >&2
                        exit 2
                        ;;
                esac
                [[ "$attempts" =~ ^[0-9]+$ ]] || {
                    echo "diag-injector attempts must be an integer" >&2
                    exit 2
                }
                [ "$attempts" -ge 1 ] || {
                    echo "diag-injector attempts must be >= 1" >&2
                    exit 2
                }
                [[ "$interval_ms" =~ ^[0-9]+$ ]] || {
                    echo "diag-injector interval-ms must be an integer" >&2
                    exit 2
                }

                local runner_mode runner_bin
                local ptt_rustflags="${PARAKEET_PTT_RUSTFLAGS:-}"
                local ptt_runner_preference="${PARAKEET_PTT_RUNNER_PREFERENCE:-cargo}"
                if [ "$ptt_runner_preference" != "cargo" ] && [ "$ptt_runner_preference" != "release" ]; then
                    echo "   - Invalid PARAKEET_PTT_RUNNER_PREFERENCE='$ptt_runner_preference'; defaulting to cargo."
                    ptt_runner_preference="cargo"
                fi

                runner_mode="$(_select_client_runner_mode "$CLIENT_DIR/target/release/parakeet-ptt" "$ptt_runner_preference")"
                runner_bin=""
                if [ "$runner_mode" = "release" ]; then
                    if _ptt_binary_supports_diag_injection_flags "$CLIENT_DIR/target/release/parakeet-ptt"; then
                        runner_bin="./target/release/parakeet-ptt"
                    else
                        runner_mode="cargo"
                    fi
                fi
                if [ "$ptt_runner_preference" = "release" ] && [ "$runner_mode" = "cargo" ] && [ -x "$CLIENT_DIR/target/release/parakeet-ptt" ]; then
                    echo "   - release binary missing expected diag-injector flags; using cargo run --release --bin parakeet-ptt"
                fi

                echo "   - diag backend: uinput"
                echo "   - diag attempts per backend: $attempts"
                echo "   - diag forced shortcut: $shortcut"
                echo "   - diag text prefix: $text_prefix"
                echo "   - diag interval ms: $interval_ms"
                if [ -e /dev/uinput ]; then
                    if [ -w /dev/uinput ]; then
                        echo "   - capability: /dev/uinput writable=yes"
                    else
                        echo "   - capability: /dev/uinput writable=no (set udev/group permissions before uinput backend)"
                    fi
                else
                    echo "   - capability: /dev/uinput missing (load uinput module: sudo modprobe uinput)"
                fi

                run_case() {
                    local injection_mode paste_backend_failure_policy
                    local uinput_dwell_ms paste_seat paste_write_primary
                    local completion_sound completion_sound_path completion_sound_volume
                    local llm_pre_modifier_key llm_base_url llm_model llm_timeout_seconds llm_max_tokens llm_temperature llm_system_prompt llm_overlay_stream
                    local -a ptt_args
                    local -a diag_args

                    _load_start_vars_from_defaults
                    injection_mode="paste"
                    _build_ptt_args ptt_args no

                    diag_args=(
                        --test-injection
                        --test-injection-count "$attempts"
                        --test-injection-text-prefix "$text_prefix"
                        --test-injection-interval-ms "$interval_ms"
                    )
                    if [ "$shortcut" != "auto" ]; then
                        diag_args+=(--test-injection-shortcut "$shortcut")
                    fi

                    echo "   - case backend=uinput"
                    if [ -n "$runner_bin" ]; then
                        RUST_LOG="${RUST_LOG:-parakeet_ptt=info,parakeet_ptt::injector=debug}" \
                            "$runner_bin" "${diag_args[@]}" "${ptt_args[@]}"
                    else
                        RUST_LOG="${RUST_LOG:-parakeet_ptt=info,parakeet_ptt::injector=debug}" \
                            RUSTFLAGS="$ptt_rustflags" cargo run --release --bin parakeet-ptt -- "${diag_args[@]}" "${ptt_args[@]}"
                    fi
                }

                run_case
            )
            ;;
        *)
            echo "Unknown command: $cmd"
            echo
            _print_help_main
            return 1
            ;;
    esac
}

# To use: source scripts/stt-helper.sh (or copy this function into your shell rc)
get_stt_start_args() {
    if [ "$#" -lt 1 ]; then
        echo "get_stt_start_args requires an output array name" >&2
        return 2
    fi
    local out_name="$1"
    shift
    local -n out_ref="$out_name"
    out_ref=()
    local output
    output="$(stt __start-cli-args "$@")" || return $?
    local arg
    while IFS= read -r arg; do
        out_ref+=("$arg")
    done <<<"$output"
}
