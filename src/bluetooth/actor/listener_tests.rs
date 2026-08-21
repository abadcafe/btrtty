use std::time::{Duration, Instant};

use super::listener::{CALLBACK_GRACE, IncomingListener};

#[test]
fn test_recent_generations_survive_for_in_flight_callbacks() {
    let started = Instant::now();
    let mut listener = IncomingListener::new(7, 1_u8, started);

    let before_grace = CALLBACK_GRACE
        .checked_sub(Duration::from_millis(1))
        .expect("one millisecond is shorter than the callback grace");
    let retired = listener.install(2, started + before_grace);

    assert!(retired.is_empty());
    assert_eq!(listener.channel_id(), 7);
    assert_eq!(listener.into_notifications().collect::<Vec<_>>(), [1, 2]);
}

#[test]
fn test_expired_generations_are_retired_but_the_newest_is_kept() {
    let started = Instant::now();
    let mut listener = IncomingListener::new(3, 1_u8, started);
    assert!(
        listener
            .install(2, started + Duration::from_secs(1))
            .is_empty()
    );

    let retired = listener.install(3, started + CALLBACK_GRACE);

    assert_eq!(retired, vec![1]);
    assert_eq!(listener.into_notifications().collect::<Vec<_>>(), [2, 3]);
}

#[test]
fn test_shutdown_returns_every_registered_generation() {
    let started = Instant::now();
    let mut listener = IncomingListener::new(1, 10_u8, started);
    assert!(
        listener
            .install(11, started + Duration::from_secs(1))
            .is_empty()
    );

    assert_eq!(listener.into_notifications().collect::<Vec<_>>(), [10, 11]);
}
