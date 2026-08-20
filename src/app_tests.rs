use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::app::{self, AppConfig};
use crate::bluetooth::{BtCommand, BtEvent, BtInput, ConnectionId, DeviceAddress};
use crate::session::{BluetoothSink, SessionConfig};

#[derive(Clone, Default)]
struct RecordingSink {
    commands: Arc<Mutex<Vec<BtCommand>>>,
    changed: Arc<Notify>,
}

impl BluetoothSink for RecordingSink {
    fn submit(&self, command: BtCommand) -> anyhow::Result<()> {
        self.commands.lock().unwrap().push(command);
        self.changed.notify_one();
        Ok(())
    }
}

#[tokio::test]
async fn test_cancelled_start_submits_only_shutdown() {
    let sink = RecordingSink::default();
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let (_input_tx, input_rx) = mpsc::channel(1);
    let shutdown = CancellationToken::new();
    shutdown.cancel();

    app::start(event_rx, input_rx, sink.clone(), config(), shutdown)
        .unwrap()
        .await
        .unwrap();

    assert_eq!(commands(&sink), vec![BtCommand::Shutdown]);
}

#[tokio::test]
async fn test_shutdown_precedes_already_queued_connection() {
    let sink = RecordingSink::default();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (_input_tx, input_rx) = mpsc::channel(1);
    let shutdown = CancellationToken::new();
    let server = app::start(event_rx, input_rx, sink.clone(), config(), shutdown.clone()).unwrap();
    event_tx.send(connected(connection(1), false)).unwrap();
    shutdown.cancel();

    server.await.unwrap();

    assert_eq!(commands(&sink), vec![BtCommand::Shutdown]);
}

#[tokio::test]
async fn test_unpaired_device_is_closed_by_connection_id_then_rearms() {
    let sink = RecordingSink::default();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (input_tx, input_rx) = mpsc::channel(1);
    let shutdown = CancellationToken::new();
    let server = app::start(event_rx, input_rx, sink.clone(), config(), shutdown.clone()).unwrap();
    let task = tokio::spawn(server);
    let connection = connection(2);

    event_tx.send(connected(connection, false)).unwrap();
    wait_for(&sink, |command| {
        matches!(command, BtCommand::Close { connection: actual } if *actual == connection)
    })
    .await;
    input_tx
        .try_send(BtInput::new(
            connection,
            Bytes::from_static(b"unpaired input"),
        ))
        .unwrap();
    event_tx.send(BtEvent::Disconnected { connection }).unwrap();
    wait_for(&sink, |command| {
        matches!(
            command,
            BtCommand::AcceptNext { after: actual } if *actual == connection
        )
    })
    .await;
    input_tx
        .try_send(BtInput::new(
            ConnectionId::new(3).unwrap(),
            Bytes::from_static(b"next client"),
        ))
        .expect("stale unpaired input was not drained before accepting again");
    shutdown.cancel();
    task.await.unwrap().unwrap();

    let commands = commands(&sink);
    assert_eq!(
        commands,
        vec![
            BtCommand::Close { connection },
            BtCommand::AcceptNext { after: connection },
            BtCommand::Shutdown,
        ]
    );
}

#[tokio::test]
async fn test_stale_disconnect_cannot_reopen_accept_gate() {
    let sink = RecordingSink::default();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (_input_tx, input_rx) = mpsc::channel(1);
    let shutdown = CancellationToken::new();
    let server = app::start(event_rx, input_rx, sink.clone(), config(), shutdown.clone()).unwrap();
    let task = tokio::spawn(server);
    let active = connection(4);

    event_tx.send(connected(active, false)).unwrap();
    wait_for(
        &sink,
        |command| matches!(command, BtCommand::Close { connection } if *connection == active),
    )
    .await;
    event_tx
        .send(BtEvent::Disconnected {
            connection: connection(3),
        })
        .unwrap();
    tokio::task::yield_now().await;
    shutdown.cancel();
    task.await.unwrap().unwrap();

    assert!(
        !commands(&sink)
            .iter()
            .any(|command| matches!(command, BtCommand::AcceptNext { .. }))
    );
}

fn connected(connection: ConnectionId, paired: bool) -> BtEvent {
    BtEvent::Connected {
        connection,
        device: DeviceAddress::new("11:22:33:44:55:66").unwrap(),
        mtu: 127,
        paired,
    }
}

fn config() -> AppConfig {
    AppConfig {
        session: SessionConfig::new("exec /bin/cat", 40, 120).unwrap(),
    }
}

async fn wait_for(sink: &RecordingSink, predicate: impl Fn(&BtCommand) -> bool) {
    loop {
        if sink.commands.lock().unwrap().iter().any(&predicate) {
            return;
        }
        sink.changed.notified().await;
    }
}

fn commands(sink: &RecordingSink) -> Vec<BtCommand> {
    sink.commands.lock().unwrap().clone()
}

fn connection(value: u64) -> ConnectionId {
    ConnectionId::new(value).unwrap()
}
