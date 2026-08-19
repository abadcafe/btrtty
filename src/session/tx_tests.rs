use super::tx::Transmitter;

#[test]
fn test_transmitter_rejects_invalid_limits() {
    assert!(Transmitter::new(0, 1024, 1).is_err());
    assert!(Transmitter::new(64, 63, 1).is_err());
    assert!(Transmitter::new(64, 1024, 0).is_err());
}

#[test]
fn test_transmitter_exposes_idle_byte_immediately() {
    let mut transmitter = Transmitter::new(64, 1024, 4).unwrap();
    transmitter.append(b"x").unwrap();

    assert_eq!(transmitter.next_write_len(), Some(1));
    let write = start_next(&mut transmitter);
    assert_eq!(&write.data[..], b"x");
}

#[test]
fn test_transmitter_coalesces_fragments_until_submission() {
    let mut transmitter = Transmitter::new(8, 32, 4).unwrap();
    transmitter.append(b"a").unwrap();
    transmitter.append(b"bc").unwrap();
    transmitter.append(b"def").unwrap();

    assert_eq!(transmitter.next_write_len(), Some(6));
    let write = start_next(&mut transmitter);
    assert_eq!(&write.data[..], b"abcdef");
}

#[test]
fn test_transmitter_packs_full_mtu_in_original_order() {
    let mut transmitter = Transmitter::new(4, 16, 4).unwrap();
    transmitter.append(b"ab").unwrap();
    transmitter.append(b"cdef").unwrap();
    transmitter.append(b"ghij").unwrap();

    let first = start_next(&mut transmitter);
    let second = start_next(&mut transmitter);
    let third = start_next(&mut transmitter);
    assert_eq!(&first.data[..], b"abcd");
    assert_eq!(&second.data[..], b"efgh");
    assert_eq!(&third.data[..], b"ij");
}

#[test]
fn test_transmitter_enforces_staging_byte_budget() {
    let mut transmitter = Transmitter::new(4, 8, 2).unwrap();
    transmitter.append(b"12345678").unwrap();

    assert_eq!(transmitter.remaining_capacity(), 0);
    assert!(transmitter.append(b"9").is_err());
    let write = start_next(&mut transmitter);
    assert_eq!(&write.data[..], b"1234");
    assert_eq!(transmitter.remaining_capacity(), 4);
}

#[test]
fn test_transmitter_enforces_derived_write_window() {
    let mut transmitter = Transmitter::new(2, 16, 3).unwrap();
    transmitter.append(b"abcdefgh").unwrap();

    let writes = (0..3)
        .map(|_| start_next(&mut transmitter))
        .collect::<Vec<_>>();
    assert!(transmitter.window_full());
    assert_eq!(transmitter.next_write_len(), None);
    assert!(transmitter.start_write(1).is_err());

    transmitter
        .complete(writes[1].token, writes[1].data.len())
        .unwrap();
    assert_eq!(&start_next(&mut transmitter).data[..], b"gh");
}

#[test]
fn test_transmitter_accepts_out_of_order_completions() {
    let mut transmitter = Transmitter::new(2, 8, 4).unwrap();
    transmitter.append(b"abcdef").unwrap();
    let first = start_next(&mut transmitter);
    let second = start_next(&mut transmitter);
    let third = start_next(&mut transmitter);

    transmitter.complete(second.token, 2).unwrap();
    transmitter.complete(first.token, 2).unwrap();
    transmitter.complete(third.token, 2).unwrap();
    assert!(transmitter.is_drained());
}

#[test]
fn test_transmitter_rejects_invalid_completion() {
    let mut transmitter = Transmitter::new(8, 8, 1).unwrap();
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

#[test]
fn test_transmitter_sends_an_admitted_prefix_without_waiting_for_the_rest() {
    let mut transmitter = Transmitter::new(8, 16, 2).unwrap();
    transmitter.append(b"abcdefgh").unwrap();

    let first = transmitter.start_write(3).unwrap();
    let second = start_next(&mut transmitter);
    assert_eq!(&first.data[..], b"abc");
    assert_eq!(&second.data[..], b"defgh");
}

fn start_next(transmitter: &mut Transmitter) -> super::tx::TxWrite {
    let length = transmitter
        .next_write_len()
        .expect("test transmitter has a next write");
    transmitter.start_write(length).unwrap()
}
