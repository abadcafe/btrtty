use std::convert::Infallible;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::bluetooth::{BtInput, ConnectionId};

pub(super) async fn pump(
    input: &mut mpsc::Receiver<BtInput>,
    connection: ConnectionId,
    writer: &mut pty_process::OwnedWritePty,
) -> Result<Infallible> {
    loop {
        let input = input
            .recv()
            .await
            .context("Bluetooth input channel closed")?;
        if input.connection != connection {
            continue;
        }
        writer
            .write_all(&input.data)
            .await
            .context("write raw RFCOMM data to PTY")?;
    }
}
