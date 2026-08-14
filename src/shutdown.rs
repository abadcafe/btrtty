use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

/// Cancel the shared shutdown token after SIGINT or SIGTERM.
///
/// # Errors
///
/// Returns an error if the platform signal handlers cannot be installed or
/// polled.
pub async fn wait_for_signal(shutdown: CancellationToken) -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;

    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("wait for SIGINT")?,
        _ = terminate.recv() => {},
    }
    shutdown.cancel();
    Ok(())
}
