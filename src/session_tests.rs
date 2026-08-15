use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::bluetooth::{BtCommand, BtEvent};
use crate::egress::{EgressLimiter, OutputRate};
use crate::session::{BluetoothSink, SessionConfig, SessionEnd, run_session};

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
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let config =
        SessionConfig::new("stty raw -echo; printf READY; exec /bin/cat", 40, 120).unwrap();
    let payload = Bytes::from_static("中文\0\x1b[A\n".as_bytes());

    let session = run_session(&mut event_rx, sink, config, 4, &mut limiter, shutdown);
    let drive = async move {
        let mut received = Vec::new();
        let mut sent_payload = false;

        loop {
            observer.changed.notified().await;
            for command in take_commands(&observer) {
                if let BtCommand::Send { token, data } = command {
                    received.extend_from_slice(&data);
                    acknowledge(&event_tx, token, data.len());
                }
            }
            if !sent_payload && received.windows(5).any(|window| window == b"READY") {
                received.clear();
                event_tx.send(BtEvent::Data(payload.clone())).unwrap();
                sent_payload = true;
            }
            if sent_payload && received.len() >= payload.len() {
                break;
            }
        }

        event_tx.send(BtEvent::Disconnected).unwrap();
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
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let config = SessionConfig::new(
        "trap 'printf INTERRUPTED; sleep 0.1; exit 0' INT; printf READY; while :; do sleep 1; done",
        40,
        120,
    )
    .unwrap();

    let session = run_session(&mut event_rx, sink, config, 127, &mut limiter, shutdown);
    let drive = async move {
        let mut received = Vec::new();
        let mut sent_interrupt = false;
        loop {
            observer.changed.notified().await;
            for command in take_commands(&observer) {
                if let BtCommand::Send { token, data } = command {
                    received.extend_from_slice(&data);
                    acknowledge(&event_tx, token, data.len());
                }
            }
            if !sent_interrupt && received.windows(5).any(|window| window == b"READY") {
                event_tx
                    .send(BtEvent::Data(Bytes::from_static(b"\x03")))
                    .unwrap();
                sent_interrupt = true;
            }
            if received
                .windows(b"INTERRUPTED".len())
                .any(|window| window == b"INTERRUPTED")
            {
                event_tx.send(BtEvent::Disconnected).unwrap();
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
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let config = SessionConfig::new("stty size; sleep 0.1", 51, 177).unwrap();

    let session = run_session(&mut event_rx, sink, config, 127, &mut limiter, shutdown);
    let drive = async move {
        let mut received = Vec::new();
        loop {
            observer.changed.notified().await;
            for command in take_commands(&observer) {
                if let BtCommand::Send { token, data } = command {
                    received.extend_from_slice(&data);
                    acknowledge(&event_tx, token, data.len());
                }
            }
            if received.windows(6).any(|window| window == b"51 177") {
                event_tx.send(BtEvent::Disconnected).unwrap();
                return;
            }
        }
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("fixed PTY size test timed out");
    assert_eq!(result.unwrap(), SessionEnd::Disconnected);
}

#[tokio::test]
async fn test_run_session_sets_standard_terminal_environment() {
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let config = SessionConfig::new(
        "printf '%s|%s' \"$TERM\" \"$COLORTERM\"; sleep 0.1",
        40,
        120,
    )
    .unwrap();

    let session = run_session(&mut event_rx, sink, config, 127, &mut limiter, shutdown);
    let drive = async move {
        let mut received = Vec::new();
        loop {
            observer.changed.notified().await;
            for command in take_commands(&observer) {
                if let BtCommand::Send { token, data } = command {
                    received.extend_from_slice(&data);
                    acknowledge(&event_tx, token, data.len());
                }
            }
            if received
                .windows(b"xterm-256color|truecolor".len())
                .any(|window| window == b"xterm-256color|truecolor")
            {
                event_tx.send(BtEvent::Disconnected).unwrap();
                return;
            }
        }
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("standard terminal environment test timed out");
    assert_eq!(result.unwrap(), SessionEnd::Disconnected);
}

#[tokio::test]
async fn test_run_session_cancellation_stops_child() {
    let sink = ObservableSink::default();
    let (_event_tx, mut event_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let cancel = shutdown.clone();
    let config = SessionConfig::new("exec /bin/sleep 60", 40, 120).unwrap();

    let session = run_session(&mut event_rx, sink, config, 127, &mut limiter, shutdown);
    let trigger = async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        cancel.cancel();
    };

    let (result, ()) = tokio::join!(session, trigger);
    assert_eq!(result.unwrap(), SessionEnd::Shutdown);
}

#[tokio::test]
async fn test_run_session_prefers_queued_disconnect_over_child_exit() {
    let sink = ObservableSink::default();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let mut limiter = test_limiter();
    let config = SessionConfig::new("exit 0", 40, 120).unwrap();

    event_tx.send(BtEvent::Disconnected).unwrap();
    let result = run_session(&mut event_rx, sink, config, 127, &mut limiter, shutdown)
        .await
        .unwrap();

    assert_eq!(result, SessionEnd::Disconnected);
}

#[tokio::test]
async fn test_run_session_limits_sustained_output() {
    let sink = ObservableSink::default();
    let observer = sink.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
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

    let session = run_session(&mut event_rx, sink, config, 256, &mut limiter, shutdown);
    let drive = async move {
        let mut received = 0;
        while received < 256 {
            observer.changed.notified().await;
            for command in take_commands(&observer) {
                if let BtCommand::Send { token, data } = command {
                    received += data.len();
                    acknowledge(&event_tx, token, data.len());
                }
            }
        }
        assert!(started.elapsed() >= Duration::from_millis(900));
        event_tx.send(BtEvent::Disconnected).unwrap();
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(4), async {
        tokio::join!(session, drive)
    })
    .await
    .expect("rate-limited PTY output timed out");
    assert_eq!(result.unwrap(), SessionEnd::Disconnected);
}

fn test_limiter() -> EgressLimiter {
    EgressLimiter::new(OutputRate::new(100).unwrap())
}

fn take_commands(sink: &ObservableSink) -> Vec<BtCommand> {
    let mut commands = sink.commands.lock().unwrap();
    std::mem::take(&mut *commands)
}

fn acknowledge(
    events: &mpsc::UnboundedSender<BtEvent>,
    token: crate::bluetooth::WriteToken,
    bytes_written: usize,
) {
    events
        .send(BtEvent::WriteComplete {
            token,
            bytes_written,
            status: 0,
        })
        .unwrap();
}
