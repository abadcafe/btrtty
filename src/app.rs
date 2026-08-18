use std::future::Future;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::bluetooth::{BtCommand, BtEvent, BtInput, ConnectionId, DeviceAddress};
use crate::egress::{EgressLimiter, OutputRate};
use crate::session::{BluetoothSink, SessionConfig, SessionEnd, SessionRequest, run_session};

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub session: SessionConfig,
    pub output_rate: OutputRate,
}

#[derive(Debug)]
enum ServerState {
    Waiting,
    Connected {
        connection: ConnectionId,
        device: DeviceAddress,
        mtu: usize,
        paired: bool,
    },
    RunningSession {
        connection: ConnectionId,
        mtu: usize,
    },
    Disconnecting {
        connection: ConnectionId,
    },
}

/// Build the asynchronous server around an already-published native listener.
///
/// The actor's initial acceptance gate is armed before SDP/listener publication,
/// so the first connection never depends on this future being polled.
///
/// # Errors
///
/// Returns an error if an already-cancelled server cannot request native
/// shutdown.
pub fn start<S: BluetoothSink>(
    events: mpsc::UnboundedReceiver<BtEvent>,
    input: mpsc::Receiver<BtInput>,
    bluetooth: S,
    config: AppConfig,
    shutdown: CancellationToken,
) -> Result<impl Future<Output = Result<()>> + Send> {
    let stopped = shutdown.is_cancelled();
    if stopped {
        bluetooth.submit(BtCommand::Shutdown)?;
    }
    let server = Server {
        events,
        input: Some(input),
        bluetooth,
        egress: EgressLimiter::new(config.output_rate),
        config,
        shutdown,
    };
    Ok(async move { if stopped { Ok(()) } else { server.run().await } })
}

struct Server<S> {
    events: mpsc::UnboundedReceiver<BtEvent>,
    input: Option<mpsc::Receiver<BtInput>>,
    bluetooth: S,
    egress: EgressLimiter,
    config: AppConfig,
    shutdown: CancellationToken,
}

impl<S: BluetoothSink> Server<S> {
    async fn run(mut self) -> Result<()> {
        let mut state = ServerState::Waiting;
        info!("waiting for paired Bluetooth client");
        loop {
            let next = match state {
                ServerState::Waiting => self.wait_for_client().await?,
                ServerState::Connected {
                    connection,
                    device,
                    mtu,
                    paired,
                } => self.accept_client(connection, &device, mtu, paired)?,
                ServerState::RunningSession { connection, mtu } => {
                    self.run_connected_session(connection, mtu).await?
                }
                ServerState::Disconnecting { connection } => {
                    self.wait_for_disconnect(connection).await?
                }
            };
            let Some(next) = next else {
                return Ok(());
            };
            state = next;
        }
    }

    async fn wait_for_client(&mut self) -> Result<Option<ServerState>> {
        let Some(event) = self.next_event().await? else {
            return self.stop();
        };
        Ok(Some(match event {
            BtEvent::Connected {
                connection,
                device,
                mtu,
                paired,
            } => ServerState::Connected {
                connection,
                device,
                mtu,
                paired,
            },
            BtEvent::Fatal(error) => return Err(error.into()),
            BtEvent::WriteComplete { .. }
            | BtEvent::ConnectionError { .. }
            | BtEvent::Disconnected { .. } => ServerState::Waiting,
        }))
    }

    fn accept_client(
        &self,
        connection: ConnectionId,
        device: &DeviceAddress,
        mtu: usize,
        paired: bool,
    ) -> Result<Option<ServerState>> {
        if self.shutdown.is_cancelled() {
            return self.stop();
        }
        if paired {
            info!(%device, mtu, "accepted paired Bluetooth client");
            Ok(Some(ServerState::RunningSession { connection, mtu }))
        } else {
            warn!(%device, "rejecting unpaired Bluetooth device");
            self.bluetooth.submit(BtCommand::Close { connection })?;
            Ok(Some(ServerState::Disconnecting { connection }))
        }
    }

    async fn run_connected_session(
        &mut self,
        connection: ConnectionId,
        mtu: usize,
    ) -> Result<Option<ServerState>> {
        let input = self
            .input
            .take()
            .context("Bluetooth input receiver is already owned by a session")?;
        let outcome = run_session(
            &mut self.events,
            input,
            self.bluetooth.clone(),
            SessionRequest::new(
                self.config.session.clone(),
                connection,
                mtu,
                self.shutdown.clone(),
            ),
            &mut self.egress,
        )
        .await?;
        self.input = Some(outcome.input);
        match outcome.end {
            SessionEnd::Disconnected => {
                info!(
                    connection = connection.get(),
                    "Bluetooth client disconnected"
                );
                self.accept_after(connection)?;
                Ok(Some(ServerState::Waiting))
            }
            SessionEnd::ChildExited => {
                info!("PTY child exited; closing Bluetooth client");
                self.close_and_wait(connection)
            }
            SessionEnd::ConnectionFailed(error) => {
                warn!(%error, "Bluetooth session failed; closing client");
                self.close_and_wait(connection)
            }
            SessionEnd::Shutdown => self.stop(),
        }
    }

    async fn wait_for_disconnect(
        &mut self,
        connection: ConnectionId,
    ) -> Result<Option<ServerState>> {
        let Some(event) = self.next_event().await? else {
            return self.stop();
        };
        match event {
            BtEvent::Disconnected { connection: closed } if closed == connection => {
                self.accept_after(connection)?;
                Ok(Some(ServerState::Waiting))
            }
            BtEvent::Fatal(error) => Err(error.into()),
            BtEvent::Connected {
                connection: unexpected,
                ..
            } => {
                self.bluetooth.submit(BtCommand::Close {
                    connection: unexpected,
                })?;
                Ok(Some(ServerState::Disconnecting { connection }))
            }
            BtEvent::WriteComplete { .. }
            | BtEvent::ConnectionError { .. }
            | BtEvent::Disconnected { .. } => Ok(Some(ServerState::Disconnecting { connection })),
        }
    }

    async fn next_event(&mut self) -> Result<Option<BtEvent>> {
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => Ok(None),
            event = self.events.recv() => Ok(Some(event.context("Bluetooth event channel closed")?)),
        }
    }

    fn close_and_wait(&self, connection: ConnectionId) -> Result<Option<ServerState>> {
        self.bluetooth.submit(BtCommand::Close { connection })?;
        Ok(Some(ServerState::Disconnecting { connection }))
    }

    fn accept_after(&mut self, connection: ConnectionId) -> Result<()> {
        let input = self
            .input
            .as_mut()
            .context("Bluetooth input receiver is missing after session cleanup")?;
        while input.try_recv().is_ok() {}
        self.bluetooth
            .submit(BtCommand::AcceptNext { after: connection })?;
        info!("waiting for paired Bluetooth client");
        Ok(())
    }

    fn stop(&self) -> Result<Option<ServerState>> {
        self.bluetooth.submit(BtCommand::Shutdown)?;
        Ok(None)
    }
}
