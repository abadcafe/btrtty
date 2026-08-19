mod input;
mod pty;
mod tx;

use std::pin::Pin;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

use crate::bluetooth::{
    BtCommand, BtError, BtEvent, BtInput, ConnectionId, MAX_RFCOMM_WRITE_COMMANDS,
};
use crate::egress::{EgressDecision, EgressLimiter};
use tx::{Transmitter, TxWrite};

const MAX_PTY_READ_SIZE: usize = 16 * 1024;

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
pub(crate) enum SessionEnd {
    Disconnected,
    ChildExited,
    ConnectionFailed(BtError),
    Shutdown,
}

pub(crate) struct SessionRequest {
    config: SessionConfig,
    connection: ConnectionId,
    mtu: usize,
    shutdown: CancellationToken,
}

impl SessionRequest {
    #[must_use]
    pub(crate) fn new(
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

pub(crate) struct SessionOutcome {
    pub(crate) end: SessionEnd,
    pub(crate) input: mpsc::Receiver<BtInput>,
}

/// Bridge one connected RFCOMM client to a newly spawned PTY child.
///
/// # Errors
///
/// Returns an error when PTY setup/I/O, child management, transmit-state
/// validation, or Bluetooth command submission fails.
pub(crate) async fn run_session<S: BluetoothSink>(
    events: &mut mpsc::UnboundedReceiver<BtEvent>,
    input: mpsc::Receiver<BtInput>,
    bluetooth: S,
    request: SessionRequest,
    egress: &mut EgressLimiter,
) -> Result<SessionOutcome> {
    let SessionRequest {
        config,
        connection,
        mtu,
        shutdown,
    } = request;
    let staging_budget = egress.staging_budget(mtu)?;
    let write_window = egress.write_window(mtu, MAX_RFCOMM_WRITE_COMMANDS)?;
    egress.configure_mtu(mtu, Instant::now())?;
    let transmitter = Transmitter::new(mtu, staging_budget, write_window)?;
    let mut process = pty::spawn(&config).context("spawn PTY session")?;
    let input_cancel = CancellationToken::new();
    let mut input_task = tokio::spawn(input::pump(
        input,
        connection,
        process.writer,
        input_cancel.clone(),
    ));
    let mut early_input_join = None;

    let primary = {
        let mut bridge_state = Bridge {
            events,
            bluetooth: &bluetooth,
            child: &mut process.child,
            reader: &mut process.reader,
            egress,
            shutdown: &shutdown,
            connection,
        };
        let bridge = bridge_state.run(transmitter);
        tokio::pin!(bridge);

        tokio::select! {
            biased;
            result = &mut bridge => PrimaryEnd::Bridge(result),
            joined = &mut input_task => {
                early_input_join = Some(joined);
                PrimaryEnd::Input
            }
        }
    };

    input_cancel.cancel();
    let input_join = match early_input_join {
        Some(joined) => joined,
        None => input_task.await,
    };
    let mut input_outcome = match input_join {
        Ok(outcome) => outcome,
        Err(error) => {
            let cleanup = cleanup_child(&mut process.child).await;
            return merge_join_failure(primary, &error, cleanup);
        }
    };

    let session_result = match primary {
        PrimaryEnd::Bridge(result) => result,
        PrimaryEnd::Input => {
            let error = input_outcome
                .error
                .take()
                .unwrap_or_else(|| anyhow!("PTY input pump stopped before the session ended"));
            Err(error)
        }
    };

    let input_result = input_outcome.error.take().map_or(Ok(()), Err);
    let cleanup_result = cleanup_child(&mut process.child).await;
    while input_outcome.receiver.try_recv().is_ok() {}

    let end = merge_session_results(session_result, input_result, cleanup_result)?;
    Ok(SessionOutcome {
        end,
        input: input_outcome.receiver,
    })
}

enum PrimaryEnd {
    Bridge(Result<SessionEnd>),
    Input,
}

struct Bridge<'a, S> {
    events: &'a mut mpsc::UnboundedReceiver<BtEvent>,
    bluetooth: &'a S,
    child: &'a mut tokio::process::Child,
    reader: &'a mut pty_process::OwnedReadPty,
    egress: &'a mut EgressLimiter,
    shutdown: &'a CancellationToken,
    connection: ConnectionId,
}

impl<S: BluetoothSink> Bridge<'_, S> {
    async fn run(&mut self, mut transmitter: Transmitter) -> Result<SessionEnd> {
        let send_timer = tokio::time::sleep_until(Instant::now());
        tokio::pin!(send_timer);
        let mut timer_armed = false;
        let mut child_exited = false;
        let mut output_closed = false;
        let mut read_buffer = vec![0_u8; MAX_PTY_READ_SIZE.min(transmitter.remaining_capacity())];

        loop {
            schedule_next(
                &mut transmitter,
                self.egress,
                &mut timer_armed,
                send_timer.as_mut(),
                self.bluetooth,
                self.connection,
            )?;

            if child_exited && output_closed && transmitter.is_drained() {
                return Ok(SessionEnd::ChildExited);
            }

            let read_capacity = read_buffer.len().min(transmitter.remaining_capacity());
            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => return Ok(SessionEnd::Shutdown),
                event = self.events.recv() => {
                    let event = event.context("Bluetooth event channel closed")?;
                    if let Some(end) = self.handle_event(&mut transmitter, event)? {
                        return Ok(end);
                    }
                }
                () = &mut send_timer, if timer_armed => {
                    timer_armed = false;
                }
                status = self.child.wait(), if !child_exited => {
                    status.context("wait for PTY child")?;
                    child_exited = true;
                }
                output = self.reader.read(&mut read_buffer[..read_capacity]),
                    if !output_closed && read_capacity > 0 =>
                {
                    let count = output.context("read PTY output")?;
                    if count == 0 {
                        output_closed = true;
                    } else {
                        if transmitter.is_drained() {
                            self.egress.begin_burst(Instant::now())?;
                        }
                        transmitter.append(&read_buffer[..count])?;
                    }
                }
            }
        }
    }

    fn handle_event(
        &mut self,
        transmitter: &mut Transmitter,
        event: BtEvent,
    ) -> Result<Option<SessionEnd>> {
        match event {
            BtEvent::WriteComplete {
                connection,
                token,
                bytes_written,
                status,
            } if connection == self.connection => {
                if status != 0 {
                    bail!("RFCOMM async write {token:?} failed with IOReturn {status:#x}");
                }
                transmitter.complete(token, bytes_written)?;
                Ok(None)
            }
            BtEvent::ConnectionError { connection, error } if connection == self.connection => {
                Ok(Some(SessionEnd::ConnectionFailed(error)))
            }
            BtEvent::Disconnected { connection } if connection == self.connection => {
                Ok(Some(SessionEnd::Disconnected))
            }
            BtEvent::Fatal(error) => Err(error.into()),
            BtEvent::Connected { connection, .. } if connection == self.connection => {
                bail!("received duplicate Bluetooth connection event during a session")
            }
            BtEvent::Connected { .. }
            | BtEvent::WriteComplete { .. }
            | BtEvent::ConnectionError { .. }
            | BtEvent::Disconnected { .. } => Ok(None),
        }
    }
}

