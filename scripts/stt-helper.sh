#!/usr/bin/env bash
# Parakeet STT helper (tmux-based start/stop). Source this file, then run: stt start

stt() {
    local REPO_ROOT="${PARAKEET_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
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

    export RUST_LOG="${RUST_LOG:-info}"

    local cmd="${1:-start}"
    shift || true

    _pid_alive() {
        local pid_file="$1"
        [ -f "$pid_file" ] && ps -p "$(cat "$pid_file")" >/dev/null 2>&1
    }

    _wait_for_socket() {
        local pid_file="$1"
        local tries="${2:-60}" # 30s with 0.5s sleep
        local ready=0
        for _ in $(seq 1 "$tries"); do
            if command -v nc >/dev/null 2>&1; then
                if nc -z "$HOST" "$PORT" 2>/dev/null; then
                    ready=1
                    break
                fi
            else
                if python3 - "$HOST" "$PORT" <<'PY' >/dev/null 2>&1
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
                then
                    ready=1
                    break
                fi
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
            echo ">>> Starting Parakeet STT (detached tmux)..."

            if ! _resolve_port; then
                return 1
            fi

            if pgrep -f "parakeet-stt-daemon" >/dev/null; then
                echo "   - Daemon already running."
            else
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

            if pgrep -f "parakeet-ptt" >/dev/null 2>&1; then
                echo "   - Stopping existing parakeet-ptt processes..."
                pkill -f "parakeet-ptt" >/dev/null 2>&1 || true
            fi

            echo "--- Session Start: $(date) ---" >> "$LOG_CLIENT"
            _log_client "start client in tmux"

            if tmux has-session -t "$TMUX_SESSION" >/dev/null 2>&1; then
                tmux kill-session -t "$TMUX_SESSION"
            fi

            local client_cmd
            client_cmd='
                set -e
                if [ -x target/release/parakeet-ptt ]; then
                    exec ./target/release/parakeet-ptt --endpoint "$DEFAULT_ENDPOINT" 2>&1 | tee -a "$LOG_CLIENT"
                else
                    echo "[helper] running cargo run --release" >> "$LOG_CLIENT"
                    exec cargo run --release -- --endpoint "$DEFAULT_ENDPOINT" 2>&1 | tee -a "$LOG_CLIENT"
                fi
            '

            tmux new-session -d -s "$TMUX_SESSION" -n "$TMUX_WINDOW" -c "$CLIENT_DIR" \
                "LOG_CLIENT=\"$LOG_CLIENT\" DEFAULT_ENDPOINT=\"$DEFAULT_ENDPOINT\" RUST_LOG=\"$RUST_LOG\" bash -lc '$client_cmd'"
            tmux split-window -t "$TMUX_SESSION:$TMUX_WINDOW" -v -c /tmp "bash -lc 'tail -f \"$LOG_DAEMON\" \"$LOG_CLIENT\"'"
            tmux select-layout -t "$TMUX_SESSION:$TMUX_WINDOW" even-vertical
            tmux select-pane -t "$TMUX_SESSION:$TMUX_WINDOW.0"

            local client_ok=0
            for _ in $(seq 1 20); do
                local pid
                pid=$(pgrep -n "parakeet-ptt" || true)
                if [ -n "$pid" ]; then
                    echo "$pid" > "$CLIENT_PID_FILE"
                    client_ok=1
                    break
                fi
                sleep 0.5
            done

            if [ "$client_ok" -ne 1 ]; then
                echo "   - Client did not stay up; recent client log:"
                tail -n 120 "$LOG_CLIENT"
                return 1
            fi

            echo "   - Dictation ready (tmux session: $TMUX_SESSION)."
            echo "     Use 'stt show' to view panes; Ctrl+b d to detach."
            ;;
        restart)
            stt stop
            stt start
            ;;
        stop)
            echo ">>> Stopping Parakeet..."
            if _pid_alive "$CLIENT_PID_FILE"; then
                kill -TERM "$(cat "$CLIENT_PID_FILE")" 2>/dev/null || true
            fi
            pkill -f "parakeet-ptt" >/dev/null 2>&1 && echo "   - Client stopped"
            if command -v tmux >/dev/null 2>&1 && tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
                tmux kill-session -t "$TMUX_SESSION"
            fi

            if _pid_alive "$DAEMON_PID_FILE"; then
                kill -TERM "$(cat "$DAEMON_PID_FILE")" 2>/dev/null || true
            fi
            pkill -f "parakeet-stt-daemon" >/dev/null 2>&1 && echo "   - Daemon stopped"

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
            if pgrep -af "parakeet" >/dev/null; then
                echo "   - Matching processes:"
                pgrep -af "parakeet" | sed 's/^/     /'
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

            if pgrep -af "parakeet-stt-daemon" >/dev/null || pgrep -af "parakeet-ptt" >/dev/null; then
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
            local client_cmd="RUST_LOG=\"$RUST_LOG\" DEFAULT_ENDPOINT=\"$DEFAULT_ENDPOINT\"; if [ -x ./target/release/parakeet-ptt ]; then exec ./target/release/parakeet-ptt --endpoint \"$DEFAULT_ENDPOINT\" >> \"$LOG_CLIENT\" 2>&1; else exec cargo run --release -- --endpoint \"$DEFAULT_ENDPOINT\" >> \"$LOG_CLIENT\" 2>&1; fi"

            tmux new-session -d -s "$TMUX_SESSION" -n daemon -c "$DAEMON_DIR" "$daemon_cmd"
            tmux new-window -t "$TMUX_SESSION" -n client -c "$CLIENT_DIR" "$client_cmd"
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
        *)
            echo "Usage: stt {start|stop|restart|status|logs [client|daemon],show,tmux [attach|kill],check}"
            ;;
    esac
}

# To use: source scripts/stt-helper.sh (or copy this function into your shell rc)
