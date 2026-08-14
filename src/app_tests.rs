use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::{self, AppConfig};
use crate::bluetooth::{BtCommand, BtEvent, DeviceAddress};
use crate::egress::OutputRate;
use crate::session::{BluetoothSink, SessionConfig};

#[derive(Clone, Default)]
struct RecordingSink {
    commands: Arc<Mutex<Vec<BtCommand>>>,
}

impl BluetoothSink for RecordingSink {
    fn submit(&self, command: BtCommand) -> anyhow::Result<()> {
        self.commands.lock().unwrap().push(command);
        Ok(())
    }
}

#[tokio::test]
async fn test_run_rejects_unpaired_device() {
    assert_connection_rejected(false, false).await;
}

#[tokio::test]
async fn test_run_rejects_unencrypted_device() {
    assert_connection_rejected(true, false).await;
}

async fn assert_connection_rejected(paired: bool, encrypted: bool) {
    let sink = RecordingSink::default();
    let commands = Arc::clone(&sink.commands);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let config = AppConfig {
        session: SessionConfig::new("exec /bin/cat", 40, 120).unwrap(),
        output_rate: OutputRate::new(20).unwrap(),
    };

    event_tx
        .send(BtEvent::Connected {
            device: DeviceAddress::new("11:22:33:44:55:66").unwrap(),
            mtu: 127,
            paired,
            encrypted,
        })
        .unwrap();
    event_tx.send(BtEvent::Disconnected).unwrap();
    shutdown.cancel();

    app::run(event_rx, sink, config, shutdown).await.unwrap();

    let commands = commands.lock().unwrap();
    assert!(matches!(commands.first(), Some(BtCommand::Close)));
    assert!(matches!(commands.last(), Some(BtCommand::Shutdown)));
}
