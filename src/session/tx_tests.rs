use super::tx::Transmitter;
use crate::bluetooth::MAX_RFCOMM_WRITE_COMMANDS;

#[test]
fn test_transmitter_rejects_invalid_mtu() {
    assert!(Transmitter::new(0).is_err());
    assert!(Transmitter::new(usize::from(u16::MAX) + 1).is_err());
}

#[test]
fn test_transmitter_staging_is_exactly_one_mtu() {
    let mut transmitter = Transmitter::new(64).unwrap();
    assert_eq!(transmitter.remaining_capacity(), 64);

    transmitter.append(&[b'x'; 64]).unwrap();
    assert_eq!(transmitter.remaining_capacity(), 0);
    assert!(transmitter.append(b"y").is_err());

    let write = start_next(&mut transmitter);
    assert_eq!(write.data.len(), 64);
    assert_eq!(transmitter.remaining_capacity(), 64);
}

#[test]
fn test_transmitter_exposes_idle_byte_immediately() {
    let mut transmitter = Transmitter::new(64).unwrap();
    transmitter.append(b"x").unwrap();

    let write = start_next(&mut transmitter);
    assert_eq!(&write.data[..], b"x");
}

#[test]
fn test_transmitter_holds_a_second_partial_write_until_the_first_completes() {
    let mut transmitter = Transmitter::new(64).unwrap();
    transmitter.append(b"first").unwrap();
    let first = start_next(&mut transmitter);

    transmitter.append(b"second").unwrap();
    assert!(transmitter.start_write().is_none());

    transmitter.complete(first.token, first.data.len()).unwrap();
    assert_eq!(&start_next(&mut transmitter).data[..], b"second");
}

#[test]
fn test_transmitter_pipelines_full_mtu_while_a_partial_write_is_outstanding() {
    let mut transmitter = Transmitter::new(8).unwrap();
    transmitter.append(b"x").unwrap();
    let partial = start_next(&mut transmitter);

    transmitter.append(b"abcdefgh").unwrap();
    let full = start_next(&mut transmitter);

    assert_eq!(&partial.data[..], b"x");
    assert_eq!(&full.data[..], b"abcdefgh");
}

#[test]
fn test_transmitter_does_not_release_a_partial_on_an_unrelated_completion() {
    let mut transmitter = Transmitter::new(8).unwrap();
    transmitter.append(b"x").unwrap();
    let partial = start_next(&mut transmitter);
    transmitter.append(b"abcdefgh").unwrap();
    let full = start_next(&mut transmitter);
    transmitter.append(b"tail").unwrap();

    transmitter.complete(full.token, full.data.len()).unwrap();
    assert!(transmitter.start_write().is_none());

    transmitter
        .complete(partial.token, partial.data.len())
        .unwrap();
    assert_eq!(&start_next(&mut transmitter).data[..], b"tail");
}

#[test]
fn test_transmitter_coalesces_fragments_until_submission() {
    let mut transmitter = Transmitter::new(8).unwrap();
    transmitter.append(b"a").unwrap();
    transmitter.append(b"bc").unwrap();
    transmitter.append(b"def").unwrap();

    let write = start_next(&mut transmitter);
    assert_eq!(&write.data[..], b"abcdef");
}

#[test]
fn test_transmitter_preserves_stream_order() {
    let mut transmitter = Transmitter::new(4).unwrap();

    transmitter.append(b"abcd").unwrap();
    let first = start_next(&mut transmitter);
    transmitter.append(b"efgh").unwrap();
    let second = start_next(&mut transmitter);
    transmitter.append(b"ij").unwrap();
    let third = start_next(&mut transmitter);

    assert_eq!(&first.data[..], b"abcd");
    assert_eq!(&second.data[..], b"efgh");
    assert_eq!(&third.data[..], b"ij");
}

#[test]
fn test_transmitter_enforces_native_write_window() {
    let mut transmitter = Transmitter::new(2).unwrap();
    let mut writes = Vec::new();

    for _ in 0..MAX_RFCOMM_WRITE_COMMANDS {
        transmitter.append(b"ab").unwrap();
        writes.push(start_next(&mut transmitter));
    }
    transmitter.append(b"cd").unwrap();

    assert!(transmitter.window_full());
    assert!(transmitter.start_write().is_none());

    transmitter
        .complete(writes[1].token, writes[1].data.len())
        .unwrap();
    assert_eq!(&start_next(&mut transmitter).data[..], b"cd");
}

#[test]
fn test_transmitter_window_is_bounded_to_eight_writes() {
    let mut transmitter = Transmitter::new(2).unwrap();

    for _ in 0..8 {
        transmitter.append(b"ab").unwrap();
        let _ = start_next(&mut transmitter);
    }

    assert!(transmitter.window_full());
}

#[test]
fn test_transmitter_accepts_out_of_order_completions() {
    let mut transmitter = Transmitter::new(2).unwrap();
    transmitter.append(b"ab").unwrap();
    let first = start_next(&mut transmitter);
    transmitter.append(b"cd").unwrap();
    let second = start_next(&mut transmitter);
    transmitter.append(b"ef").unwrap();
    let third = start_next(&mut transmitter);

    transmitter.complete(second.token, 2).unwrap();
    transmitter.complete(first.token, 2).unwrap();
    transmitter.complete(third.token, 2).unwrap();
    assert!(transmitter.is_drained());
}

#[test]
fn test_transmitter_rejects_invalid_completion() {
    let mut transmitter = Transmitter::new(8).unwrap();
    transmitter.append(b"hello").unwrap();
    let write = start_next(&mut transmitter);

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

fn start_next(transmitter: &mut Transmitter) -> super::tx::TxWrite {
    transmitter
        .start_write()
        .expect("test transmitter has a next write")
}
