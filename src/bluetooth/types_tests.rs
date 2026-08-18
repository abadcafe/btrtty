use bytes::Bytes;
use std::time::Instant;

use super::types::{BtCommand, ConnectionId, DeviceAddress, WriteToken};

#[test]
fn test_device_address_normalizes_common_separators() {
    let address = DeviceAddress::new("aa-bb-0c-0d-ee-ff").unwrap();
    assert_eq!(address.as_str(), "AA:BB:0C:0D:EE:FF");
}

#[test]
fn test_device_address_rejects_non_mac_values() {
    for value in ["", "AA:BB", "GG:BB:CC:DD:EE:FF", "AABBCCDDEEFF"] {
        assert!(DeviceAddress::new(value).is_err(), "accepted {value}");
    }
}

#[test]
fn test_write_token_never_allows_zero() {
    assert!(WriteToken::new(0).is_none());
    assert_eq!(WriteToken::new(7).unwrap().get(), 7);
}

#[test]
fn test_connection_id_never_allows_zero_and_wraps_without_zero() {
    assert_eq!(ConnectionId::new(0), None);
    assert_eq!(ConnectionId::new(7).unwrap().get(), 7);
    assert_eq!(ConnectionId::new(u64::MAX).unwrap().next().get(), 1);
}

#[test]
fn test_bt_command_send_owns_payload() {
    let connection = ConnectionId::new(9).unwrap();
    let token = WriteToken::new(1).unwrap();
    let data = Bytes::from_static(b"payload");
    let command = BtCommand::Send {
        connection,
        token,
        data: data.clone(),
        queued_at: Instant::now(),
    };
    drop(data);

    let BtCommand::Send {
        connection: sent_connection,
        data,
        ..
    } = command
    else {
        panic!("wrong command")
    };
    assert_eq!(sent_connection, connection);
    assert_eq!(&data[..], b"payload");
}
