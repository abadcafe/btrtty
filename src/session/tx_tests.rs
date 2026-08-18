use std::time::{Duration, Instant};

use super::tx::Transmitter;

#[test]
fn test_transmitter_rejects_invalid_limits() {
    assert!(Transmitter::new(0, 1024, 1).is_err());
    assert!(Transmitter::new(64, 63, 1).is_err());
    assert!(Transmitter::new(64, 1024, 0).is_err());
}

#[test]
fn test_transmitter_exposes_idle_byte_immediately() {
    let now = Instant::now();
    let mut transmitter = Transmitter::new(64, 1024, 4).unwrap();
    transmitter.append(b"x", now).unwrap();

    let write = transmitter.start_write().unwrap();
    assert_eq!(&write.data[..], b"x");
    assert_eq!(write.buffered_since, now);
}

#[test]
fn test_transmitter_coalesces_fragments_until_submission() {
    let started = Instant::now();
    let mut transmitter = Transmitter::new(8, 32, 4).unwrap();
    transmitter.append(b"a", started).unwrap();
    transmitter
        .append(b"bc", started + Duration::from_millis(1))
        .unwrap();
    transmitter
        .append(b"def", started + Duration::from_millis(2))
        .unwrap();

    let write = transmitter.start_write().unwrap();
    assert_eq!(&write.data[..], b"abcdef");
    assert_eq!(write.buffered_since, started);
}

#[test]
fn test_transmitter_packs_full_mtu_in_original_order() {
    let started = Instant::now();
    let mut transmitter = Transmitter::new(4, 16, 4).unwrap();
    transmitter.append(b"ab", started).unwrap();
    transmitter.append(b"cdef", started).unwrap();
    transmitter.append(b"ghij", started).unwrap();

    let first = transmitter.start_write().unwrap();
    let second = transmitter.start_write().unwrap();
    let third = transmitter.start_write().unwrap();
    assert_eq!(&first.data[..], b"abcd");
    assert_eq!(&second.data[..], b"efgh");
    assert_eq!(&third.data[..], b"ij");
}

#[test]
fn test_transmitter_enforces_staging_byte_budget() {
    let now = Instant::now();
    let mut transmitter = Transmitter::new(4, 8, 2).unwrap();
    transmitter.append(b"12345678", now).unwrap();

    assert_eq!(transmitter.remaining_capacity(), 0);
    assert!(transmitter.append(b"9", now).is_err());
    let write = transmitter.start_write().unwrap();
    assert_eq!(&write.data[..], b"1234");
    assert_eq!(transmitter.remaining_capacity(), 4);
}

#[test]
fn test_transmitter_enforces_derived_write_window() {
    let now = Instant::now();
    let mut transmitter = Transmitter::new(2, 16, 3).unwrap();
    transmitter.append(b"abcdefgh", now).unwrap();

    let writes = (0..3)
        .map(|_| transmitter.start_write().unwrap())
        .collect::<Vec<_>>();
    assert!(transmitter.window_full());
    assert!(transmitter.start_write().is_none());

    transmitter
        .complete(writes[1].token, writes[1].data.len())
        .unwrap();
    assert_eq!(&transmitter.start_write().unwrap().data[..], b"gh");
}

#[test]
fn test_transmitter_accepts_out_of_order_completions() {
    let now = Instant::now();
    let mut transmitter = Transmitter::new(2, 8, 4).unwrap();
    transmitter.append(b"abcdef", now).unwrap();
    let first = transmitter.start_write().unwrap();
    let second = transmitter.start_write().unwrap();
    let third = transmitter.start_write().unwrap();

    transmitter.complete(second.token, 2).unwrap();
    transmitter.complete(first.token, 2).unwrap();
    transmitter.complete(third.token, 2).unwrap();
    assert!(transmitter.is_drained());
}

#[test]
fn test_transmitter_rejects_invalid_completion() {
    let now = Instant::now();
    let mut transmitter = Transmitter::new(8, 8, 1).unwrap();
    transmitter.append(b"hello", now).unwrap();
    let write = transmitter.start_write().unwrap();

    assert!(
        transmitter
            .complete(write.token.next(), write.data.len())
            .is_err()
    );
    assert!(
        transmitter
            .complete(write.token, write.data.len() - 1)
            .is_err()
    );
    transmitter.complete(write.token, write.data.len()).unwrap();
}
