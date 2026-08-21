use super::acceptance::AcceptanceGate;
use crate::bluetooth::types::ConnectionId;

#[test]
fn test_initial_acceptance_is_armed_before_publication() {
    let mut gate = AcceptanceGate::new();
    assert!(gate.begin_connection());
    assert!(!gate.begin_connection());
}

#[test]
fn test_reconnect_requires_exact_closed_connection() {
    let mut gate = busy_gate();
    gate.connection_closed(connection(7)).unwrap();

    assert!(!gate.arm(connection(6)));
    assert!(!gate.begin_connection());
    assert!(gate.is_awaiting(connection(7)));
    assert!(gate.arm(connection(7)));
    assert!(gate.begin_connection());
}

#[test]
fn test_duplicate_reconnect_permit_cannot_open_two_connections() {
    let mut gate = busy_gate();
    gate.connection_closed(connection(8)).unwrap();
    assert!(gate.arm(connection(8)));
    assert!(!gate.arm(connection(8)));

    assert!(gate.begin_connection());
    assert!(!gate.begin_connection());
}

#[test]
fn test_shutdown_permanently_closes_gate() {
    let mut gate = AcceptanceGate::new();
    gate.stop();
    assert!(!gate.arm(connection(1)));

    assert!(gate.is_stopped());
    assert!(!gate.begin_connection());
}

fn busy_gate() -> AcceptanceGate {
    let mut gate = AcceptanceGate::new();
    assert!(gate.begin_connection());
    gate
}

fn connection(value: u64) -> ConnectionId {
    ConnectionId::new(value).unwrap()
}
