use bytes::Bytes;

use super::dispatch::{DispatchBuffer, DispatchItem, Dispatcher, NativeAction};
use super::types::{BtCommand, ConnectionId, WriteToken};

#[test]
fn test_dispatch_buffer_preserves_command_and_native_order() {
    let mut buffer = DispatchBuffer::new();
    let accept = BtCommand::AcceptNext {
        after: ConnectionId::new(1).unwrap(),
    };
    buffer.push_command(accept.clone()).unwrap();
    buffer
        .push_native(NativeAction::ChannelClosed { channel_key: 7 })
        .unwrap();

    assert_eq!(
        buffer.drain(),
        vec![
            DispatchItem::Command(accept),
            DispatchItem::Native(NativeAction::ChannelClosed { channel_key: 7 }),
        ]
    );
}

#[test]
fn test_dispatch_buffer_shutdown_keeps_native_callbacks_only() {
    let mut buffer = DispatchBuffer::new();
    buffer
        .push_command(BtCommand::AcceptNext {
            after: ConnectionId::new(2).unwrap(),
        })
        .unwrap();
    buffer
        .push_native(NativeAction::FlowAvailable { channel_key: 9 })
        .unwrap();

    buffer.close_commands();

    assert!(buffer.push_command(BtCommand::Shutdown).is_err());
    assert_eq!(
        buffer.drain(),
        vec![DispatchItem::Native(NativeAction::FlowAvailable {
            channel_key: 9
        })]
    );
}

#[test]
fn test_dispatch_buffer_deactivate_rejects_all_work() {
    let mut buffer = DispatchBuffer::new();
    buffer.deactivate();

    assert!(
        buffer
            .push_command(BtCommand::AcceptNext {
                after: ConnectionId::new(3).unwrap(),
            })
            .is_err()
    );
    assert!(
        buffer
            .push_native(NativeAction::InputOverflow { channel_key: 1 })
            .is_err()
    );
}

#[test]
fn test_dispatch_item_owns_connection_scoped_send() {
    let connection = ConnectionId::new(3).unwrap();
    let token = WriteToken::new(5).unwrap();
    let item = DispatchItem::Command(BtCommand::Send {
        connection,
        token,
        data: Bytes::from_static(b"owned"),
    });

    let DispatchItem::Command(BtCommand::Send {
        connection: actual, ..
    }) = item
    else {
        panic!("wrong dispatch item")
    };
    assert_eq!(actual, connection);
}

#[test]
fn test_dispatch_buffer_coalesces_repeatable_native_hints_until_drain() {
    let mut buffer = DispatchBuffer::new();
    let flow = NativeAction::FlowAvailable { channel_key: 11 };
    let overflow = NativeAction::InputOverflow { channel_key: 11 };

    buffer.push_native(flow).unwrap();
    buffer.push_native(flow).unwrap();
    buffer.push_native(overflow).unwrap();
    buffer.push_native(overflow).unwrap();
    assert_eq!(
        buffer.drain(),
        vec![DispatchItem::Native(flow), DispatchItem::Native(overflow)]
    );

    buffer.push_native(flow).unwrap();
    assert_eq!(buffer.drain(), vec![DispatchItem::Native(flow)]);
}

#[test]
fn test_failed_uninstalled_submission_does_not_enqueue_command() {
    let dispatcher = Dispatcher::new();
    let command_queue = dispatcher.command_queue();

    assert!(
        command_queue
            .submit(BtCommand::AcceptNext {
                after: ConnectionId::new(12).unwrap(),
            })
            .is_err()
    );
    assert!(dispatcher.drain().unwrap().is_empty());
}
