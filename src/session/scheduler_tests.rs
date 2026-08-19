use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use super::tx::Transmitter;
use super::{BluetoothSink, schedule_next};
use crate::bluetooth::{BtCommand, ConnectionId};
use crate::egress::{EgressLimiter, OutputRate};

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
async fn test_only_the_timer_branch_can_release_an_armed_pacing_wait() {
    let now = Instant::now();
    let mut limiter = EgressLimiter::new(OutputRate::new(200).unwrap());
    limiter.configure_mtu(128, now).unwrap();
    let mut transmitter = Transmitter::new(128, 128, 1).unwrap();
    transmitter.append(b"x").unwrap();
    let sink = RecordingSink::default();
    let send_timer = tokio::time::sleep_until(now - Duration::from_millis(1));
    tokio::pin!(send_timer);
    let mut timer_armed = true;
    let connection = ConnectionId::new(1).unwrap();

    schedule_next(
        &mut transmitter,
        &mut limiter,
        &mut timer_armed,
        send_timer.as_mut(),
        &sink,
        connection,
    )
    .unwrap();
    assert!(sink.commands.lock().unwrap().is_empty());

    // This is the state transition performed by the selected Sleep branch.
    timer_armed = false;
    schedule_next(
        &mut transmitter,
        &mut limiter,
        &mut timer_armed,
        send_timer.as_mut(),
        &sink,
        connection,
    )
    .unwrap();
    assert_eq!(sink.commands.lock().unwrap().len(), 1);
}
