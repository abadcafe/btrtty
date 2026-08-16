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
    let second = window.start_write().unwrap();
    assert_eq!(&second.data[..], b"efgh");
    let third = window.start_write().unwrap();
    assert_eq!(&third.data[..], b"ij");
    assert!(window.start_write().is_none());

    window.complete(second.token, 4).unwrap();
    window.complete(first.token, 4).unwrap();
    window.complete(third.token, 2).unwrap();
    assert!(window.is_ready_for_chunk());
    assert!(window.is_drained());
}

#[test]
fn test_tx_window_keeps_eight_outstanding_writes() {
    let mut window = TxWindow::new(4).unwrap();
    window
        .enqueue(Bytes::from_static(b"abcdefghijklmnopqrstuvwxyzabcdefg"))
        .unwrap();
    let writes = (0..8)
        .map(|_| window.start_write().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        writes.iter().map(|write| write.data.len()).sum::<usize>(),
        32
    );
    assert!(window.start_write().is_none());
    assert!(!window.is_drained());
    assert!(!window.is_ready_for_chunk());
    assert!(window.enqueue(Bytes::from_static(b"later")).is_err());

    window
        .complete(writes[4].token, writes[4].data.len())
        .unwrap();
    let final_write = window.start_write().unwrap();
    assert_eq!(&final_write.data[..], b"g");
    for write in [
        writes[7].token,
        writes[0].token,
        writes[3].token,
        writes[1].token,
        writes[6].token,
        writes[2].token,
        writes[5].token,
    ] {
        window.complete(write, 4).unwrap();
    }
    window.complete(final_write.token, 1).unwrap();
    assert!(window.is_ready_for_chunk());
    assert!(window.is_drained());
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
