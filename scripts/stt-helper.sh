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
    local default_paste_shortcut="${PARAKEET_PASTE_SHORTCUT:-ctrl-shift-v}"
    local default_paste_shortcut_fallback="${PARAKEET_PASTE_SHORTCUT_FALLBACK:-none}"
    local default_paste_strategy="${PARAKEET_PASTE_STRATEGY:-single}"
    local default_paste_chain_delay_ms="${PARAKEET_PASTE_CHAIN_DELAY_MS:-45}"
    local default_paste_restore_policy="${PARAKEET_PASTE_RESTORE_POLICY:-never}"
    local default_paste_restore_delay_ms="${PARAKEET_PASTE_RESTORE_DELAY_MS:-250}"
    local default_paste_post_chord_hold_ms="${PARAKEET_PASTE_POST_CHORD_HOLD_MS:-700}"
    local default_paste_copy_foreground="${PARAKEET_PASTE_COPY_FOREGROUND:-true}"
    local default_paste_mime_type="${PARAKEET_PASTE_MIME_TYPE:-text/plain;charset=utf-8}"
    local default_paste_key_backend="${PARAKEET_PASTE_KEY_BACKEND:-auto}"
    local default_paste_backend_failure_policy="${PARAKEET_PASTE_BACKEND_FAILURE_POLICY:-copy-only}"
    local default_paste_routing_mode="${PARAKEET_PASTE_ROUTING_MODE:-adaptive}"
    local default_adaptive_terminal_shortcut="${PARAKEET_ADAPTIVE_TERMINAL_SHORTCUT:-ctrl-shift-v}"
    local default_adaptive_general_shortcut="${PARAKEET_ADAPTIVE_GENERAL_SHORTCUT:-ctrl-v}"
    local default_adaptive_unknown_shortcut="${PARAKEET_ADAPTIVE_UNKNOWN_SHORTCUT:-ctrl-shift-v}"
    local default_uinput_dwell_ms="${PARAKEET_UINPUT_DWELL_MS:-18}"
    local default_paste_seat="${PARAKEET_PASTE_SEAT:-}"
    local default_paste_write_primary="${PARAKEET_PASTE_WRITE_PRIMARY:-false}"
    local default_ydotool_path="${PARAKEET_YDOTOOL_PATH:-}"
    local default_completion_sound="${PARAKEET_COMPLETION_SOUND:-true}"
    local default_completion_sound_path="${PARAKEET_COMPLETION_SOUND_PATH:-}"
    local default_completion_sound_volume="${PARAKEET_COMPLETION_SOUND_VOLUME:-100}"
    local default_client_ready_timeout_seconds="${PARAKEET_CLIENT_READY_TIMEOUT_SECONDS:-30}"

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

    case "$cmd" in
        start)
            local injection_mode="$default_injection_mode"
            local paste_shortcut="$default_paste_shortcut"
            local paste_shortcut_fallback="$default_paste_shortcut_fallback"
            local paste_strategy="$default_paste_strategy"
            local paste_chain_delay_ms="$default_paste_chain_delay_ms"
            local paste_restore_policy="$default_paste_restore_policy"
            local paste_restore_delay_ms="$default_paste_restore_delay_ms"
            local paste_post_chord_hold_ms="$default_paste_post_chord_hold_ms"
            local paste_copy_foreground="$default_paste_copy_foreground"
            local paste_mime_type="$default_paste_mime_type"
            local paste_key_backend="$default_paste_key_backend"
            local paste_backend_failure_policy="$default_paste_backend_failure_policy"
            local paste_routing_mode="$default_paste_routing_mode"
            local adaptive_terminal_shortcut="$default_adaptive_terminal_shortcut"
            local adaptive_general_shortcut="$default_adaptive_general_shortcut"
            local adaptive_unknown_shortcut="$default_adaptive_unknown_shortcut"
            local uinput_dwell_ms="$default_uinput_dwell_ms"
            local paste_seat="$default_paste_seat"
            local paste_write_primary="$default_paste_write_primary"
            local ydotool_path="$default_ydotool_path"
            local completion_sound="$default_completion_sound"
            local completion_sound_path="$default_completion_sound_path"
            local completion_sound_volume="$default_completion_sound_volume"
            local client_ready_timeout_seconds="$default_client_ready_timeout_seconds"
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
                    --paste-shortcut)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-shortcut"
                            return 1
                        fi
                        paste_shortcut="$2"
                        shift 2
                        ;;
                    --paste-shortcut-fallback)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-shortcut-fallback"
                            return 1
                        fi
                        paste_shortcut_fallback="$2"
                        shift 2
                        ;;
                    --paste-strategy)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-strategy"
                            return 1
                        fi
                        paste_strategy="$2"
                        shift 2
                        ;;
                    --paste-chain-delay-ms)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-chain-delay-ms"
                            return 1
                        fi
                        paste_chain_delay_ms="$2"
                        shift 2
                        ;;
                    --paste-restore-policy)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-restore-policy"
                            return 1
                        fi
                        paste_restore_policy="$2"
                        shift 2
                        ;;
                    --paste-restore-delay-ms)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-restore-delay-ms"
                            return 1
                        fi
                        paste_restore_delay_ms="$2"
                        shift 2
                        ;;
                    --paste-post-chord-hold-ms)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-post-chord-hold-ms"
                            return 1
                        fi
                        paste_post_chord_hold_ms="$2"
                        shift 2
                        ;;
                    --paste-copy-foreground)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-copy-foreground"
                            return 1
                        fi
                        paste_copy_foreground="$2"
                        shift 2
                        ;;
                    --paste-mime-type)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-mime-type"
                            return 1
                        fi
                        paste_mime_type="$2"
                        shift 2
                        ;;
                    --paste-key-backend)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-key-backend"
                            return 1
                        fi
                        paste_key_backend="$2"
                        shift 2
                        ;;
                    --paste-routing-mode)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-routing-mode"
                            return 1
                        fi
                        paste_routing_mode="$2"
                        shift 2
                        ;;
                    --adaptive-terminal-shortcut)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --adaptive-terminal-shortcut"
                            return 1
                        fi
                        adaptive_terminal_shortcut="$2"
                        shift 2
                        ;;
                    --adaptive-general-shortcut)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --adaptive-general-shortcut"
                            return 1
                        fi
                        adaptive_general_shortcut="$2"
                        shift 2
                        ;;
                    --adaptive-unknown-shortcut)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --adaptive-unknown-shortcut"
                            return 1
                        fi
                        adaptive_unknown_shortcut="$2"
                        shift 2
                        ;;
                    --paste-backend-failure-policy)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-backend-failure-policy"
                            return 1
                        fi
                        paste_backend_failure_policy="$2"
                        shift 2
                        ;;
                    --uinput-dwell-ms)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --uinput-dwell-ms"
                            return 1
                        fi
                        uinput_dwell_ms="$2"
                        shift 2
                        ;;
                    --paste-seat)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-seat"
                            return 1
                        fi
                        paste_seat="$2"
                        shift 2
                        ;;
                    --paste-write-primary)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --paste-write-primary"
                            return 1
                        fi
                        paste_write_primary="$2"
                        shift 2
                        ;;
                    --ydotool)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --ydotool"
                            return 1
                        fi
                        ydotool_path="$2"
                        shift 2
                        ;;
                    --completion-sound)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --completion-sound"
                            return 1
                        fi
                        completion_sound="$2"
                        shift 2
                        ;;
                    --completion-sound-path)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --completion-sound-path"
                            return 1
                        fi
                        completion_sound_path="$2"
                        shift 2
                        ;;
                    --completion-sound-volume)
                        if [[ $# -lt 2 ]]; then
                            echo "   - Missing value for --completion-sound-volume"
                            return 1
                        fi
                        completion_sound_volume="$2"
                        shift 2
                        ;;
                    *)
                        echo "   - Unknown option for 'stt start': $1"
                        echo "   - Run 'stt' with no args to see supported commands."
                        return 1
                        ;;
                esac
            done

            if ! [[ "$client_ready_timeout_seconds" =~ ^[0-9]+$ ]] || [ "$client_ready_timeout_seconds" -lt 1 ]; then
                echo "   - Invalid PARAKEET_CLIENT_READY_TIMEOUT_SECONDS='$client_ready_timeout_seconds'; defaulting to 30."
                client_ready_timeout_seconds=30
            fi

            echo ">>> Starting Parakeet STT (detached tmux)..."
            echo "   - Injection mode: $injection_mode"
            echo "   - Paste shortcut: $paste_shortcut"
            echo "   - Paste shortcut fallback: $paste_shortcut_fallback"
            echo "   - Paste strategy: $paste_strategy"
            echo "   - Paste chain delay (ms): $paste_chain_delay_ms"
            echo "   - Paste restore policy: $paste_restore_policy"
            echo "   - Paste restore delay (ms): $paste_restore_delay_ms"
            echo "   - Paste post-chord hold (ms): $paste_post_chord_hold_ms"
            echo "   - Paste copy foreground: $paste_copy_foreground"
            echo "   - Paste MIME type: $paste_mime_type"
            echo "   - Paste key backend: $paste_key_backend"
            echo "   - Paste backend failure policy: $paste_backend_failure_policy"
            echo "   - Paste routing mode: $paste_routing_mode"
            echo "   - Adaptive terminal shortcut: $adaptive_terminal_shortcut"
            echo "   - Adaptive general shortcut: $adaptive_general_shortcut"
            echo "   - Adaptive unknown shortcut: $adaptive_unknown_shortcut"
            echo "   - uinput dwell (ms): $uinput_dwell_ms"
            echo "   - Paste seat: ${paste_seat:-<default>}"
            echo "   - Paste write primary: $paste_write_primary"
            echo "   - Completion sound: $completion_sound"
            echo "   - Completion sound path: ${completion_sound_path:-<system default>}"
            echo "   - Completion sound volume: $completion_sound_volume"
            echo "   - Client ready timeout (s): $client_ready_timeout_seconds"

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

            local client_cmd
            client_cmd='
                set -e
                runner_bin=""
                if [ -x target/release/parakeet-ptt ]; then
                    if target/release/parakeet-ptt --help 2>&1 | grep -q -- "--injection-mode" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-shortcut" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-shortcut-fallback" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-strategy" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-chain-delay-ms" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-restore-policy" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-restore-delay-ms" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-post-chord-hold-ms" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-copy-foreground" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-mime-type" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-key-backend" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-routing-mode" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--adaptive-terminal-shortcut" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--adaptive-general-shortcut" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--adaptive-unknown-shortcut" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-backend-failure-policy" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--uinput-dwell-ms" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-seat" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-write-primary" \
                        && target/release/parakeet-ptt --help 2>&1 | grep -q -- "--completion-sound"; then
                        runner_bin="./target/release/parakeet-ptt"
                    else
                        echo "[helper] release binary missing new flags; falling back to cargo run --release" >> "$LOG_CLIENT"
                    fi
                fi
                if [ -z "$runner_bin" ]; then
                    echo "[helper] running cargo run --release" >> "$LOG_CLIENT"
                fi

                args=( \
                    --endpoint "$DEFAULT_ENDPOINT" \
                    --injection-mode "$INJECTION_MODE" \
                    --paste-shortcut "$PASTE_SHORTCUT" \
                    --paste-shortcut-fallback "$PASTE_SHORTCUT_FALLBACK" \
                    --paste-strategy "$PASTE_STRATEGY" \
                    --paste-chain-delay-ms "$PASTE_CHAIN_DELAY_MS" \
                    --paste-restore-policy "$PASTE_RESTORE_POLICY" \
                    --paste-restore-delay-ms "$PASTE_RESTORE_DELAY_MS" \
                    --paste-post-chord-hold-ms "$PASTE_POST_CHORD_HOLD_MS" \
                    --paste-copy-foreground "$PASTE_COPY_FOREGROUND" \
                    --paste-mime-type "$PASTE_MIME_TYPE" \
                    --paste-key-backend "$PASTE_KEY_BACKEND" \
                    --paste-routing-mode "$PASTE_ROUTING_MODE" \
                    --adaptive-terminal-shortcut "$ADAPTIVE_TERMINAL_SHORTCUT" \
                    --adaptive-general-shortcut "$ADAPTIVE_GENERAL_SHORTCUT" \
                    --adaptive-unknown-shortcut "$ADAPTIVE_UNKNOWN_SHORTCUT" \
                    --paste-backend-failure-policy "$PASTE_BACKEND_FAILURE_POLICY" \
                    --uinput-dwell-ms "$UINPUT_DWELL_MS" \
                    --paste-write-primary "$PASTE_WRITE_PRIMARY" \
                    --completion-sound "$COMPLETION_SOUND" \
                    --completion-sound-volume "$COMPLETION_SOUND_VOLUME" \
                )
                if [ -n "${PASTE_SEAT:-}" ]; then
                    args+=(--paste-seat "$PASTE_SEAT")
                fi
                if [ -n "${YDOTOOL_PATH:-}" ]; then
                    args+=(--ydotool "$YDOTOOL_PATH")
                fi
                if [ -n "${COMPLETION_SOUND_PATH:-}" ]; then
                    args+=(--completion-sound-path "$COMPLETION_SOUND_PATH")
                fi

                if [ -n "$runner_bin" ]; then
                    "$runner_bin" "${args[@]}" 2>&1 | tee -a "$LOG_CLIENT"
                else
                    cargo run --release -- "${args[@]}" 2>&1 | tee -a "$LOG_CLIENT"
                fi
            '

            tmux new-session -d -s "$TMUX_SESSION" -n "$TMUX_WINDOW" -c "$CLIENT_DIR" \
                "LOG_CLIENT=\"$LOG_CLIENT\" DEFAULT_ENDPOINT=\"$DEFAULT_ENDPOINT\" INJECTION_MODE=\"$injection_mode\" PASTE_SHORTCUT=\"$paste_shortcut\" PASTE_SHORTCUT_FALLBACK=\"$paste_shortcut_fallback\" PASTE_STRATEGY=\"$paste_strategy\" PASTE_CHAIN_DELAY_MS=\"$paste_chain_delay_ms\" PASTE_RESTORE_POLICY=\"$paste_restore_policy\" PASTE_RESTORE_DELAY_MS=\"$paste_restore_delay_ms\" PASTE_POST_CHORD_HOLD_MS=\"$paste_post_chord_hold_ms\" PASTE_COPY_FOREGROUND=\"$paste_copy_foreground\" PASTE_MIME_TYPE=\"$paste_mime_type\" PASTE_KEY_BACKEND=\"$paste_key_backend\" PASTE_ROUTING_MODE=\"$paste_routing_mode\" ADAPTIVE_TERMINAL_SHORTCUT=\"$adaptive_terminal_shortcut\" ADAPTIVE_GENERAL_SHORTCUT=\"$adaptive_general_shortcut\" ADAPTIVE_UNKNOWN_SHORTCUT=\"$adaptive_unknown_shortcut\" PASTE_BACKEND_FAILURE_POLICY=\"$paste_backend_failure_policy\" UINPUT_DWELL_MS=\"$uinput_dwell_ms\" PASTE_SEAT=\"$paste_seat\" PASTE_WRITE_PRIMARY=\"$paste_write_primary\" YDOTOOL_PATH=\"$ydotool_path\" COMPLETION_SOUND=\"$completion_sound\" COMPLETION_SOUND_PATH=\"$completion_sound_path\" COMPLETION_SOUND_VOLUME=\"$completion_sound_volume\" RUST_LOG=\"$RUST_LOG\" bash -lc '$client_cmd'"
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
            local injection_mode="${INJECTION_MODE:-$default_injection_mode}"
            local paste_shortcut="${PASTE_SHORTCUT:-$default_paste_shortcut}"
            local paste_shortcut_fallback="${PASTE_SHORTCUT_FALLBACK:-$default_paste_shortcut_fallback}"
            local paste_strategy="${PASTE_STRATEGY:-$default_paste_strategy}"
            local paste_chain_delay_ms="${PASTE_CHAIN_DELAY_MS:-$default_paste_chain_delay_ms}"
            local paste_restore_policy="${PASTE_RESTORE_POLICY:-$default_paste_restore_policy}"
            local paste_restore_delay_ms="${PASTE_RESTORE_DELAY_MS:-$default_paste_restore_delay_ms}"
            local paste_post_chord_hold_ms="${PASTE_POST_CHORD_HOLD_MS:-$default_paste_post_chord_hold_ms}"
            local paste_copy_foreground="${PASTE_COPY_FOREGROUND:-$default_paste_copy_foreground}"
            local paste_mime_type="${PASTE_MIME_TYPE:-$default_paste_mime_type}"
            local paste_key_backend="${PASTE_KEY_BACKEND:-$default_paste_key_backend}"
            local paste_backend_failure_policy="${PASTE_BACKEND_FAILURE_POLICY:-$default_paste_backend_failure_policy}"
            local paste_routing_mode="${PASTE_ROUTING_MODE:-$default_paste_routing_mode}"
            local adaptive_terminal_shortcut="${ADAPTIVE_TERMINAL_SHORTCUT:-$default_adaptive_terminal_shortcut}"
            local adaptive_general_shortcut="${ADAPTIVE_GENERAL_SHORTCUT:-$default_adaptive_general_shortcut}"
            local adaptive_unknown_shortcut="${ADAPTIVE_UNKNOWN_SHORTCUT:-$default_adaptive_unknown_shortcut}"
            local uinput_dwell_ms="${UINPUT_DWELL_MS:-$default_uinput_dwell_ms}"
            local paste_seat="${PASTE_SEAT:-$default_paste_seat}"
            local paste_write_primary="${PASTE_WRITE_PRIMARY:-$default_paste_write_primary}"
            local ydotool_path="${YDOTOOL_PATH:-$default_ydotool_path}"
            local completion_sound="${COMPLETION_SOUND:-$default_completion_sound}"
            local completion_sound_path="${COMPLETION_SOUND_PATH:-$default_completion_sound_path}"
            local completion_sound_volume="${COMPLETION_SOUND_VOLUME:-$default_completion_sound_volume}"
            local client_cmd='
                set -e
                runner_bin=""
                if [ -x ./target/release/parakeet-ptt ]; then
                    if ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--injection-mode" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-shortcut" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-shortcut-fallback" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-strategy" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-chain-delay-ms" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-restore-policy" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-restore-delay-ms" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-post-chord-hold-ms" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-copy-foreground" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-mime-type" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-key-backend" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-routing-mode" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--adaptive-terminal-shortcut" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--adaptive-general-shortcut" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--adaptive-unknown-shortcut" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-backend-failure-policy" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--uinput-dwell-ms" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-seat" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-write-primary" \
                        && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--completion-sound"; then
                        runner_bin="./target/release/parakeet-ptt"
                    else
                        echo "[helper] release binary missing new flags; falling back to cargo run --release" >> "$LOG_CLIENT"
                    fi
                fi
                if [ -z "$runner_bin" ]; then
                    echo "[helper] running cargo run --release" >> "$LOG_CLIENT"
                fi

                args=( \
                    --endpoint "$DEFAULT_ENDPOINT" \
                    --injection-mode "${INJECTION_MODE:-paste}" \
                    --paste-shortcut "${PASTE_SHORTCUT:-ctrl-shift-v}" \
                    --paste-shortcut-fallback "${PASTE_SHORTCUT_FALLBACK:-none}" \
                    --paste-strategy "${PASTE_STRATEGY:-single}" \
                    --paste-chain-delay-ms "${PASTE_CHAIN_DELAY_MS:-45}" \
                    --paste-restore-policy "${PASTE_RESTORE_POLICY:-never}" \
                    --paste-restore-delay-ms "${PASTE_RESTORE_DELAY_MS:-250}" \
                    --paste-post-chord-hold-ms "${PASTE_POST_CHORD_HOLD_MS:-700}" \
                    --paste-copy-foreground "${PASTE_COPY_FOREGROUND:-true}" \
                    --paste-mime-type "${PASTE_MIME_TYPE:-text/plain;charset=utf-8}" \
                    --paste-key-backend "${PASTE_KEY_BACKEND:-auto}" \
                    --paste-routing-mode "${PASTE_ROUTING_MODE:-adaptive}" \
                    --adaptive-terminal-shortcut "${ADAPTIVE_TERMINAL_SHORTCUT:-ctrl-shift-v}" \
                    --adaptive-general-shortcut "${ADAPTIVE_GENERAL_SHORTCUT:-ctrl-v}" \
                    --adaptive-unknown-shortcut "${ADAPTIVE_UNKNOWN_SHORTCUT:-ctrl-shift-v}" \
                    --paste-backend-failure-policy "${PASTE_BACKEND_FAILURE_POLICY:-copy-only}" \
                    --uinput-dwell-ms "${UINPUT_DWELL_MS:-18}" \
                    --paste-write-primary "${PASTE_WRITE_PRIMARY:-false}" \
                    --completion-sound "${COMPLETION_SOUND:-true}" \
                    --completion-sound-volume "${COMPLETION_SOUND_VOLUME:-100}" \
                )
                if [ -n "${PASTE_SEAT:-}" ]; then
                    args+=(--paste-seat "$PASTE_SEAT")
                fi
                if [ -n "${YDOTOOL_PATH:-}" ]; then
                    args+=(--ydotool "$YDOTOOL_PATH")
                fi
                if [ -n "${COMPLETION_SOUND_PATH:-}" ]; then
                    args+=(--completion-sound-path "$COMPLETION_SOUND_PATH")
                fi

                if [ -n "$runner_bin" ]; then
                    "$runner_bin" "${args[@]}" >> "$LOG_CLIENT" 2>&1
                else
                    cargo run --release -- "${args[@]}" >> "$LOG_CLIENT" 2>&1
                fi
            '

            tmux new-session -d -s "$TMUX_SESSION" -n daemon -c "$DAEMON_DIR" "$daemon_cmd"
            tmux new-window -t "$TMUX_SESSION" -n client -c "$CLIENT_DIR" "LOG_CLIENT=\"$LOG_CLIENT\" DEFAULT_ENDPOINT=\"$DEFAULT_ENDPOINT\" INJECTION_MODE=\"$injection_mode\" PASTE_SHORTCUT=\"$paste_shortcut\" PASTE_SHORTCUT_FALLBACK=\"$paste_shortcut_fallback\" PASTE_STRATEGY=\"$paste_strategy\" PASTE_CHAIN_DELAY_MS=\"$paste_chain_delay_ms\" PASTE_RESTORE_POLICY=\"$paste_restore_policy\" PASTE_RESTORE_DELAY_MS=\"$paste_restore_delay_ms\" PASTE_POST_CHORD_HOLD_MS=\"$paste_post_chord_hold_ms\" PASTE_COPY_FOREGROUND=\"$paste_copy_foreground\" PASTE_MIME_TYPE=\"$paste_mime_type\" PASTE_KEY_BACKEND=\"$paste_key_backend\" PASTE_ROUTING_MODE=\"$paste_routing_mode\" ADAPTIVE_TERMINAL_SHORTCUT=\"$adaptive_terminal_shortcut\" ADAPTIVE_GENERAL_SHORTCUT=\"$adaptive_general_shortcut\" ADAPTIVE_UNKNOWN_SHORTCUT=\"$adaptive_unknown_shortcut\" PASTE_BACKEND_FAILURE_POLICY=\"$paste_backend_failure_policy\" UINPUT_DWELL_MS=\"$uinput_dwell_ms\" PASTE_SEAT=\"$paste_seat\" PASTE_WRITE_PRIMARY=\"$paste_write_primary\" YDOTOOL_PATH=\"$ydotool_path\" COMPLETION_SOUND=\"$completion_sound\" COMPLETION_SOUND_PATH=\"$completion_sound_path\" COMPLETION_SOUND_VOLUME=\"$completion_sound_volume\" RUST_LOG=\"$RUST_LOG\" bash -lc '$client_cmd'"
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
                runner_bin=""
                if [ -x ./target/release/parakeet-ptt ] \
                    && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-strategy" \
                    && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-key-backend" \
                    && ./target/release/parakeet-ptt --help 2>&1 | grep -q -- "--paste-backend-failure-policy"; then
                    runner_bin="./target/release/parakeet-ptt"
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
                    echo "   - case shortcut=$shortcut fallback=$fallback"
                    if [ -n "$runner_bin" ]; then
                        RUST_LOG="${RUST_LOG:-parakeet_ptt=info,parakeet_ptt::injector=debug}" \
                            "$runner_bin" --test-injection --injection-mode paste \
                            --paste-shortcut "$shortcut" \
                            --paste-shortcut-fallback "$fallback" \
                            --paste-strategy "${PARAKEET_PASTE_STRATEGY:-single}" \
                            --paste-chain-delay-ms "${PARAKEET_PASTE_CHAIN_DELAY_MS:-45}" \
                            --paste-restore-policy "${PARAKEET_PASTE_RESTORE_POLICY:-never}" \
                            --paste-restore-delay-ms "${PARAKEET_PASTE_RESTORE_DELAY_MS:-250}" \
                            --paste-post-chord-hold-ms "${PARAKEET_PASTE_POST_CHORD_HOLD_MS:-700}" \
                            --paste-copy-foreground "${PARAKEET_PASTE_COPY_FOREGROUND:-true}" \
                            --paste-mime-type "${PARAKEET_PASTE_MIME_TYPE:-text/plain;charset=utf-8}" \
                            --paste-key-backend "${PARAKEET_PASTE_KEY_BACKEND:-auto}" \
                            --paste-routing-mode "${PARAKEET_PASTE_ROUTING_MODE:-adaptive}" \
                            --adaptive-terminal-shortcut "${PARAKEET_ADAPTIVE_TERMINAL_SHORTCUT:-ctrl-shift-v}" \
                            --adaptive-general-shortcut "${PARAKEET_ADAPTIVE_GENERAL_SHORTCUT:-ctrl-v}" \
                            --adaptive-unknown-shortcut "${PARAKEET_ADAPTIVE_UNKNOWN_SHORTCUT:-ctrl-shift-v}" \
                            --paste-backend-failure-policy "${PARAKEET_PASTE_BACKEND_FAILURE_POLICY:-copy-only}" \
                            --uinput-dwell-ms "${PARAKEET_UINPUT_DWELL_MS:-18}" \
                            --paste-write-primary "${PARAKEET_PASTE_WRITE_PRIMARY:-false}"
                    else
                        RUST_LOG="${RUST_LOG:-parakeet_ptt=info,parakeet_ptt::injector=debug}" \
                            cargo run --release -- --test-injection --injection-mode paste \
                            --paste-shortcut "$shortcut" \
                            --paste-shortcut-fallback "$fallback" \
                            --paste-strategy "${PARAKEET_PASTE_STRATEGY:-single}" \
                            --paste-chain-delay-ms "${PARAKEET_PASTE_CHAIN_DELAY_MS:-45}" \
                            --paste-restore-policy "${PARAKEET_PASTE_RESTORE_POLICY:-never}" \
                            --paste-restore-delay-ms "${PARAKEET_PASTE_RESTORE_DELAY_MS:-250}" \
                            --paste-post-chord-hold-ms "${PARAKEET_PASTE_POST_CHORD_HOLD_MS:-700}" \
                            --paste-copy-foreground "${PARAKEET_PASTE_COPY_FOREGROUND:-true}" \
                            --paste-mime-type "${PARAKEET_PASTE_MIME_TYPE:-text/plain;charset=utf-8}" \
                            --paste-key-backend "${PARAKEET_PASTE_KEY_BACKEND:-auto}" \
                            --paste-routing-mode "${PARAKEET_PASTE_ROUTING_MODE:-adaptive}" \
                            --adaptive-terminal-shortcut "${PARAKEET_ADAPTIVE_TERMINAL_SHORTCUT:-ctrl-shift-v}" \
                            --adaptive-general-shortcut "${PARAKEET_ADAPTIVE_GENERAL_SHORTCUT:-ctrl-v}" \
                            --adaptive-unknown-shortcut "${PARAKEET_ADAPTIVE_UNKNOWN_SHORTCUT:-ctrl-shift-v}" \
                            --paste-backend-failure-policy "${PARAKEET_PASTE_BACKEND_FAILURE_POLICY:-copy-only}" \
                            --uinput-dwell-ms "${PARAKEET_UINPUT_DWELL_MS:-18}" \
                            --paste-write-primary "${PARAKEET_PASTE_WRITE_PRIMARY:-false}"
                    fi
                }

                run_case "ctrl-shift-v" "shift-insert"
                run_case "shift-insert" "ctrl-v"
                run_case "ctrl-v" "shift-insert"
            )
            ;;
        *)
            echo "Usage: stt {start|stop|restart|status|logs [client|daemon],show,tmux [attach|kill],check,diag-injector}"
            ;;
    esac
}

# To use: source scripts/stt-helper.sh (or copy this function into your shell rc)
