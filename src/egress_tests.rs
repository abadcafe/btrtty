use std::time::Duration;

use tokio::time::Instant;

use crate::egress::{EgressLimiter, OutputRate};

#[test]
fn test_output_rate_accepts_supported_bounds() {
    assert_eq!(OutputRate::new(1).unwrap().kib_per_second(), 1);
    assert_eq!(OutputRate::new(200).unwrap().kib_per_second(), 200);
}

#[test]
fn test_output_rate_rejects_values_outside_supported_range() {
    assert!(OutputRate::new(0).is_err());
    assert!(OutputRate::new(201).is_err());
}

#[test]
fn test_staging_budget_tracks_one_hundred_milliseconds_of_rate() {
    assert_eq!(
        EgressLimiter::new(OutputRate::new(1).unwrap())
            .staging_budget(667)
            .unwrap(),
        667
    );
    assert_eq!(
        EgressLimiter::new(OutputRate::new(20).unwrap())
            .staging_budget(667)
            .unwrap(),
        2048
    );
    assert_eq!(
        EgressLimiter::new(OutputRate::new(200).unwrap())
            .staging_budget(667)
            .unwrap(),
        20_480
    );
}

#[test]
fn test_write_window_tracks_twenty_five_milliseconds_of_rate() {
    let limiter = EgressLimiter::new(OutputRate::new(200).unwrap());
    assert_eq!(limiter.write_window(667, 32).unwrap(), 8);
    assert_eq!(limiter.write_window(127, 32).unwrap(), 32);

    let slow = EgressLimiter::new(OutputRate::new(20).unwrap());
    assert_eq!(slow.write_window(667, 32).unwrap(), 1);
}

#[test]
fn test_write_window_rejects_invalid_limits() {
    let limiter = EgressLimiter::new(OutputRate::new(200).unwrap());
    assert!(limiter.write_window(0, 32).is_err());
    assert!(limiter.write_window(667, 0).is_err());
}

#[test]
fn test_record_sent_paces_from_actual_submission_time() {
    let mut limiter = EgressLimiter::new(OutputRate::new(2).unwrap());
    let now = Instant::now();
    assert_eq!(limiter.ready_at(now), now);
    limiter.record_sent_at(2 * 1024, now).unwrap();
    assert_eq!(limiter.ready_at(now), now + Duration::from_secs(1));

    let late = now + Duration::from_millis(1100);
    limiter.record_sent_at(2 * 1024, late).unwrap();
    assert_eq!(limiter.ready_at(late), late + Duration::from_secs(1));
}

#[test]
fn test_limiter_does_not_accumulate_idle_burst() {
    let mut limiter = EgressLimiter::new(OutputRate::new(2).unwrap());
    let now = Instant::now();
    limiter.record_sent_at(2 * 1024, now).unwrap();

    let after_idle = now + Duration::from_secs(10);
    assert_eq!(limiter.ready_at(after_idle), after_idle);
    limiter.record_sent_at(2 * 1024, after_idle).unwrap();
    assert_eq!(
        limiter.ready_at(after_idle),
        after_idle + Duration::from_secs(1)
    );
}

#[test]
fn test_limiter_rejects_early_or_empty_submission() {
    let mut limiter = EgressLimiter::new(OutputRate::new(2).unwrap());
    let now = Instant::now();
    assert!(limiter.record_sent_at(0, now).is_err());
    limiter.record_sent_at(1024, now).unwrap();
    assert!(
        limiter
            .record_sent_at(1024, now + Duration::from_millis(1))
            .is_err()
    );
}