fn schedule_next<S: BluetoothSink>(
    transmitter: &mut Transmitter,
    egress: &mut EgressLimiter,
    timer_armed: &mut bool,
    mut send_timer: Pin<&mut Sleep>,
    bluetooth: &S,
    connection: ConnectionId,
) -> Result<()> {
    if *timer_armed {
        return Ok(());
    }

    loop {
        if !transmitter.has_buffered_data() || transmitter.window_full() {
            *timer_armed = false;
            return Ok(());
        }

        let now = Instant::now();
        let maximum_bytes = transmitter
            .next_write_len()
            .expect("buffered transmitter has a next write length");
        let permit = match egress.reserve_up_to(maximum_bytes, now)? {
            EgressDecision::Send(permit) => permit,
            EgressDecision::WaitUntil(deadline) => {
                if !*timer_armed || send_timer.deadline() != deadline {
                    send_timer.as_mut().reset(deadline);
                }
                *timer_armed = true;
                return Ok(());
            }
        };

        let byte_count = permit.byte_count();
        let write = transmitter.start_write(byte_count)?;
        submit_write(bluetooth, connection, write)?;
        permit.commit(Instant::now());
        *timer_armed = false;
    }
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

async fn cleanup_child(child: &mut tokio::process::Child) -> Result<()> {
    if child.try_wait().context("inspect PTY child")?.is_none() {
        child.start_kill().context("kill PTY child")?;
        child.wait().await.context("reap PTY child")?;
    }
    Ok(())
}

fn merge_session_results(
    session: Result<SessionEnd>,
    input: Result<()>,
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
    if let Err(error) = input {
        errors.push(format!("PTY input pump failed: {error:#}"));
    }
    if let Err(error) = cleanup {
        errors.push(format!("PTY child cleanup failed: {error:#}"));
    }
    if errors.is_empty() {
        return end.ok_or_else(|| anyhow!("session ended without a result"));
    }
    bail!(errors.join("; "))
}

fn merge_join_failure(
    primary: PrimaryEnd,
    join_error: &tokio::task::JoinError,
    cleanup: Result<()>,
) -> Result<SessionOutcome> {
    let mut errors = Vec::new();
    if let PrimaryEnd::Bridge(Err(error)) = primary {
        errors.push(format!("session bridge failed: {error:#}"));
    }
    errors.push(format!("PTY input task panicked: {join_error}"));
    if let Err(error) = cleanup {
        errors.push(format!("PTY child cleanup failed: {error:#}"));
    }
    bail!(errors.join("; "))
}

#[cfg(test)]
mod scheduler_tests;
#[cfg(test)]
mod tx_tests;
