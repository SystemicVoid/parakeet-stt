#!/usr/bin/env bash
# Shell functions (more powerful than aliases)

# Web scraper function - supports URL as argument: scrape example.com
scrape() {
    cd /home/hugo/Documents/Engineering/crawl4ai && uv run scrape_to_markdown.py "$@"
}

# STT system helper (robust start/stop with explicit paths and append-only logs)
stt() {
    local REPO_ROOT="$HOME/Documents/Engineering/parakeet-stt"
    local DAEMON_DIR="$REPO_ROOT/parakeet-stt-daemon"
    local CLIENT_DIR="$REPO_ROOT/parakeet-ptt"
    local LOG_CLIENT="/tmp/parakeet-ptt.log"
    local LOG_DAEMON="/tmp/parakeet-daemon.log"
    local CLIENT_PID_FILE="/tmp/parakeet-ptt.pid"
    local DAEMON_PID_FILE="/tmp/parakeet-daemon.pid"
    local DEFAULT_ENDPOINT="ws://127.0.0.1:8765/ws"
    local TMUX_SESSION="parakeet-stt"

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
                if nc -z 127.0.0.1 8765 2>/dev/null; then
                    ready=1
                    break
                fi
            else
                if python3 - <<'PY' >/dev/null 2>&1
import socket, sys
s = socket.socket()
s.settimeout(0.5)
try:
    s.connect(("127.0.0.1", 8765))
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

    _log_client() { echo "[$(date -Is)] $*" >> "$LOG_CLIENT"; }
    _log_daemon() { echo "[$(date -Is)] $*" >> "$LOG_DAEMON"; }

    case "$cmd" in
        start)
            echo ">>> Starting Parakeet STT..."

            if pgrep -f "parakeet-stt-daemon" >/dev/null; then
                echo "   - Daemon already running."
            else
                echo "   - Launching daemon..."
                _log_daemon "launch via stt helper (--no-streaming)"
                (
                    cd "$DAEMON_DIR" || exit 1
                    nohup uv run parakeet-stt-daemon --no-streaming >> "$LOG_DAEMON" 2>&1 &
                    echo $! > "$DAEMON_PID_FILE"
                )
            fi

            echo -n "   - Waiting for socket..."
            if _wait_for_socket "$DAEMON_PID_FILE" 60; then
                echo " OK"
            else
                echo " not ready; last daemon log lines:"
                tail -n 80 "$LOG_DAEMON"
                return 1
            fi

            echo "   - Launching client..."
            echo "--- Session Start: $(date) ---" >> "$LOG_CLIENT"
            _log_client "start release binary"

            if [ -f "$CLIENT_DIR/target/release/parakeet-ptt" ]; then
                (
                    cd "$CLIENT_DIR" || exit 1
                    nohup ./target/release/parakeet-ptt >> "$LOG_CLIENT" 2>&1 &
                    echo $! > "$CLIENT_PID_FILE"
                )
                if _wait_pid_alive "$CLIENT_PID_FILE" 10; then
                    _log_client "release binary running (pid $(cat "$CLIENT_PID_FILE"))"
                    echo "   - Client started (release pid $(cat "$CLIENT_PID_FILE"))"
                    return 0
                else
                    _log_client "release binary exited quickly; fallback to cargo run"
                    echo "   - Release binary exited quickly. Last log lines:"
                    tail -n 40 "$LOG_CLIENT"
                    echo "   - Attempting fallback to 'cargo run --release'..."
                fi
            fi

            if ! command -v cargo >/dev/null 2>&1; then
                echo "   - Error: 'cargo' not found in PATH. Is Rust loaded?"
                return 1
            fi

            _log_client "fallback cargo run --release -- --endpoint $DEFAULT_ENDPOINT"
            (
                cd "$CLIENT_DIR" || exit 1
                nohup cargo run --release -- --endpoint "$DEFAULT_ENDPOINT" >> "$LOG_CLIENT" 2>&1 &
                echo $! > "$CLIENT_PID_FILE"
            )
            if _wait_pid_alive "$CLIENT_PID_FILE" 14; then
                _log_client "cargo run --release running (pid $(cat "$CLIENT_PID_FILE"))"
                echo "   - Client running via cargo (pid $(cat "$CLIENT_PID_FILE"))"
                return 0
            fi

            echo "   - Client did not stay up; recent client log:"
            tail -n 120 "$LOG_CLIENT"
            return 1
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

            if _pid_alive "$DAEMON_PID_FILE"; then
                kill -TERM "$(cat "$DAEMON_PID_FILE")" 2>/dev/null || true
            fi
            pkill -f "parakeet-stt-daemon" >/dev/null 2>&1 && echo "   - Daemon stopped"

            rm -f "$CLIENT_PID_FILE" "$DAEMON_PID_FILE"
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
            if pgrep -af "parakeet" >/dev/null; then
                echo "   - Matching processes:"
                pgrep -af "parakeet" | sed 's/^/     /'
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

            echo "Creating tmux session '$TMUX_SESSION' (daemon | client | logs)..."
            echo "--- tmux session start: $(date -Is) ---" >> "$LOG_DAEMON"
            echo "--- tmux session start: $(date -Is) ---" >> "$LOG_CLIENT"

            local daemon_cmd="RUST_LOG=\"$RUST_LOG\" UV_CACHE_DIR=\"$REPO_ROOT/.uv-cache\" uv run parakeet-stt-daemon --no-streaming >> \"$LOG_DAEMON\" 2>&1"
            local client_cmd="RUST_LOG=\"$RUST_LOG\" DEFAULT_ENDPOINT=\"$DEFAULT_ENDPOINT\"; if [ -x ./target/release/parakeet-ptt ]; then exec ./target/release/parakeet-ptt >> \"$LOG_CLIENT\" 2>&1; else exec cargo run --release -- --endpoint \"$DEFAULT_ENDPOINT\" >> \"$LOG_CLIENT\" 2>&1; fi"

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
            echo "Usage: stt {start|stop|restart|status|logs [client|daemon],tmux [attach|kill],check}"
            ;;
    esac
}

# Add more functions as needed
# Example: Extract archives
# extract() {
#     if [ -f "$1" ]; then
#         case "$1" in
#             *.tar.bz2) tar xjf "$1" ;;
#             *.tar.gz)  tar xzf "$1" ;;
#             *.tar)     tar xf "$1" ;;
#             *.tbz2)    tar xjf "$1" ;;
#             *.tgz)     tar xzf "$1" ;;
#             *.zip)     unzip "$1" ;;
#             *.Z)       uncompress "$1" ;;
#             *.7z)      7z x "$1" ;;
#             *)         echo "'$1' cannot be extracted" ;;
#         esac
#     else
#         echo "'$1' is not a valid file"
#     fi
# }
