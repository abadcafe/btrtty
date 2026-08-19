use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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

        let result: Result<()> = tokio::select! {
            biased;
            () = cancel.cancelled() => break None,
            result = writer.write_all(&input.data) => {
                result.context("write raw RFCOMM data to PTY")
            }
        };
        if let Err(error) = result {
            break Some(error);
        }
    };

    InputOutcome { receiver, error }
}
