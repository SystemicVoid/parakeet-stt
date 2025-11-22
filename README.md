# Parakeet STT (daemon + client)

Quick commands that work from any directory on this machine:

```bash
repo="$HOME/Documents/Engineering/parakeet-stt"

# Install deps (daemon)
cd "$repo/parakeet-stt-daemon" && uv sync --dev
uv sync --extra inference --prerelease allow \
  --index https://download.pytorch.org/whl/nightly/cu124 \
  --index-strategy unsafe-best-match

# Terminal 1: daemon (logs -> /tmp/parakeet-daemon.log)
(cd "$repo/parakeet-stt-daemon" && uv run --prerelease allow \
  --index https://download.pytorch.org/whl/nightly/cu124 \
  --index-strategy unsafe-best-match \
  parakeet-stt-daemon --host 127.0.0.1 --port 8765 \
  > /tmp/parakeet-daemon.log 2>&1)

# Terminal 2: client (Rust)
(cd "$repo/parakeet-ptt" && cargo run --release)

# Optional health check before running daemon
(cd "$repo/parakeet-stt-daemon" && uv run parakeet-stt-daemon --check)
```

Systemd user units are available under `deploy/` if you want automatic startup.
