use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::bluetooth::{BtCommand, BtError, BtEvent, BtInput, ConnectionId};
use crate::egress::{EgressLimiter, OutputRate};
use crate::session::{BluetoothSink, SessionConfig, SessionEnd, SessionRequest, run_session};

#[derive(Clone, Default)]
struct ObservableSink {
    commands: Arc<Mutex<Vec<BtCommand>>>,
    changed: Arc<Notify>,
}

impl BluetoothSink for ObservableSink {
    fn submit(&self, command: BtCommand) -> anyhow::Result<()> {
        self.commands.lock().unwrap().push(command);
        self.changed.notify_one();
        Ok(())
    }
}

#[tokio::test]
async fn test_run_session_transfers_raw_rfcomm_bytes() {
    let connection = connection(1);
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (input_tx, mut input_rx) = mpsc::channel(8);
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let config =
        SessionConfig::new("stty raw -echo; printf READY; exec /bin/cat", 40, 120).unwrap();
    let payload = Bytes::from_static("中文\0\x1b[A\n".as_bytes());

    let session = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 4, shutdown),
        &mut limiter,
    );
    let drive = async move {
        let mut received = Vec::new();
        let mut sent_payload = false;

        loop {
            for command in next_commands(&observer).await {
                if let BtCommand::Send {
                    connection: actual,
                    token,
                    data,
                } = command
                {
                    assert_eq!(actual, connection);
                    received.extend_from_slice(&data);
                    acknowledge(&event_tx, connection, token, data.len());
                }
            }
            if !sent_payload && received.windows(5).any(|window| window == b"READY") {
                received.clear();
                input_tx
                    .send(BtInput {
                        connection,
                        data: payload.clone(),
                    })
                    .await
                    .unwrap();
                sent_payload = true;
            }
            if sent_payload && received.len() >= payload.len() {
                break;
            }
        }

        event_tx.send(BtEvent::Disconnected { connection }).unwrap();
        assert_eq!(received, payload);
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("raw PTY byte transfer timed out");
    assert_eq!(result.unwrap(), SessionEnd::Disconnected);
}

#[tokio::test]
async fn test_run_session_delivers_raw_ctrl_c() {
    let connection = connection(2);
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (input_tx, mut input_rx) = mpsc::channel(8);
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let config = SessionConfig::new(
        "trap 'printf INTERRUPTED; sleep 0.1; exit 0' INT; printf READY; while :; do sleep 1; done",
        40,
        120,
    )
    .unwrap();

    let session = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 127, shutdown),
        &mut limiter,
    );
    let drive = async move {
        let mut received = Vec::new();
        let mut sent_interrupt = false;
        loop {
            for command in next_commands(&observer).await {
                if let BtCommand::Send { token, data, .. } = command {
                    received.extend_from_slice(&data);
                    acknowledge(&event_tx, connection, token, data.len());
                }
            }
            if !sent_interrupt && received.windows(5).any(|window| window == b"READY") {
                input_tx
                    .send(BtInput {
                        connection,
                        data: Bytes::from_static(b"\x03"),
                    })
                    .await
                    .unwrap();
                sent_interrupt = true;
            }
            if received
                .windows(b"INTERRUPTED".len())
                .any(|window| window == b"INTERRUPTED")
            {
                event_tx.send(BtEvent::Disconnected { connection }).unwrap();
                return;
            }
        }
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("Ctrl-C delivery timed out");
    assert_eq!(result.unwrap(), SessionEnd::Disconnected);
}

#[tokio::test]
async fn test_run_session_uses_configured_pty_size() {
    assert_session_output("stty size; sleep 0.1", b"51 177", 51, 177).await;
}

#[tokio::test]
async fn test_run_session_sets_standard_terminal_environment() {
    assert_session_output(
        "printf '%s|%s' \"$TERM\" \"$COLORTERM\"; sleep 0.1",
        b"xterm-256color|truecolor",
        40,
        120,
    )
    .await;
}

#[tokio::test]
async fn test_run_session_starts_in_current_user_home() {
    assert_session_output(
        "test \"$PWD\" = \"$HOME\" && printf HOME_WORKDIR; sleep 0.1",
        b"HOME_WORKDIR",
        40,
        120,
    )
    .await;
}

