use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    parakeet_ptt::overlay_renderer::run_from_env_args().await
}
