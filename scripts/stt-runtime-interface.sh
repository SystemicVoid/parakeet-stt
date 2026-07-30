# shellcheck shell=bash
# shellcheck disable=SC2034
# Shell projection of docs/protocol/runtime-interface.json.
# Keep synchronized with the documented runtime interface; drift tests compare
# this file with the Python and Rust projections.

STT_DEFAULT_DAEMON_HOST="127.0.0.1"
STT_DEFAULT_DAEMON_PORT="8765"
STT_DAEMON_WS_PATH="/ws"
STT_DAEMON_STATUS_PATH="/status"
STT_DAEMON_HEALTH_PATH="/healthz"

STT_DEFAULT_LLM_SERVER_HOST="127.0.0.1"
STT_DEFAULT_LLM_SERVER_PORT="8081"
STT_LLM_API_PATH="/v1"
STT_LLM_HEALTH_PATH="/health"