#[tokio::test]
async fn test_run_session_flushes_output_after_child_exit() {
    let connection = connection(4);
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let _event_keepalive = event_tx.clone();
    let (_input_tx, mut input_rx) = mpsc::channel(1);
    let mut limiter = test_limiter();
    let config = SessionConfig::new("printf FINAL_OUTPUT", 40, 120).unwrap();

    let session = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 127, CancellationToken::new()),
        &mut limiter,
    );
    let drive = async move {
        let mut output = Vec::new();
        while !output
            .windows(b"FINAL_OUTPUT".len())
            .any(|window| window == b"FINAL_OUTPUT")
        {
            for command in next_commands(&observer).await {
                if let BtCommand::Send { token, data, .. } = command {
                    output.extend_from_slice(&data);
                    acknowledge(&event_tx, connection, token, data.len());
                }
            }
        }
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("child output flush timed out");
    assert_eq!(result.unwrap(), SessionEnd::ChildExited);
}

#[tokio::test]
async fn test_run_session_cancellation_stops_child() {
    let connection = connection(5);
    let sink = ObservableSink::default();
    let (_event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_input_tx, mut input_rx) = mpsc::channel(1);
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let cancel = shutdown.clone();
    let config = SessionConfig::new("exec /bin/sleep 60", 40, 120).unwrap();

    let session = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 127, shutdown),
        &mut limiter,
    );
    let trigger = async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        cancel.cancel();
    };

    let (result, ()) = tokio::join!(session, trigger);
    assert_eq!(result.unwrap(), SessionEnd::Shutdown);
}

#[tokio::test]
async fn test_shutdown_cancels_blocked_pty_input_write() {
    let connection = connection(6);
    let sink = ObservableSink::default();
    let (_event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (input_tx, mut input_rx) = mpsc::channel(128);
    for _ in 0..128 {
        input_tx
            .try_send(BtInput {
                connection,
                data: Bytes::from(vec![b'x'; 4096]),
            })
            .unwrap();
    }
    let shutdown = CancellationToken::new();
    let cancel = shutdown.clone();
    let mut limiter = test_limiter();
    let config = SessionConfig::new("stty raw -echo; exec /bin/sleep 60", 40, 120).unwrap();

    let session = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 4096, shutdown),
        &mut limiter,
    );
    let trigger = async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(session, trigger)
    })
    .await
    .expect("blocked PTY input write ignored shutdown");
    assert_eq!(result.unwrap(), SessionEnd::Shutdown);
}

#[tokio::test]
async fn test_run_session_prefers_matching_disconnect_over_child_exit() {
    let connection = connection(7);
    let sink = ObservableSink::default();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_input_tx, mut input_rx) = mpsc::channel(1);
    let mut limiter = test_limiter();
    let config = SessionConfig::new("exit 0", 40, 120).unwrap();

    event_tx.send(BtEvent::Disconnected { connection }).unwrap();
    let result = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 127, CancellationToken::new()),
        &mut limiter,
    )
    .await
    .unwrap();

    assert_eq!(result, SessionEnd::Disconnected);
}

#[tokio::test]
async fn test_run_session_ignores_stale_disconnect() {
    let stale = connection(7);
    let connection = connection(8);
    let sink = ObservableSink::default();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_input_tx, mut input_rx) = mpsc::channel(1);
    let mut limiter = test_limiter();
    let config = SessionConfig::new("exec /bin/sleep 60", 40, 120).unwrap();
    event_tx
        .send(BtEvent::Disconnected { connection: stale })
        .unwrap();
    event_tx.send(BtEvent::Disconnected { connection }).unwrap();

    let result = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 127, CancellationToken::new()),
        &mut limiter,
    )
    .await
    .unwrap();
    assert_eq!(result, SessionEnd::Disconnected);
}

#[tokio::test]
async fn test_connection_error_ends_only_the_matching_session() {
    let connection = connection(10);
    let error = BtError::new("test connection", -7);
    let sink = ObservableSink::default();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_input_tx, mut input_rx) = mpsc::channel(1);
    let mut limiter = test_limiter();
    let config = SessionConfig::new("exec /bin/sleep 60", 40, 120).unwrap();
    event_tx
        .send(BtEvent::ConnectionError {
            connection,
            error: error.clone(),
        })
        .unwrap();

    let result = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 127, CancellationToken::new()),
        &mut limiter,
    )
    .await
    .unwrap();
    assert_eq!(result, SessionEnd::ConnectionFailed(error));
}

