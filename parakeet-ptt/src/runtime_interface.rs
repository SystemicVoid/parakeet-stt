//! Shared endpoint and path defaults for Parakeet runtime processes.
//!
//! This is the Rust projection of `docs/protocol/runtime-interface.json`.
//! Drift checks fail when the documented interface and projections diverge.

pub(crate) const DEFAULT_DAEMON_HOST: &str = "127.0.0.1";
pub(crate) const DEFAULT_DAEMON_PORT: u16 = 8765;
pub(crate) const DAEMON_WS_PATH: &str = "/ws";
pub(crate) const DAEMON_STATUS_PATH: &str = "/status";
#[allow(dead_code)]
pub(crate) const DAEMON_HEALTH_PATH: &str = "/healthz";

pub(crate) const DEFAULT_LLM_HOST: &str = "127.0.0.1";
pub(crate) const DEFAULT_LLM_PORT: u16 = 8080;
pub(crate) const LLM_API_PATH: &str = "/v1";
#[allow(dead_code)]
pub(crate) const LLM_HEALTH_PATH: &str = "/health";

pub(crate) fn endpoint_url(scheme: &str, host: &str, port: u16, path: &str) -> String {
    format!("{scheme}://{host}:{port}{path}")
}
