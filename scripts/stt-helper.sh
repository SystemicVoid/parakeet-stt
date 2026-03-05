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
    local CLIENT_PID_FILE="/tmp/parakeet-ptt.pid"
    local DAEMON_PID_FILE="/tmp/parakeet-daemon.pid"
    local HOST="${PARAKEET_HOST:-127.0.0.1}"
    local PORT="${PARAKEET_PORT:-8765}"
    local PORT_FILE="/tmp/parakeet-daemon.port"
    local DEFAULT_ENDPOINT="ws://${HOST}:${PORT}/ws"
    local TMUX_SESSION="parakeet-stt"
    local TMUX_WINDOW="run"
    local default_injection_mode="${PARAKEET_INJECTION_MODE:-paste}"
    local default_paste_key_backend="${PARAKEET_PASTE_KEY_BACKEND:-auto}"
    local default_paste_backend_failure_policy="${PARAKEET_PASTE_BACKEND_FAILURE_POLICY:-copy-only}"
    local default_uinput_dwell_ms="${PARAKEET_UINPUT_DWELL_MS:-18}"
    local default_paste_seat="${PARAKEET_PASTE_SEAT:-}"
    local default_paste_write_primary="${PARAKEET_PASTE_WRITE_PRIMARY:-false}"
    local default_ydotool_path="${PARAKEET_YDOTOOL_PATH:-}"
    local default_completion_sound="${PARAKEET_COMPLETION_SOUND:-true}"
    local default_completion_sound_path="${PARAKEET_COMPLETION_SOUND_PATH:-}"
    local default_completion_sound_volume="${PARAKEET_COMPLETION_SOUND_VOLUME:-100}"
    local default_overlay_enabled="${PARAKEET_OVERLAY_ENABLED:-false}"
    local default_overlay_adaptive_width="${PARAKEET_OVERLAY_ADAPTIVE_WIDTH:-true}"
    local default_daemon_streaming_enabled="false"
    local default_daemon_chunk_secs="2.4"
    local default_daemon_right_context_secs="1.6"
    local default_daemon_left_context_secs="10.0"
    local default_daemon_batch_size="32"
    local default_client_ready_timeout_seconds="${PARAKEET_CLIENT_READY_TIMEOUT_SECONDS:-30}"
    # Local-only default: optimize for this workstation (Zen5 + AVX512), not portable builds.
    local default_ptt_rustflags="${PARAKEET_PTT_RUSTFLAGS:--C target-cpu=znver5 -C target-feature=+avx512f,+avx512bw,+avx512cd,+avx512dq,+avx512vl,+avx512vnni}"
    local default_ptt_runner_preference="${PARAKEET_PTT_RUNNER_PREFERENCE:-cargo}"
    local -a start_option_rows=(
        "injection-mode|injection_mode|default_injection_mode|PARAKEET_INJECTION_MODE|Injection mode|<mode>|paste|always|paste"
        "paste-key-backend|paste_key_backend|default_paste_key_backend|PARAKEET_PASTE_KEY_BACKEND|Stable controls|<v>|auto|always|auto"
        "paste-backend-failure-policy|paste_backend_failure_policy|default_paste_backend_failure_policy|PARAKEET_PASTE_BACKEND_FAILURE_POLICY|Stable controls|<v>|copy-only|always|copy-only"
        "uinput-dwell-ms|uinput_dwell_ms|default_uinput_dwell_ms|PARAKEET_UINPUT_DWELL_MS|Stable controls|<n>|18|always|18"
        "paste-seat|paste_seat|default_paste_seat|PARAKEET_PASTE_SEAT|Stable controls|<v>|<unset>|nonempty|"
        "paste-write-primary|paste_write_primary|default_paste_write_primary|PARAKEET_PASTE_WRITE_PRIMARY|Stable controls|<v>|false|always|false"
        "ydotool|ydotool_path|default_ydotool_path|PARAKEET_YDOTOOL_PATH|Stable controls|<path>|<auto>|nonempty|"
        "completion-sound|completion_sound|default_completion_sound|PARAKEET_COMPLETION_SOUND|Stable controls|<v>|true|always|true"
        "completion-sound-path|completion_sound_path|default_completion_sound_path|PARAKEET_COMPLETION_SOUND_PATH|Stable controls|<path>|<system default>|nonempty|"
        "completion-sound-volume|completion_sound_volume|default_completion_sound_volume|PARAKEET_COMPLETION_SOUND_VOLUME|Stable controls|<n>|100|always|100"
        "overlay-enabled|overlay_enabled|default_overlay_enabled|PARAKEET_OVERLAY_ENABLED|Stable controls|<v>|false|always|false"
        "overlay-adaptive-width|overlay_adaptive_width|default_overlay_adaptive_width|PARAKEET_OVERLAY_ADAPTIVE_WIDTH|Stable controls|<v>|true|always|true"
    )

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
        local guessed="$HOME/Documents/Engineering/parakeet-stt"
        if [ -d "$guessed/parakeet-stt-daemon" ] && [ -d "$guessed/parakeet-ptt" ]; then
            REPO_ROOT="$guessed"
        else
            echo "stt helper: could not locate repo root (REPO_ROOT='$REPO_ROOT'). Set PARAKEET_ROOT explicitly."
            return 1
        fi
    fi
    DAEMON_DIR="$REPO_ROOT/parakeet-stt-daemon"
    CLIENT_DIR="$REPO_ROOT/parakeet-ptt"

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
                echo "$pid" > "$CLIENT_PID_FILE"
                return 0
            fi

            if [ -f "$LOG_CLIENT" ] && grep -Eq "Starting hotkey loop; press Right Ctrl to talk|Hotkey listeners started for KEY_RIGHTCTRL|Connected to daemon" "$LOG_CLIENT"; then
                pid=$(pgrep -n "[p]arakeet-ptt" || true)
                if [ -n "$pid" ]; then
                    echo "$pid" > "$CLIENT_PID_FILE"
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
  stop                   Stop daemon/client and remove pid/port files.
  restart [options]      Restart with the same options as start.
  status                 Show daemon/client/tmux status.
  logs [client|daemon|both]
                         Tail logs (default: both).
  show | attach          Attach to tmux session.
  tmux [attach|kill]     Attach/kill helper tmux session.
  check                  Run daemon health check.
  diag-injector          Run clipboard injector diagnostics.
  help [start]           Show this help or start help.
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

    _ensure_overlay_release_binary() {
        local ptt_rustflags="$1"
        local overlay_required_raw="${2:-false}"
        local overlay_required="false"
        local overlay_binary="$CLIENT_DIR/target/release/parakeet-overlay"
        local build_cmd="cargo build --release --bin parakeet-overlay"
        local build_output=""

        case "${overlay_required_raw,,}" in
            true|1|yes|on)
                overlay_required="true"
                ;;
        esac

        if [ "$overlay_required" != "true" ]; then
            return 0
        fi

        if ! command -v cargo >/dev/null 2>&1; then
            if [ ! -x "$overlay_binary" ]; then
                echo "   - ERROR: overlay is enabled, but cargo is unavailable and overlay binary is missing."
                echo "   - Build manually when cargo is available:"
                echo "     cd \"$CLIENT_DIR\" && RUSTFLAGS=\"$ptt_rustflags\" $build_cmd"
                echo "   - Or launch without overlay: stt start --overlay-enabled false"
                echo "[helper] overlay required but binary missing at $overlay_binary and cargo unavailable; aborting start" >> "$LOG_CLIENT"
                return 1
            fi
            return 0
        fi

        echo "   - Ensuring overlay binary is available (${build_cmd})..."
        echo "[helper] ensuring overlay binary via ${build_cmd}" >> "$LOG_CLIENT"
        build_output="$(mktemp)"
        if (
            cd "$CLIENT_DIR" || exit 1
            RUSTFLAGS="$ptt_rustflags" cargo build --release --bin parakeet-overlay >"$build_output" 2>&1
        ); then
            cat "$build_output" >> "$LOG_CLIENT"
            rm -f "$build_output"
            if [ ! -x "$overlay_binary" ]; then
                echo "   - ERROR: overlay build reported success but binary is missing at $overlay_binary."
                echo "   - Retry manually:"
                echo "     cd \"$CLIENT_DIR\" && RUSTFLAGS=\"$ptt_rustflags\" $build_cmd"
                echo "[helper] overlay build succeeded but binary missing at $overlay_binary; aborting start" >> "$LOG_CLIENT"
                return 1
            fi
            return 0
        fi

        cat "$build_output" >> "$LOG_CLIENT"
        echo "   - ERROR: overlay is enabled and build failed (${build_cmd})."
        echo "   - Last overlay build output:"
        tail -n 40 "$build_output"
        echo "   - Full output saved to: $LOG_CLIENT"
        echo "   - Retry manually:"
        echo "     cd \"$CLIENT_DIR\" && RUSTFLAGS=\"$ptt_rustflags\" $build_cmd"
        echo "   - Or launch without overlay: stt start --overlay-enabled false"
        echo "[helper] overlay build failed while required; aborting start" >> "$LOG_CLIENT"
        rm -f "$build_output"
        return 1
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
            local injection_mode paste_key_backend paste_backend_failure_policy
            local uinput_dwell_ms paste_seat paste_write_primary ydotool_path
            local completion_sound completion_sound_path completion_sound_volume overlay_enabled overlay_adaptive_width
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
        start)
            local injection_mode paste_key_backend paste_backend_failure_policy
            local uinput_dwell_ms paste_seat paste_write_primary ydotool_path
            local completion_sound completion_sound_path completion_sound_volume overlay_enabled overlay_adaptive_width
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
            echo "   - Paste key backend: $paste_key_backend"
            echo "   - Paste backend failure policy: $paste_backend_failure_policy"
            echo "   - uinput dwell (ms): $uinput_dwell_ms"
            echo "   - Paste seat: ${paste_seat:-<default>}"
            echo "   - Paste write primary: $paste_write_primary"
            echo "   - Completion sound: $completion_sound"
            echo "   - Completion sound path: ${completion_sound_path:-<system default>}"
            echo "   - Completion sound volume: $completion_sound_volume"
            echo "   - Overlay enabled: $overlay_enabled"
            echo "   - Overlay adaptive width: $overlay_adaptive_width"
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
                    nohup uv run parakeet-stt-daemon --host "$HOST" --port "$PORT" >> "$LOG_DAEMON" 2>&1 &
                    echo $! > "$DAEMON_PID_FILE"
                )
            fi

            echo -n "   - Waiting for socket..."
            if _wait_for_socket "$DAEMON_PID_FILE" 60; then
                echo " OK"
                echo "${HOST}:${PORT}" > "$PORT_FILE"
            else
                echo " not ready; last daemon log lines:"
                tail -n 80 "$LOG_DAEMON"
                return 1
            fi

            if ! command -v tmux >/dev/null 2>&1; then
                echo "   - tmux is required for the default start path. Install with: sudo apt install tmux"
                return 1
            fi
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

            if tmux has-session -t "$TMUX_SESSION" >/dev/null 2>&1; then
                tmux kill-session -t "$TMUX_SESSION"
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
            _ensure_overlay_release_binary "$ptt_rustflags" "$overlay_enabled" || return 1

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
            if command -v tmux >/dev/null 2>&1 && tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
                tmux kill-session -t "$TMUX_SESSION"
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
            if ! command -v tmux >/dev/null 2>&1; then
                echo "tmux is not installed; install it first (sudo apt install tmux)."
                return 1
            fi
            if tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
                echo "Attaching to tmux session '$TMUX_SESSION' (Ctrl+b d to detach)..."
                tmux attach -t "$TMUX_SESSION"
            else
                echo "No tmux session '$TMUX_SESSION' found. Start with 'stt start'."
            fi
            ;;
        status)
            echo ">>> Status:"
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
            if command -v tmux >/dev/null 2>&1 && tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
                echo "   - tmux session: $TMUX_SESSION"
            fi
            ;;
        tmux)
            if ! command -v tmux >/dev/null 2>&1; then
                echo "tmux is not installed; install it first (sudo apt install tmux)."
                return 1
            fi
            local action="${1:-attach}"
            if tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
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
            echo "${HOST}:${PORT}" > "$PORT_FILE"

            local daemon_streaming_enabled="$default_daemon_streaming_enabled"
            local injection_mode paste_key_backend paste_backend_failure_policy
            local uinput_dwell_ms paste_seat paste_write_primary ydotool_path
            local completion_sound completion_sound_path completion_sound_volume overlay_enabled overlay_adaptive_width
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
            _ensure_overlay_release_binary "$ptt_rustflags" "$overlay_enabled" || return 1

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
                local runner_mode runner_bin
                local ptt_rustflags="${PARAKEET_PTT_RUSTFLAGS:--C target-cpu=znver5 -C target-feature=+avx512f,+avx512bw,+avx512cd,+avx512dq,+avx512vl,+avx512vnni}"
                local ptt_runner_preference="${PARAKEET_PTT_RUNNER_PREFERENCE:-cargo}"
                if [ "$ptt_runner_preference" != "cargo" ] && [ "$ptt_runner_preference" != "release" ]; then
                    echo "   - Invalid PARAKEET_PTT_RUNNER_PREFERENCE='$ptt_runner_preference'; defaulting to cargo."
                    ptt_runner_preference="cargo"
                fi

                runner_mode="$(_select_client_runner_mode "$CLIENT_DIR/target/release/parakeet-ptt" "$ptt_runner_preference")"
                runner_bin=""
                if [ "$runner_mode" = "release" ]; then
                    runner_bin="./target/release/parakeet-ptt"
                elif [ -x "$CLIENT_DIR/target/release/parakeet-ptt" ]; then
                    echo "   - release binary missing expected start flags; using cargo run --release --bin parakeet-ptt"
                fi

                echo "   - capability: ydotool=$(command -v ydotool >/dev/null 2>&1 && echo yes || echo no)"
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
                    local backend="$1"
                    local injection_mode paste_key_backend paste_backend_failure_policy
                    local uinput_dwell_ms paste_seat paste_write_primary ydotool_path
                    local completion_sound completion_sound_path completion_sound_volume
                    local -a ptt_args

                    _load_start_vars_from_defaults
                    injection_mode="paste"
                    paste_key_backend="$backend"
                    _build_ptt_args ptt_args no

                    echo "   - case backend=$backend"
                    if [ -n "$runner_bin" ]; then
                        RUST_LOG="${RUST_LOG:-parakeet_ptt=info,parakeet_ptt::injector=debug}" \
                            "$runner_bin" --test-injection "${ptt_args[@]}"
                    else
                        RUST_LOG="${RUST_LOG:-parakeet_ptt=info,parakeet_ptt::injector=debug}" \
                            RUSTFLAGS="$ptt_rustflags" cargo run --release --bin parakeet-ptt -- --test-injection "${ptt_args[@]}"
                    fi
                }

                run_case "auto"
                run_case "uinput"
                run_case "ydotool"
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
