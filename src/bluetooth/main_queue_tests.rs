use bytes::Bytes;

use super::main_queue::CommandBuffer;
use super::types::{BtCommand, WriteToken};

#[test]
fn test_command_buffer_preserves_fifo_order() {
    let mut buffer = CommandBuffer::new();
    buffer.push(BtCommand::Close).unwrap();
    buffer
        .push(BtCommand::Send {
            token: WriteToken::new(1).unwrap(),
            data: Bytes::from_static(b"x"),
        })
        .unwrap();

    let commands = buffer.drain();
    assert!(matches!(commands[0], BtCommand::Close));
    assert!(matches!(commands[1], BtCommand::Send { .. }));
}

#[test]
fn test_command_buffer_rejects_after_close() {
    let mut buffer = CommandBuffer::new();
    buffer.push(BtCommand::Close).unwrap();
    buffer.close();

    assert!(buffer.drain().is_empty());
    assert!(buffer.push(BtCommand::Shutdown).is_err());
}
