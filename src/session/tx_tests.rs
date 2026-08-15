use bytes::Bytes;

use super::tx::TxWindow;

#[test]
fn test_tx_window_rejects_zero_mtu() {
    assert!(TxWindow::new(0).is_err());
}

#[test]
fn test_tx_window_splits_chunks_at_mtu() {
    let mut window = TxWindow::new(4).unwrap();
    window.enqueue(Bytes::from_static(b"abcdefghij")).unwrap();

    let first = window.start_write().unwrap();
    assert_eq!(&first.data[..], b"abcd");
    assert!(window.start_write().is_none());
    window.complete(first.token, 4).unwrap();

    let second = window.start_write().unwrap();
    assert_eq!(&second.data[..], b"efgh");
    window.complete(second.token, 4).unwrap();

    let third = window.start_write().unwrap();
    assert_eq!(&third.data[..], b"ij");
    window.complete(third.token, 2).unwrap();
    assert!(window.is_ready_for_chunk());
    assert!(window.is_drained());
}

#[test]
fn test_tx_window_keeps_one_outstanding_write() {
    let mut window = TxWindow::new(8).unwrap();
    window.enqueue(Bytes::from_static(b"hello")).unwrap();
    let write = window.start_write().unwrap();

    assert!(window.start_write().is_none());
    assert!(!window.is_drained());
    assert!(!window.is_ready_for_chunk());
    assert!(window.enqueue(Bytes::from_static(b"later")).is_err());
    window.complete(write.token, write.data.len()).unwrap();
    assert!(window.is_ready_for_chunk());
}

#[test]
fn test_tx_window_rejects_wrong_completion() {
    let mut window = TxWindow::new(8).unwrap();
    window.enqueue(Bytes::from_static(b"hello")).unwrap();
    let write = window.start_write().unwrap();
    let wrong = write.token.next();

    assert!(window.complete(wrong, write.data.len()).is_err());
    assert!(window.complete(write.token, write.data.len() - 1).is_err());
    window.complete(write.token, write.data.len()).unwrap();
}
