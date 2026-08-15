mod pty;
mod tx;

use std::pin::Pin;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

use crate::bluetooth::{BtCommand, BtEvent};
use crate::egress::EgressLimiter;
use tx::{TxWindow, TxWrite};

const PTY_CHUNK_SIZE: usize = 256;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionEnd {
    Disconnected,
    ChildExited,
    Shutdown,
}

/// Bridge one connected RFCOMM client to a newly spawned PTY child.
///
/// # Errors
///
/// Returns an error when PTY setup/I/O, child management, transmit window
/// validation, or Bluetooth command submission fails.
pub async fn run_session<S: BluetoothSink>(
    events: &mut mpsc::UnboundedReceiver<BtEvent>,
    bluetooth: S,
    config: SessionConfig,
    mtu: usize,
    egress: &mut EgressLimiter,
    shutdown: CancellationToken,
) -> Result<SessionEnd> {
    let output_capacity = egress.queue_capacity(PTY_CHUNK_SIZE)?;
    let mut process = pty::spawn(&config).context("spawn PTY session")?;
    let (output_tx, mut output_rx) = mpsc::channel(output_capacity);
    let reader_cancel = shutdown.child_token();
    let reader_token = reader_cancel.clone();
    let reader = tokio::spawn(async move {
        let mut buffer = vec![0_u8; PTY_CHUNK_SIZE];
        loop {
            tokio::select! {
                biased;
                () = reader_token.cancelled() => return Ok::<(), std::io::Error>(()),
                result = process.reader.read(&mut buffer) => {
                    let count = result?;
                    if count == 0 {
                        return Ok(());
                    }
                    if output_tx
                        .send(Bytes::copy_from_slice(&buffer[..count]))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }
    });

    let session_result = Bridge {
        events,
        bluetooth: &bluetooth,
        writer: &mut process.writer,
        child: &mut process.child,
        output_rx: &mut output_rx,
        egress,
        shutdown: &shutdown,
    }
    .run(mtu)
    .await;

    reader_cancel.cancel();
    reader.abort();
    let _ = reader.await;
    drop(process.writer);
    let cleanup_result = async {
        if process
            .child
            .try_wait()
            .context("inspect PTY child")?
            .is_none()
        {
            process.child.start_kill().context("kill PTY child")?;
            process.child.wait().await.context("reap PTY child")?;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    match session_result {
        Ok(end) => {
            cleanup_result?;
            Ok(end)
        }
        Err(error) => {
            let _ = cleanup_result;
            Err(error)
        }
    }
}

struct Bridge<'a, S> {
    events: &'a mut mpsc::UnboundedReceiver<BtEvent>,
    bluetooth: &'a S,
    writer: &'a mut pty_process::OwnedWritePty,
    child: &'a mut tokio::process::Child,
    output_rx: &'a mut mpsc::Receiver<Bytes>,
    egress: &'a mut EgressLimiter,
    shutdown: &'a CancellationToken,
}

impl<S: BluetoothSink> Bridge<'_, S> {
    async fn run(&mut self, mtu: usize) -> Result<SessionEnd> {
        let mut window = TxWindow::new(mtu)?;
        let mut scheduled_write = None;
        let send_timer = tokio::time::sleep_until(Instant::now());
        tokio::pin!(send_timer);

        loop {
            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => return Ok(SessionEnd::Shutdown),
                event = self.events.recv() => {
                    match event.context("Bluetooth event channel closed")? {
                        BtEvent::Data(data) => {
                            self.writer.write_all(&data).await.context("write raw RFCOMM data to PTY")?;
                        }
                        BtEvent::WriteComplete { token, bytes_written, status } => {
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
                            )?;
                        }
                        BtEvent::FlowControlChanged => {}
                        BtEvent::Disconnected => return Ok(SessionEnd::Disconnected),
                        BtEvent::Error(error) => return Err(error.into()),
                        BtEvent::Connected { .. } => bail!("received a second Bluetooth connection during a session"),
                    }
                }
                status = self.child.wait() => {
                    status.context("wait for PTY child")?;
                    return Ok(SessionEnd::ChildExited);
                }
                output = self.output_rx.recv(), if window.is_ready_for_chunk() => {
                    match output {
                        Some(data) => {
                            window.enqueue(data)?;
                            schedule_next(
                                &mut window,
                                self.egress,
                                &mut scheduled_write,
                                send_timer.as_mut(),
                                self.bluetooth,
                            )?;
                        }
                        None => return Ok(SessionEnd::ChildExited),
                    }
                }
                () = &mut send_timer, if scheduled_write.is_some() => {
                    let Some(write) = scheduled_write.take() else {
                        bail!("send timer fired without a scheduled RFCOMM write");
                    };
                    submit_write(self.bluetooth, write)?;
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
) -> Result<()> {
    if scheduled_write.is_some() {
        bail!("cannot schedule overlapping RFCOMM writes");
    }
    if let Some(write) = window.start_write() {
        let deadline = egress.reserve(write.data.len())?;
        if deadline <= Instant::now() {
            submit_write(bluetooth, write)?;
        } else {
            *scheduled_write = Some(write);
            send_timer.as_mut().reset(deadline);
        }
    }
    Ok(())
}

fn submit_write<S: BluetoothSink>(bluetooth: &S, write: TxWrite) -> Result<()> {
    bluetooth.submit(BtCommand::Send {
        token: write.token,
        data: write.data,
    })
}

#[cfg(test)]
mod tx_tests;
