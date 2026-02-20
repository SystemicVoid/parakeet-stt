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
    local -a ignored_compat_requests=()
    local default_injection_mode="${PARAKEET_INJECTION_MODE:-paste}"
    local default_paste_shortcut="ctrl-shift-v"
    local default_paste_shortcut_fallback="none"
    local default_paste_strategy="single"
    local default_paste_chain_delay_ms="45"
    local default_paste_restore_policy="never"
    local default_paste_restore_delay_ms="250"
    local default_paste_post_chord_hold_ms="700"
    local default_paste_copy_foreground="true"
    local default_paste_mime_type="text/plain;charset=utf-8"
    local default_paste_key_backend="${PARAKEET_PASTE_KEY_BACKEND:-auto}"
    local default_paste_backend_failure_policy="${PARAKEET_PASTE_BACKEND_FAILURE_POLICY:-copy-only}"
    local default_paste_routing_mode="adaptive"
    local default_adaptive_terminal_shortcut="ctrl-shift-v"
    local default_adaptive_general_shortcut="ctrl-v"
    local default_adaptive_unknown_shortcut="ctrl-shift-v"
    local default_focus_resolver_source="wayland"
    local default_focus_resolve_budget_ms="450"
    local default_focus_deep_scan_max_apps="1"
    local default_focus_wayland_stale_ms="30000"
    local default_focus_wayland_transition_grace_ms="500"
    local default_uinput_dwell_ms="${PARAKEET_UINPUT_DWELL_MS:-18}"
    local default_paste_seat="${PARAKEET_PASTE_SEAT:-}"
    local default_paste_write_primary="${PARAKEET_PASTE_WRITE_PRIMARY:-false}"
    local default_ydotool_path="${PARAKEET_YDOTOOL_PATH:-}"
    local default_completion_sound="${PARAKEET_COMPLETION_SOUND:-true}"
    local default_completion_sound_path="${PARAKEET_COMPLETION_SOUND_PATH:-}"
    local default_completion_sound_volume="${PARAKEET_COMPLETION_SOUND_VOLUME:-100}"
    local default_client_ready_timeout_seconds="${PARAKEET_CLIENT_READY_TIMEOUT_SECONDS:-30}"
    local -a start_option_rows=(
        "injection-mode|injection_mode|default_injection_mode|PARAKEET_INJECTION_MODE|Injection mode|<mode>|paste|always|paste"
        "paste-shortcut|paste_shortcut|default_paste_shortcut|PARAKEET_PASTE_SHORTCUT|Compatibility (deprecated)|<v>|ctrl-shift-v|compat|ctrl-shift-v"
        "paste-shortcut-fallback|paste_shortcut_fallback|default_paste_shortcut_fallback|PARAKEET_PASTE_SHORTCUT_FALLBACK|Compatibility (deprecated)|<v>|none|compat|none"
        "paste-strategy|paste_strategy|default_paste_strategy|PARAKEET_PASTE_STRATEGY|Compatibility (deprecated)|<v>|single|compat|single"
        "paste-chain-delay-ms|paste_chain_delay_ms|default_paste_chain_delay_ms|PARAKEET_PASTE_CHAIN_DELAY_MS|Compatibility (deprecated)|<n>|45|compat|45"
        "paste-restore-policy|paste_restore_policy|default_paste_restore_policy|PARAKEET_PASTE_RESTORE_POLICY|Compatibility (deprecated)|<v>|never|compat|never"
        "paste-restore-delay-ms|paste_restore_delay_ms|default_paste_restore_delay_ms|PARAKEET_PASTE_RESTORE_DELAY_MS|Compatibility (deprecated)|<n>|250|compat|250"
        "paste-post-chord-hold-ms|paste_post_chord_hold_ms|default_paste_post_chord_hold_ms|PARAKEET_PASTE_POST_CHORD_HOLD_MS|Compatibility (deprecated)|<n>|700|compat|700"
        "paste-copy-foreground|paste_copy_foreground|default_paste_copy_foreground|PARAKEET_PASTE_COPY_FOREGROUND|Compatibility (deprecated)|<v>|true|compat|true"
        "paste-mime-type|paste_mime_type|default_paste_mime_type|PARAKEET_PASTE_MIME_TYPE|Compatibility (deprecated)|<v>|text/plain;charset=utf-8|compat|text/plain;charset=utf-8"
        "paste-routing-mode|paste_routing_mode|default_paste_routing_mode|PARAKEET_PASTE_ROUTING_MODE|Compatibility (deprecated)|<v>|adaptive|compat|adaptive"
        "adaptive-terminal-shortcut|adaptive_terminal_shortcut|default_adaptive_terminal_shortcut|PARAKEET_ADAPTIVE_TERMINAL_SHORTCUT|Compatibility (deprecated)|<v>|ctrl-shift-v|compat|ctrl-shift-v"
        "adaptive-general-shortcut|adaptive_general_shortcut|default_adaptive_general_shortcut|PARAKEET_ADAPTIVE_GENERAL_SHORTCUT|Compatibility (deprecated)|<v>|ctrl-v|compat|ctrl-v"
        "adaptive-unknown-shortcut|adaptive_unknown_shortcut|default_adaptive_unknown_shortcut|PARAKEET_ADAPTIVE_UNKNOWN_SHORTCUT|Compatibility (deprecated)|<v>|ctrl-shift-v|compat|ctrl-shift-v"
        "focus-resolver-source|focus_resolver_source|default_focus_resolver_source|PARAKEET_FOCUS_RESOLVER_SOURCE|Compatibility (deprecated)|<v>|wayland|compat|wayland"
        "focus-resolve-budget-ms|focus_resolve_budget_ms|default_focus_resolve_budget_ms|PARAKEET_FOCUS_RESOLVE_BUDGET_MS|Compatibility (deprecated)|<n>|450|compat|450"
        "focus-deep-scan-max-apps|focus_deep_scan_max_apps|default_focus_deep_scan_max_apps|PARAKEET_FOCUS_DEEP_SCAN_MAX_APPS|Compatibility (deprecated)|<n>|1|compat|1"
        "focus-wayland-stale-ms|focus_wayland_stale_ms|default_focus_wayland_stale_ms|PARAKEET_FOCUS_WAYLAND_STALE_MS|Compatibility (deprecated)|<n>|30000|compat|30000"
        "focus-wayland-transition-grace-ms|focus_wayland_transition_grace_ms|default_focus_wayland_transition_grace_ms|PARAKEET_FOCUS_WAYLAND_TRANSITION_GRACE_MS|Compatibility (deprecated)|<n>|500|compat|500"
        "paste-key-backend|paste_key_backend|default_paste_key_backend|PARAKEET_PASTE_KEY_BACKEND|Stable controls|<v>|auto|always|auto"
        "paste-backend-failure-policy|paste_backend_failure_policy|default_paste_backend_failure_policy|PARAKEET_PASTE_BACKEND_FAILURE_POLICY|Stable controls|<v>|copy-only|always|copy-only"
        "uinput-dwell-ms|uinput_dwell_ms|default_uinput_dwell_ms|PARAKEET_UINPUT_DWELL_MS|Stable controls|<n>|18|always|18"
        "paste-seat|paste_seat|default_paste_seat|PARAKEET_PASTE_SEAT|Stable controls|<v>|<unset>|nonempty|"
        "paste-write-primary|paste_write_primary|default_paste_write_primary|PARAKEET_PASTE_WRITE_PRIMARY|Stable controls|<v>|false|always|false"
        "ydotool|ydotool_path|default_ydotool_path|PARAKEET_YDOTOOL_PATH|Stable controls|<path>|<auto>|nonempty|"
        "completion-sound|completion_sound|default_completion_sound|PARAKEET_COMPLETION_SOUND|Stable controls|<v>|true|always|true"
        "completion-sound-path|completion_sound_path|default_completion_sound_path|PARAKEET_COMPLETION_SOUND_PATH|Stable controls|<path>|<system default>|nonempty|"
        "completion-sound-volume|completion_sound_volume|default_completion_sound_volume|PARAKEET_COMPLETION_SOUND_VOLUME|Stable controls|<n>|100|always|100"
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

    _is_deprecated_start_option() {
        local target="$1"
        local row opt_name option_group
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name _ _ _ option_group _ _ _ _ <<<"$row"
            [ "$opt_name" = "$target" ] || continue
            [ "$option_group" = "Compatibility (deprecated)" ]
            return $?
        done
        return 1
    }

    _warn_deprecated_env_overrides() {
        local row opt_name env_name option_group
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name _ _ env_name option_group _ _ _ _ <<<"$row"
            [ "$option_group" = "Compatibility (deprecated)" ] || continue
            if [ "${!env_name+x}" = "x" ]; then
                ignored_compat_requests+=("env:$env_name=${!env_name}")
                echo "   - Warning: env $env_name is deprecated and ignored; robust defaults remain active." >&2
            fi
        done
    }

    _set_start_option_value() {
        local target="$1"
        local value="$2"
        local row opt_name var_name option_group
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name var_name _ _ option_group _ _ _ _ <<<"$row"
            if [ "$opt_name" = "$target" ]; then
                if [ "$option_group" = "Compatibility (deprecated)" ]; then
                    ignored_compat_requests+=("--$target=$value")
                    return 0
                fi
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

    _apply_legacy_override_vars() {
        local row opt_name var_name legacy_name option_group
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name var_name _ _ option_group _ _ _ _ <<<"$row"
            legacy_name="$(tr '[:lower:]' '[:upper:]' <<<"$var_name")"
            if [ "${!legacy_name+x}" = "x" ]; then
                if [ "$option_group" = "Compatibility (deprecated)" ]; then
                    ignored_compat_requests+=("legacy:$legacy_name=${!legacy_name}")
                    continue
                fi
                printf -v "$var_name" "%s" "${!legacy_name}"
            fi
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

    _print_start_option_names_by_group() {
        local group_name="$1"
        local row opt_name option_group
        for row in "${start_option_rows[@]}"; do
            IFS='|' read -r opt_name _ _ _ option_group _ _ _ _ <<<"$row"
            [ "$option_group" = "$group_name" ] || continue
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
            if [ "$include_policy" = "compat" ]; then
                continue
            fi
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
        if [ -x "$binary" ] && _ptt_binary_supports_start_flags "$binary"; then
            printf "release"
        else
            printf "cargo"
        fi
    }

    _parse_start_options() {
        while [[ $# -gt 0 ]]; do
            case "$1" in
                --paste)
                    injection_mode="paste"
                    shift
                    ;;
                --type)
                    injection_mode="type"
                    shift
                    ;;
                --copy-only)
                    injection_mode="copy-only"
                    shift
                    ;;
                --help-compat)
                    _print_help_start_compat
                    return 2
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
                    if _is_deprecated_start_option "$opt_name"; then
                        echo "   - Warning: --$opt_name is deprecated and ignored; robust defaults remain active." >&2
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
        grep -Fq "[helper] running cargo run --release" "$LOG_CLIENT" || return 1
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
                fi
                return 0
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
  stt <command> [args]

Commands:
  start [options]        Start daemon + client (default command).
  stop                   Stop daemon/client and remove pid/port files.
  restart [options]      Restart with the same options as start.
  status                 Show daemon/client/tmux status.
  logs [client|daemon|both]
                         Tail logs (default: both).
  show | attach          Attach to tmux session.
  tmux [attach|kill]     Attach/kill helper tmux session.
  check                  Run daemon health check.
  diag-injector          Run clipboard injector diagnostics.
  help [start|start-compat]
                         Show this help, stable start help, or deprecated compat flags.

Help shortcuts:
  stt --help
  stt help
  stt help start
  stt help start-compat
  stt start --help
  stt start --help-compat
EOF
    }
    _print_help_start() {
        cat <<EOF
Usage:
  stt start [options]

Injection mode:
  --paste                              Alias for --injection-mode paste
  --type                               Alias for --injection-mode type
  --copy-only                          Alias for --injection-mode copy-only
EOF
        _print_start_option_group "Injection mode"
        echo
        echo "Stable controls:"
        _print_start_option_group "Stable controls"
        cat <<EOF

Deprecated compatibility options are hidden from primary help and ignored at runtime.
Show them with:
  stt help start-compat

Other environment overrides:
  PARAKEET_HOST=$HOST
  PARAKEET_PORT=$PORT
  PARAKEET_CLIENT_READY_TIMEOUT_SECONDS=$default_client_ready_timeout_seconds
EOF
    }
    _print_help_start_compat() {
        cat <<EOF
Usage:
  stt start [deprecated-compat-options]

Compatibility options are parsed for compatibility but ignored.
EOF
        _print_start_option_group "Compatibility (deprecated)"
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
                start-compat)
                    _print_help_start_compat
                    ;;
                *)
                    echo "Unknown help topic: $1"
                    echo
                    _print_help_main
                    return 1
                    ;;
            esac
            ;;
        __start-option-names-stable)
            _print_start_option_names_by_group "Injection mode"
            _print_start_option_names_by_group "Stable controls"
            ;;
        __start-option-names-deprecated)
            _print_start_option_names_by_group "Compatibility (deprecated)"
            ;;
        __start-option-names)
            _print_start_option_names
            ;;
        __start-args)
            local injection_mode
            local paste_shortcut
            local paste_shortcut_fallback
            local paste_strategy
            local paste_chain_delay_ms
            local paste_restore_policy
            local paste_restore_delay_ms
            local paste_post_chord_hold_ms
            local paste_copy_foreground
            local paste_mime_type
            local paste_key_backend
            local paste_backend_failure_policy
            local paste_routing_mode
            local adaptive_terminal_shortcut
            local adaptive_general_shortcut
            local adaptive_unknown_shortcut
            local focus_resolver_source
            local focus_resolve_budget_ms
            local focus_deep_scan_max_apps
            local focus_wayland_stale_ms
            local focus_wayland_transition_grace_ms
            local uinput_dwell_ms
            local paste_seat
            local paste_write_primary
            local ydotool_path
            local completion_sound
            local completion_sound_path
            local completion_sound_volume
            local -a ptt_args
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
            local injection_mode
            local paste_shortcut
            local paste_shortcut_fallback
            local paste_strategy
            local paste_chain_delay_ms
            local paste_restore_policy
            local paste_restore_delay_ms
            local paste_post_chord_hold_ms
            local paste_copy_foreground
            local paste_mime_type
            local paste_key_backend
            local paste_backend_failure_policy
            local paste_routing_mode
            local adaptive_terminal_shortcut
            local adaptive_general_shortcut
            local adaptive_unknown_shortcut
            local focus_resolver_source
            local focus_resolve_budget_ms
            local focus_deep_scan_max_apps
            local focus_wayland_stale_ms
            local focus_wayland_transition_grace_ms
            local uinput_dwell_ms
            local paste_seat
            local paste_write_primary
            local ydotool_path
            local completion_sound
            local completion_sound_path
            local completion_sound_volume
            local client_ready_timeout_seconds="$default_client_ready_timeout_seconds"
            _load_start_vars_from_defaults

            local parse_status=0
            _parse_start_options "$@" || parse_status=$?
            if [ "$parse_status" -eq 2 ]; then
                return 0
            elif [ "$parse_status" -ne 0 ]; then
                return "$parse_status"
            fi

            _warn_deprecated_env_overrides

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
            echo "   - Client ready timeout (s): $client_ready_timeout_seconds"
            if [ "${#ignored_compat_requests[@]}" -gt 0 ]; then
                echo "   - Deprecated compatibility overrides ignored:"
                printf '     %s\n' "${ignored_compat_requests[@]}"
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
                _log_daemon "launch via stt helper (--no-streaming)"
                (
                    cd "$DAEMON_DIR" || exit 1
                    PARAKEET_HOST="$HOST" PARAKEET_PORT="$PORT" nohup uv run parakeet-stt-daemon --host "$HOST" --port "$PORT" --no-streaming >> "$LOG_DAEMON" 2>&1 &
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
            runner_mode="$(_select_client_runner_mode "$CLIENT_DIR/target/release/parakeet-ptt")"
            if [ "$runner_mode" = "cargo" ] && [ -x "$CLIENT_DIR/target/release/parakeet-ptt" ]; then
                echo "[helper] release binary missing expected start flags; falling back to cargo run --release" >> "$LOG_CLIENT"
            fi

            local client_cmd
            client_cmd='
                set -e
                runner_bin=""
                if [ "${RUNNER_MODE:-cargo}" = "release" ] && [ -x ./target/release/parakeet-ptt ]; then
                    runner_bin="./target/release/parakeet-ptt"
                else
                    echo "[helper] running cargo run --release" >> "$LOG_CLIENT"
                fi

                eval "set -- $PTT_ARGS_SHELL"
                args=("$@")

                if [ -n "$runner_bin" ]; then
                    "$runner_bin" "${args[@]}" 2>&1 | tee -a "$LOG_CLIENT"
                else
                    cargo run --release -- "${args[@]}" 2>&1 | tee -a "$LOG_CLIENT"
                fi
            '

            tmux new-session -d -s "$TMUX_SESSION" -n "$TMUX_WINDOW" -c "$CLIENT_DIR" \
                "LOG_CLIENT=\"$LOG_CLIENT\" RUNNER_MODE=\"$runner_mode\" PTT_ARGS_SHELL=\"$ptt_args_shell\" RUST_LOG=\"$RUST_LOG\" bash -lc '$client_cmd'"
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
            stt stop
            stt start "$@"
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

            local daemon_cmd="RUST_LOG=\"$RUST_LOG\" UV_CACHE_DIR=\"$REPO_ROOT/.uv-cache\" PARAKEET_HOST=\"$HOST\" PARAKEET_PORT=\"$PORT\" PARAKEET_SILENCE_FLOOR_DB=-60.0 uv run parakeet-stt-daemon --host \"$HOST\" --port \"$PORT\" --no-streaming >> \"$LOG_DAEMON\" 2>&1"
            local injection_mode
            local paste_shortcut
            local paste_shortcut_fallback
            local paste_strategy
            local paste_chain_delay_ms
            local paste_restore_policy
            local paste_restore_delay_ms
            local paste_post_chord_hold_ms
            local paste_copy_foreground
            local paste_mime_type
            local paste_key_backend
            local paste_backend_failure_policy
            local paste_routing_mode
            local adaptive_terminal_shortcut
            local adaptive_general_shortcut
            local adaptive_unknown_shortcut
            local focus_resolver_source
            local focus_resolve_budget_ms
            local focus_deep_scan_max_apps
            local focus_wayland_stale_ms
            local focus_wayland_transition_grace_ms
            local uinput_dwell_ms
            local paste_seat
            local paste_write_primary
            local ydotool_path
            local completion_sound
            local completion_sound_path
            local completion_sound_volume
            _load_start_vars_from_defaults
            _apply_legacy_override_vars

            local -a ptt_args
            _build_ptt_args ptt_args
            local ptt_args_shell
            ptt_args_shell="$(_args_to_shell_words ptt_args)"

            local runner_mode
            runner_mode="$(_select_client_runner_mode "$CLIENT_DIR/target/release/parakeet-ptt")"
            if [ "$runner_mode" = "cargo" ] && [ -x "$CLIENT_DIR/target/release/parakeet-ptt" ]; then
                echo "[helper] release binary missing expected start flags; falling back to cargo run --release" >> "$LOG_CLIENT"
            fi

            local client_cmd='
                set -e
                runner_bin=""
                if [ "${RUNNER_MODE:-cargo}" = "release" ] && [ -x ./target/release/parakeet-ptt ]; then
                    runner_bin="./target/release/parakeet-ptt"
                else
                    echo "[helper] running cargo run --release" >> "$LOG_CLIENT"
                fi

                eval "set -- $PTT_ARGS_SHELL"
                args=("$@")

                if [ -n "$runner_bin" ]; then
                    "$runner_bin" "${args[@]}" >> "$LOG_CLIENT" 2>&1
                else
                    cargo run --release -- "${args[@]}" >> "$LOG_CLIENT" 2>&1
                fi
            '

            tmux new-session -d -s "$TMUX_SESSION" -n daemon -c "$DAEMON_DIR" "$daemon_cmd"
            tmux new-window -t "$TMUX_SESSION" -n client -c "$CLIENT_DIR" "LOG_CLIENT=\"$LOG_CLIENT\" RUNNER_MODE=\"$runner_mode\" PTT_ARGS_SHELL=\"$ptt_args_shell\" RUST_LOG=\"$RUST_LOG\" bash -lc '$client_cmd'"
            tmux new-window -t "$TMUX_SESSION" -n logs -c /tmp "tail -f \"$LOG_DAEMON\" \"$LOG_CLIENT\""
            tmux select-window -t "$TMUX_SESSION:daemon"
            tmux attach -t "$TMUX_SESSION"
            ;;
        check)
            echo ">>> Health check (daemon --check)..."
            (
                cd "$DAEMON_DIR" || exit 1
                UV_CACHE_DIR="$REPO_ROOT/.uv-cache" uv run parakeet-stt-daemon --check
            )
            ;;
        diag-injector)
            echo ">>> Clipboard injector diagnostics (test-injection)"
            (
                cd "$CLIENT_DIR" || exit 1
                set -e
                local runner_mode runner_bin
                runner_mode="$(_select_client_runner_mode "$CLIENT_DIR/target/release/parakeet-ptt")"
                runner_bin=""
                if [ "$runner_mode" = "release" ]; then
                    runner_bin="./target/release/parakeet-ptt"
                elif [ -x "$CLIENT_DIR/target/release/parakeet-ptt" ]; then
                    echo "   - release binary missing expected start flags; using cargo run --release"
                fi

                echo "   - capability: wtype=$(command -v wtype >/dev/null 2>&1 && echo yes || echo no)"
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
                    local shortcut="$1"
                    local fallback="$2"
                    local injection_mode
                    local paste_shortcut
                    local paste_shortcut_fallback
                    local paste_strategy
                    local paste_chain_delay_ms
                    local paste_restore_policy
                    local paste_restore_delay_ms
                    local paste_post_chord_hold_ms
                    local paste_copy_foreground
                    local paste_mime_type
                    local paste_key_backend
                    local paste_backend_failure_policy
                    local paste_routing_mode
                    local adaptive_terminal_shortcut
                    local adaptive_general_shortcut
                    local adaptive_unknown_shortcut
                    local focus_resolver_source
                    local focus_resolve_budget_ms
                    local focus_deep_scan_max_apps
                    local focus_wayland_stale_ms
                    local focus_wayland_transition_grace_ms
                    local uinput_dwell_ms
                    local paste_seat
                    local paste_write_primary
                    local ydotool_path
                    local completion_sound
                    local completion_sound_path
                    local completion_sound_volume
                    local -a ptt_args

                    _load_start_vars_from_defaults
                    injection_mode="paste"
                    paste_shortcut="$shortcut"
                    paste_shortcut_fallback="$fallback"
                    _build_ptt_args ptt_args no

                    echo "   - case shortcut=$shortcut fallback=$fallback"
                    if [ -n "$runner_bin" ]; then
                        RUST_LOG="${RUST_LOG:-parakeet_ptt=info,parakeet_ptt::injector=debug}" \
                            "$runner_bin" --test-injection "${ptt_args[@]}"
                    else
                        RUST_LOG="${RUST_LOG:-parakeet_ptt=info,parakeet_ptt::injector=debug}" \
                            cargo run --release -- --test-injection "${ptt_args[@]}"
                    fi
                }

                run_case "ctrl-shift-v" "shift-insert"
                run_case "shift-insert" "ctrl-v"
                run_case "ctrl-v" "shift-insert"
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
