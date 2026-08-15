use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::bluetooth::{BtCommand, BtEvent, DeviceAddress};
use crate::egress::{EgressLimiter, OutputRate};
use crate::session::{BluetoothSink, SessionConfig, SessionEnd, run_session};

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub session: SessionConfig,
    pub output_rate: OutputRate,
}

#[derive(Debug)]
enum ServerState {
    Starting,
    Waiting,
    Connected {
        device: DeviceAddress,
        mtu: usize,
        paired: bool,
    },
    RunningSession {
        mtu: usize,
    },
    Disconnecting,
    Stopping,
}

/// Run the asynchronous server state machine until shutdown.
///
/// # Errors
///
/// Returns an error when the Bluetooth command bridge closes, a native
/// Bluetooth operation fails, or a PTY session cannot be run or cleaned up.
pub async fn run<S: BluetoothSink>(
    mut events: mpsc::UnboundedReceiver<BtEvent>,
    bluetooth: S,
    config: AppConfig,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut state = ServerState::Starting;
    let mut egress = EgressLimiter::new(config.output_rate);

    loop {
        state = match state {
            ServerState::Starting => {
                info!("waiting for Bluetooth client");
                ServerState::Waiting
            }
            ServerState::Waiting => {
                tokio::select! {
                    biased;
                    event = events.recv() => match event {
                        Some(BtEvent::Connected { device, mtu, paired }) => ServerState::Connected { device, mtu, paired },
                        Some(BtEvent::Error(error)) => return Err(error.into()),
                        Some(_) => ServerState::Waiting,
                        None => ServerState::Stopping,
                    },
                    () = shutdown.cancelled() => ServerState::Stopping,
                }
            }
            ServerState::Connected {
                device,
                mtu,
                paired,
            } => {
                if paired {
                    info!(%device, mtu, "accepted paired Bluetooth client");
                    ServerState::RunningSession { mtu }
                } else {
                    warn!(%device, "rejecting unpaired Bluetooth device");
                    bluetooth.submit(BtCommand::Close)?;
                    ServerState::Disconnecting
                }
            }
            ServerState::RunningSession { mtu } => {
                match run_session(
                    &mut events,
                    bluetooth.clone(),
                    config.session.clone(),
                    mtu,
                    &mut egress,
                    shutdown.clone(),
                )
                .await?
                {
                    SessionEnd::Disconnected => {
                        info!("Bluetooth client disconnected");
                        ServerState::Waiting
                    }
                    SessionEnd::ChildExited => {
                        info!("PTY child exited; closing Bluetooth client");
                        bluetooth.submit(BtCommand::Close)?;
                        ServerState::Disconnecting
                    }
                    SessionEnd::Shutdown => ServerState::Stopping,
                }
            }
            ServerState::Disconnecting => {
                tokio::select! {
                    biased;
                    event = events.recv() => match event {
                        Some(BtEvent::Disconnected) => ServerState::Waiting,
                        Some(BtEvent::Error(error)) => return Err(error.into()),
                        Some(BtEvent::Connected { .. }) => {
                            bluetooth.submit(BtCommand::Close)?;
                            ServerState::Disconnecting
                        }
                        Some(_) => ServerState::Disconnecting,
                        None => ServerState::Stopping,
                    },
                    () = shutdown.cancelled() => ServerState::Stopping,
                }
            }
            ServerState::Stopping => {
                bluetooth.submit(BtCommand::Shutdown)?;
                return Ok(());
            }
        };
    }
}
