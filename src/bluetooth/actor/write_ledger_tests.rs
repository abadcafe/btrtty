use bytes::Bytes;

use super::write_ledger::{Completion, PendingWrite, WriteLedger};
use crate::bluetooth::types::{ConnectionId, MAX_RFCOMM_WRITE_COMMANDS, WriteToken};

#[test]
fn test_admit_rejects_duplicate_queued_token() {
    let mut ledger = WriteLedger::new();
    let queued = ledger.admit(write(1)).unwrap();
    ledger.queue(queued);

    assert!(ledger.admit(write(1)).is_err());
    assert_eq!(ledger.len(), 1);
}

#[test]
fn test_admit_rejects_duplicate_pending_token_without_replacing_buffer() {
    let mut ledger = WriteLedger::new();
    let pending = ledger.admit(write_with_data(1, b"first")).unwrap();
    ledger.start(pending);

    assert!(ledger.admit(write_with_data(1, b"replacement")).is_err());
    let Completion::Current(completed) = ledger.complete(token(1)) else {
        panic!("the original pending buffer must remain owned");
    };
    assert_eq!(completed.data(), &Bytes::from_static(b"first"));
}

#[test]
fn test_admit_rejects_retired_token_after_unbounded_completion_history() {
    let mut ledger = WriteLedger::new();
    for value in 1..=10_000 {
        let pending = ledger.admit(write(value)).unwrap();
        ledger.start(pending);
        assert!(matches!(
            ledger.complete(token(value)),
            Completion::Current(_)
        ));
        assert!(ledger.is_empty());
    }

    assert!(ledger.admit(write(1)).is_err());
    assert!(matches!(ledger.complete(token(1)), Completion::Retired));
}

#[test]
fn test_complete_accepts_out_of_order_pending_writes() {
    let mut ledger = WriteLedger::new();
    for value in 1..=3 {
        let admitted = ledger.admit(write(value)).unwrap();
        ledger.start(admitted);
    }

    assert!(matches!(ledger.complete(token(2)), Completion::Current(_)));
    assert!(matches!(ledger.complete(token(1)), Completion::Current(_)));
    assert!(matches!(ledger.complete(token(3)), Completion::Current(_)));
    assert!(ledger.is_empty());
}

#[test]
fn test_complete_distinguishes_retired_and_unknown_tokens() {
    let mut ledger = WriteLedger::new();
    let admitted = ledger.admit(write(5)).unwrap();
    ledger.start(admitted);
    assert!(matches!(ledger.complete(token(5)), Completion::Current(_)));

    assert!(matches!(ledger.complete(token(5)), Completion::Retired));
    assert!(matches!(ledger.complete(token(6)), Completion::Unknown));
}

#[test]
fn test_complete_rejects_a_token_that_was_never_submitted_natively() {
    let mut ledger = WriteLedger::new();
    let queued = ledger.admit(write(1)).unwrap();
    ledger.queue(queued);

    assert!(matches!(ledger.complete(token(1)), Completion::Queued));
    assert_eq!(ledger.len(), 1);
}

#[test]
fn test_clear_queued_preserves_native_pending_writes() {
    let mut ledger = WriteLedger::new();
    let pending = ledger.admit(write(1)).unwrap();
    ledger.start(pending);
    for value in 2..=4 {
        let queued = ledger.admit(write(value)).unwrap();
        ledger.queue(queued);
    }

    ledger.clear_queued();

    assert_eq!(ledger.len(), 1);
    assert!(matches!(ledger.complete(token(1)), Completion::Current(_)));
}

#[test]
fn test_admit_enforces_bounded_native_window() {
    let mut ledger = WriteLedger::new();
    for value in 1..=u64::try_from(MAX_RFCOMM_WRITE_COMMANDS).unwrap() {
        let queued = ledger.admit(write(value)).unwrap();
        ledger.queue(queued);
    }

    assert!(
        ledger
            .admit(write(u64::try_from(MAX_RFCOMM_WRITE_COMMANDS).unwrap() + 1))
            .is_err()
    );
}

fn write(value: u64) -> PendingWrite {
    write_with_data(value, b"x")
}

fn write_with_data(value: u64, data: &'static [u8]) -> PendingWrite {
    PendingWrite::new(connection(), token(value), Bytes::from_static(data))
}

fn connection() -> ConnectionId {
    ConnectionId::new(1).unwrap()
}

fn token(value: u64) -> WriteToken {
    WriteToken::new(value).unwrap()
}
