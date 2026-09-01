use std::sync::{Arc, Mutex};

use super::tx::Transmitter;
use super::{BluetoothSink, schedule_next};
use crate::bluetooth::{BtCommand, ConnectionId, MAX_RFCOMM_WRITE_COMMANDS};

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

#[test]
fn test_schedule_next_submits_buffered_output_immediately() {
    let mut transmitter = Transmitter::new(128).unwrap();
    transmitter.append(&[b'x'; 128]).unwrap();
    let sink = RecordingSink::default();

    schedule_next(&mut transmitter, &sink, ConnectionId::new(1).unwrap()).unwrap();

    let commands = sink.commands.lock().unwrap();
    assert_eq!(commands.len(), 1);
    let BtCommand::Send { data, .. } = &commands[0] else {
        panic!("scheduler submitted a non-write command");
    };
    assert_eq!(data.len(), 128);
}

#[test]
fn test_schedule_next_submits_partial_output_without_waiting_for_prior_completion() {
    let mut transmitter = Transmitter::new(128).unwrap();
    let sink = RecordingSink::default();
    let connection = ConnectionId::new(1).unwrap();

    transmitter.append(b"first").unwrap();
    schedule_next(&mut transmitter, &sink, connection).unwrap();
    transmitter.append(b"second").unwrap();
    schedule_next(&mut transmitter, &sink, connection).unwrap();

    let commands = sink.commands.lock().unwrap();
    assert_eq!(commands.len(), 2);
    let BtCommand::Send { data, .. } = &commands[0] else {
        panic!("scheduler submitted a non-write command");
    };
    assert_eq!(&data[..], b"first");
    let BtCommand::Send { data, .. } = &commands[1] else {
        panic!("scheduler submitted a non-write command");
    };
    assert_eq!(&data[..], b"second");
}

#[test]
fn test_schedule_next_stops_at_native_window_with_one_staged_mtu() {
    let mut transmitter = Transmitter::new(128).unwrap();
    let sink = RecordingSink::default();
    let connection = ConnectionId::new(1).unwrap();

    for _ in 0..MAX_RFCOMM_WRITE_COMMANDS {
        transmitter.append(&[b'x'; 128]).unwrap();
        schedule_next(&mut transmitter, &sink, connection).unwrap();
    }
    transmitter.append(&[b'y'; 128]).unwrap();
    schedule_next(&mut transmitter, &sink, connection).unwrap();

    assert_eq!(
        sink.commands.lock().unwrap().len(),
        MAX_RFCOMM_WRITE_COMMANDS
    );
    assert_eq!(transmitter.remaining_capacity(), 0);
}
