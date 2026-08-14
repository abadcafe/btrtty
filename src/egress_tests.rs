use std::time::Duration;

use tokio::time::Instant;

use crate::egress::{EgressLimiter, OutputRate};

const PTY_CHUNK_SIZE: usize = 256;

#[test]
fn test_output_rate_accepts_supported_bounds() {
    assert_eq!(OutputRate::new(1).unwrap().kib_per_second(), 1);
    assert_eq!(OutputRate::new(100).unwrap().kib_per_second(), 100);
}

#[test]
fn test_output_rate_rejects_values_outside_supported_range() {
    assert!(OutputRate::new(0).is_err());
    assert!(OutputRate::new(101).is_err());
}

#[test]
fn test_reserve_sends_first_allocation_immediately_then_paces_output() {
    let mut limiter = EgressLimiter::new(OutputRate::new(2).unwrap());
    let now = Instant::now();

    assert_eq!(limiter.reserve_at(2 * 1024, now).unwrap(), now);
    assert_eq!(
        limiter.reserve_at(2 * 1024, now).unwrap(),
        now + Duration::from_secs(1)
    );
}

#[test]
fn test_reserve_serializes_future_allocations() {
    let mut limiter = EgressLimiter::new(OutputRate::new(2).unwrap());
    let now = Instant::now();

    assert_eq!(limiter.reserve_at(2 * 1024, now).unwrap(), now);
    assert_eq!(
        limiter.reserve_at(2 * 1024, now).unwrap(),
        now + Duration::from_secs(1)
    );
    assert_eq!(
        limiter.reserve_at(2 * 1024, now).unwrap(),
        now + Duration::from_secs(2)
    );
}

#[test]
fn test_reserve_does_not_accumulate_idle_burst() {
    let mut limiter = EgressLimiter::new(OutputRate::new(2).unwrap());
    let now = Instant::now();

    assert_eq!(limiter.reserve_at(2 * 1024, now).unwrap(), now);
    let after_idle = now + Duration::from_secs(10);
    assert_eq!(
        limiter.reserve_at(2 * 1024, after_idle).unwrap(),
        after_idle
    );
    assert_eq!(
        limiter.reserve_at(2 * 1024, after_idle).unwrap(),
        after_idle + Duration::from_secs(1)
    );
}

#[test]
fn test_queue_capacity_tracks_configured_rate() {
    assert_eq!(
        EgressLimiter::new(OutputRate::new(1).unwrap())
            .queue_capacity(PTY_CHUNK_SIZE)
            .unwrap(),
        1
    );
    assert_eq!(
        EgressLimiter::new(OutputRate::new(20).unwrap())
            .queue_capacity(PTY_CHUNK_SIZE)
            .unwrap(),
        8
    );
    assert_eq!(
        EgressLimiter::new(OutputRate::new(100).unwrap())
            .queue_capacity(PTY_CHUNK_SIZE)
            .unwrap(),
        40
    );
}

#[test]
fn test_reserve_rejects_empty_allocation() {
    let mut limiter = EgressLimiter::new(OutputRate::new(20).unwrap());

    assert!(limiter.reserve_at(0, Instant::now()).is_err());
}