#[tokio::test]
async fn test_run_session_limits_sustained_output() {
    let connection = connection(9);
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_input_tx, mut input_rx) = mpsc::channel(1);
    let mut limiter = EgressLimiter::new(OutputRate::new(1).unwrap());
    let now = Instant::now();
    assert_eq!(limiter.reserve_at(1024, now).unwrap(), now);
    let config = SessionConfig::new(
        "dd if=/dev/zero bs=256 count=1 2>/dev/null; sleep 5",
        40,
        120,
    )
    .unwrap();
    let started = Instant::now();

    let session = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 256, CancellationToken::new()),
        &mut limiter,
    );
    let drive = async move {
        let mut received = 0;
        while received < 256 {
            for command in next_commands(&observer).await {
                if let BtCommand::Send { token, data, .. } = command {
                    received += data.len();
                    acknowledge(&event_tx, connection, token, data.len());
                }
            }
        }
        assert!(started.elapsed() >= Duration::from_millis(900));
        event_tx.send(BtEvent::Disconnected { connection }).unwrap();
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(4), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("rate-limited PTY output timed out");
    assert_eq!(result.unwrap(), SessionEnd::Disconnected);
}

#[tokio::test]
async fn test_run_session_submits_four_writes_before_completion() {
    let connection = connection(11);
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_input_tx, mut input_rx) = mpsc::channel(1);
    let mut limiter = test_limiter();
    let config = SessionConfig::new(
        "dd if=/dev/zero bs=4096 count=1 2>/dev/null; sleep 5",
        40,
        120,
    )
    .unwrap();

    let session = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 64, CancellationToken::new()),
        &mut limiter,
    );
    let drive = async move {
        let mut outstanding = Vec::new();
        while outstanding.len() < 4 {
            for command in next_commands(&observer).await {
                if let BtCommand::Send { token, data, .. } = command {
                    assert!(!data.is_empty());
                    assert!(data.len() <= 64);
                    outstanding.push((token, data.len()));
                }
            }
        }
        for (token, len) in outstanding {
            acknowledge(&event_tx, connection, token, len);
        }
        event_tx.send(BtEvent::Disconnected { connection }).unwrap();
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("four-write RFCOMM window timed out");
    assert_eq!(result.unwrap(), SessionEnd::Disconnected);
}

async fn assert_session_output(command: &str, expected: &'static [u8], rows: u16, cols: u16) {
    let connection = connection(3);
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_input_tx, mut input_rx) = mpsc::channel(1);
    let mut limiter = test_limiter();
    let config = SessionConfig::new(command, rows, cols).unwrap();
    let session = run_session(
        &mut event_rx,
        &mut input_rx,
        sink,
        SessionRequest::new(config, connection, 127, CancellationToken::new()),
        &mut limiter,
    );
    let drive = async move {
        let mut received = Vec::new();
        loop {
            for command in next_commands(&observer).await {
                if let BtCommand::Send { token, data, .. } = command {
                    received.extend_from_slice(&data);
                    acknowledge(&event_tx, connection, token, data.len());
                }
            }
            if received
                .windows(expected.len())
                .any(|window| window == expected)
            {
                event_tx.send(BtEvent::Disconnected { connection }).unwrap();
                return;
            }
        }
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("PTY output assertion timed out");
    assert_eq!(result.unwrap(), SessionEnd::Disconnected);
}

fn test_limiter() -> EgressLimiter {
    EgressLimiter::new(OutputRate::new(100).unwrap())
}

async fn next_commands(sink: &ObservableSink) -> Vec<BtCommand> {
    loop {
        let commands = take_commands(sink);
        if !commands.is_empty() {
            return commands;
        }
        sink.changed.notified().await;
    }
}

fn take_commands(sink: &ObservableSink) -> Vec<BtCommand> {
    let mut commands = sink.commands.lock().unwrap();
    std::mem::take(&mut *commands)
}

fn acknowledge(
    events: &mpsc::UnboundedSender<BtEvent>,
    connection: ConnectionId,
    token: crate::bluetooth::WriteToken,
    bytes_written: usize,
) {
    events
        .send(BtEvent::WriteComplete {
            connection,
            token,
            bytes_written,
            status: 0,
        })
        .unwrap();
}

fn connection(value: u64) -> ConnectionId {
    ConnectionId::new(value).unwrap()
}
