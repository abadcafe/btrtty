use std::time::Instant;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::metrics::InputMetrics;
use crate::bluetooth::{BtInput, ConnectionId};

pub(super) struct InputOutcome {
    pub(super) receiver: mpsc::Receiver<BtInput>,
    pub(super) error: Option<anyhow::Error>,
}

pub(super) async fn pump(
    mut receiver: mpsc::Receiver<BtInput>,
    connection: ConnectionId,
    mut writer: pty_process::OwnedWritePty,
    cancel: CancellationToken,
    metrics: InputMetrics,
) -> InputOutcome {
    let error = loop {
        let input = tokio::select! {
            biased;
            () = cancel.cancelled() => break None,
            input = receiver.recv() => match input {
                Some(input) => input,
                None => break Some(anyhow::anyhow!("Bluetooth input channel closed")),
            },
        };
        if input.connection != connection {
            continue;
        }

        let write_started = Instant::now();
        let dispatch_latency = write_started.saturating_duration_since(input.received_at);
        let bytes = input.data.len();
        let result: Result<()> = tokio::select! {
            biased;
            () = cancel.cancelled() => break None,
            result = writer.write_all(&input.data) => {
                result.context("write raw RFCOMM data to PTY")
            }
        };
        match result {
            Ok(()) => metrics.record(bytes, dispatch_latency, write_started.elapsed()),
            Err(error) => break Some(error),
        }
    };

    InputOutcome { receiver, error }
}
