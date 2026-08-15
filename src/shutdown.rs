use anyhow::{Context, Result, bail};
use tokio::signal::unix::{Signal, SignalKind};
use tokio_util::sync::CancellationToken;

pub struct SignalWaiter {
    interrupt: Signal,
    terminate: Signal,
}

impl SignalWaiter {
    /// Install SIGINT and SIGTERM handlers before Bluetooth is published.
    ///
    /// # Errors
    ///
    /// Returns an error if Tokio cannot register either signal source.
    pub fn install() -> Result<Self> {
        let interrupt = tokio::signal::unix::signal(SignalKind::interrupt())
            .context("install SIGINT handler")?;
        let terminate = tokio::signal::unix::signal(SignalKind::terminate())
            .context("install SIGTERM handler")?;
        Ok(Self {
            interrupt,
            terminate,
        })
    }

    /// Wait for one termination signal and cancel the shared token.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected platform signal stream closes
    /// unexpectedly.
    pub async fn wait(mut self, shutdown: CancellationToken) -> Result<()> {
        let received = tokio::select! {
            signal = self.interrupt.recv() => signal,
            signal = self.terminate.recv() => signal,
        };
        if received.is_none() {
            bail!("termination signal stream closed")
        }
        shutdown.cancel();
        Ok(())
    }
}
