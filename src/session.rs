mod input;
mod pty;
mod tx;

use std::pin::Pin;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

use crate::bluetooth::{BtCommand, BtError, BtEvent, BtInput, ConnectionId};
use crate::egress::EgressLimiter;
use tx::{TxWindow, TxWrite};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionConfig {
    command: String,
    rows: u16,
    cols: u16,
}

impl SessionConfig {
    /// Create a validated fixed-size PTY configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty command or zero rows/columns.
    pub fn new(command: impl Into<String>, rows: u16, cols: u16) -> Result<Self> {
        let command = command.into();
        if command.trim().is_empty() {
            bail!("session command cannot be empty");
        }
        if rows == 0 || cols == 0 {
            bail!("PTY rows and columns must be non-zero");
        }
        Ok(Self {
            command,
            rows,
            cols,
        })
    }
}

pub trait BluetoothSink: Clone + Send + Sync + 'static {
    /// Submit a command for main-thread execution.
    ///
    /// # Errors
    ///
    /// Returns an error if the native command queue is no longer available.
    fn submit(&self, command: BtCommand) -> Result<()>;
}

impl BluetoothSink for crate::bluetooth::MainThreadCommandQueue {
    fn submit(&self, command: BtCommand) -> Result<()> {
        Self::submit(self, command)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEnd {
    Disconnected,
    ChildExited,
    ConnectionFailed(BtError),
    Shutdown,
}

pub struct SessionRequest {
    config: SessionConfig,
    connection: ConnectionId,
    mtu: usize,
    shutdown: CancellationToken,
}

impl SessionRequest {
    #[must_use]
    pub fn new(
        config: SessionConfig,
        connection: ConnectionId,
        mtu: usize,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            config,
            connection,
            mtu,
            shutdown,
        }
    }
}

/// Bridge one connected RFCOMM client to a newly spawned PTY child.
///
/// # Errors
///
/// Returns an error when PTY setup/I/O, child management, transmit window
/// validation, or Bluetooth command submission fails.
pub async fn run_session<S: BluetoothSink>(
    events: &mut mpsc::UnboundedReceiver<BtEvent>,
    input: &mut mpsc::Receiver<BtInput>,
    bluetooth: S,
    request: SessionRequest,
    egress: &mut EgressLimiter,
) -> Result<SessionEnd> {
    let SessionRequest {
        config,
        connection,
        mtu,
        shutdown,
    } = request;
    let window = TxWindow::new(mtu)?;
    let output_capacity = egress.queue_capacity(mtu)?;
    let mut process = pty::spawn(&config).context("spawn PTY session")?;
    let (output_tx, mut output_rx) = mpsc::channel(output_capacity);
    let reader_cancel = shutdown.child_token();
    let reader = spawn_reader(process.reader, output_tx, mtu, reader_cancel.clone());

    let session_result = {
        let mut bridge_state = Bridge {
            events,
            bluetooth: &bluetooth,
            child: &mut process.child,
            output_rx: &mut output_rx,
            egress,
            shutdown: &shutdown,
            connection,
        };
        let bridge = bridge_state.run(window);
        let input_pump = input::pump(input, connection, &mut process.writer);
        tokio::pin!(bridge, input_pump);

        tokio::select! {
            biased;
            result = &mut bridge => result,
            result = &mut input_pump => match result {
                Ok(never) => match never {},
                Err(error) => Err(error),
            },
        }
    };

    reader_cancel.cancel();
    let reader_result = reader
        .await
        .context("join PTY output reader")
        .and_then(|result| result.context("read PTY output"));
    drop(process.writer);
    let cleanup_result = cleanup_child(&mut process.child).await;
    drain_input(input);

    merge_session_results(session_result, reader_result, cleanup_result)
}

fn spawn_reader(
    mut reader: pty_process::OwnedReadPty,
    output: mpsc::Sender<Bytes>,
    chunk_size: usize,
    cancel: CancellationToken,
) -> JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; chunk_size];
        loop {
            let count = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                result = reader.read(&mut buffer) => result?,
            };
            if count == 0 {
                return Ok(());
            }
            let chunk = Bytes::copy_from_slice(&buffer[..count]);
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                result = output.send(chunk) => {
                    if result.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    })
}

async fn cleanup_child(child: &mut tokio::process::Child) -> Result<()> {
    if child.try_wait().context("inspect PTY child")?.is_none() {
        child.start_kill().context("kill PTY child")?;
        child.wait().await.context("reap PTY child")?;
    }
    Ok(())
}

fn drain_input(input: &mut mpsc::Receiver<BtInput>) {
    while input.try_recv().is_ok() {}
}

fn merge_session_results(
    session: Result<SessionEnd>,
    reader: Result<()>,
    cleanup: Result<()>,
) -> Result<SessionEnd> {
    let mut errors = Vec::new();
    let end = match session {
        Ok(end) => Some(end),
        Err(error) => {
            errors.push(format!("session bridge failed: {error:#}"));
            None
        }
    };
    if let Err(error) = reader {
        errors.push(format!("PTY reader failed: {error:#}"));
    }
    if let Err(error) = cleanup {
        errors.push(format!("PTY child cleanup failed: {error:#}"));
    }
    if errors.is_empty() {
        return end.ok_or_else(|| anyhow!("session ended without a result"));
    }
    bail!(errors.join("; "))
}

struct Bridge<'a, S> {
    events: &'a mut mpsc::UnboundedReceiver<BtEvent>,
    bluetooth: &'a S,
    child: &'a mut tokio::process::Child,
    output_rx: &'a mut mpsc::Receiver<Bytes>,
    egress: &'a mut EgressLimiter,
    shutdown: &'a CancellationToken,
    connection: ConnectionId,
}

impl<S: BluetoothSink> Bridge<'_, S> {
    async fn run(&mut self, mut window: TxWindow) -> Result<SessionEnd> {
        let mut scheduled_write = None;
        let send_timer = tokio::time::sleep_until(Instant::now());
        tokio::pin!(send_timer);
        let mut child_exited = false;
        let mut output_closed = false;

        loop {
            if child_exited && output_closed && window.is_drained() && scheduled_write.is_none() {
                return Ok(SessionEnd::ChildExited);
            }

            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => return Ok(SessionEnd::Shutdown),
                event = self.events.recv() => {
                    match event.context("Bluetooth event channel closed")? {
                        BtEvent::WriteComplete { connection, token, bytes_written, status }
                            if connection == self.connection =>
                        {
                            if status != 0 {
                                bail!("RFCOMM async write {token:?} failed with IOReturn {status:#x}");
                            }
                            window.complete(token, bytes_written)?;
                            schedule_next(
                                &mut window,
                                self.egress,
                                &mut scheduled_write,
                                send_timer.as_mut(),
                                self.bluetooth,
                                self.connection,
                            )?;
                        }
                        BtEvent::ConnectionError { connection, error }
                            if connection == self.connection =>
                        {
                            return Ok(SessionEnd::ConnectionFailed(error));
                        }
                        BtEvent::Disconnected { connection } if connection == self.connection => {
                            return Ok(SessionEnd::Disconnected);
                        }
                        BtEvent::Fatal(error) => return Err(error.into()),
                        BtEvent::Connected { connection, .. } if connection == self.connection => {
                            bail!("received duplicate Bluetooth connection event during a session");
                        }
                        BtEvent::Connected { .. }
                        | BtEvent::WriteComplete { .. }
                        | BtEvent::ConnectionError { .. }
                        | BtEvent::Disconnected { .. } => {}
                    }
                }
                status = self.child.wait(), if !child_exited => {
                    status.context("wait for PTY child")?;
                    child_exited = true;
                }
                output = self.output_rx.recv(), if !output_closed && window.is_ready_for_chunk() => {
                    match output {
                        Some(data) => {
                            window.enqueue(data)?;
                            schedule_next(
                                &mut window,
                                self.egress,
                                &mut scheduled_write,
                                send_timer.as_mut(),
                                self.bluetooth,
                                self.connection,
                            )?;
                        }
                        None => output_closed = true,
                    }
                }
                () = &mut send_timer, if scheduled_write.is_some() => {
                    let Some(write) = scheduled_write.take() else {
                        bail!("send timer fired without a scheduled RFCOMM write");
                    };
                    submit_write(self.bluetooth, self.connection, write)?;
                    schedule_next(
                        &mut window,
                        self.egress,
                        &mut scheduled_write,
                        send_timer.as_mut(),
                        self.bluetooth,
                        self.connection,
                    )?;
                }
            }
        }
    }
}

fn schedule_next<S: BluetoothSink>(
    window: &mut TxWindow,
    egress: &mut EgressLimiter,
    scheduled_write: &mut Option<TxWrite>,
    mut send_timer: Pin<&mut Sleep>,
    bluetooth: &S,
    connection: ConnectionId,
) -> Result<()> {
    if scheduled_write.is_some() {
        return Ok(());
    }
    while let Some(write) = window.start_write() {
        let deadline = egress.reserve(write.data.len())?;
        if deadline <= Instant::now() {
            submit_write(bluetooth, connection, write)?;
        } else {
            *scheduled_write = Some(write);
            send_timer.as_mut().reset(deadline);
            break;
        }
    }
    Ok(())
}

fn submit_write<S: BluetoothSink>(
    bluetooth: &S,
    connection: ConnectionId,
    write: TxWrite,
) -> Result<()> {
    bluetooth.submit(BtCommand::Send {
        connection,
        token: write.token,
        data: write.data,
    })
}

#[cfg(test)]
mod tx_tests;
