use std::time::Duration;

use tokio::time::Instant;

use crate::egress::{EgressDecision, EgressLimiter, OutputRate};

const NANOS_PER_SECOND: u128 = 1_000_000_000;

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
        limiter(1).staging_budget(667).unwrap(),
        667,
        "one MTU is the minimum staging budget"
    );
    assert_eq!(limiter(20).staging_budget(667).unwrap(), 2048);
    assert_eq!(limiter(200).staging_budget(667).unwrap(), 20_480);
}

#[test]
fn test_write_window_tracks_twenty_five_milliseconds_of_rate() {
    assert_eq!(limiter(200).write_window(667, 32).unwrap(), 26);
    assert_eq!(limiter(200).write_window(127, 32).unwrap(), 32);
    assert_eq!(limiter(20).write_window(667, 32).unwrap(), 26);
    assert_eq!(limiter(1).write_window(667, 32).unwrap(), 26);
}

#[test]
fn test_write_window_rejects_invalid_limits() {
    assert!(limiter(200).write_window(0, 32).is_err());
    assert!(limiter(200).write_window(667, 0).is_err());
}

#[test]
fn test_one_mtu_is_immediate_and_the_next_byte_waits() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    limiter.configure_mtu(128, now).unwrap();

    assert_eq!(
        commit_send(limiter.reserve_up_to(128, now).unwrap(), now),
        128
    );
    assert_eq!(
        wait_until(limiter.reserve_up_to(128, now).unwrap()),
        now + Duration::from_nanos(976_563)
    );
}

#[test]
fn test_available_prefix_sends_without_waiting_for_a_full_packet() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    limiter.configure_mtu(128, now).unwrap();
    assert_eq!(
        commit_send(limiter.reserve_up_to(28, now).unwrap(), now),
        28
    );

    assert_eq!(
        commit_send(limiter.reserve_up_to(128, now).unwrap(), now),
        100
    );
    assert!(matches!(
        limiter.reserve_up_to(1, now).unwrap(),
        EgressDecision::WaitUntil(_)
    ));
}

#[test]
fn test_fragmentation_consumes_the_same_mtu_credit() {
    let now = Instant::now();
    let mut whole = limiter(1);
    let mut fragmented = limiter(1);
    whole.configure_mtu(128, now).unwrap();
    fragmented.configure_mtu(128, now).unwrap();

    assert_eq!(
        commit_send(whole.reserve_up_to(128, now).unwrap(), now),
        128
    );
    for bytes in [40, 8, 79, 1] {
        assert_eq!(
            commit_send(fragmented.reserve_up_to(bytes, now).unwrap(), now),
            bytes
        );
    }

    assert_eq!(
        wait_until(whole.reserve_up_to(1, now).unwrap()),
        wait_until(fragmented.reserve_up_to(1, now).unwrap())
    );
}

#[test]
fn test_timer_lateness_becomes_bounded_whole_byte_credit() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    limiter.configure_mtu(128, now).unwrap();
    assert_eq!(
        commit_send(limiter.reserve_up_to(128, now).unwrap(), now),
        128
    );

    let late = now + Duration::from_millis(100);
    assert_eq!(
        commit_send(limiter.reserve_up_to(128, late).unwrap(), late),
        102
    );
    assert_eq!(
        wait_until(limiter.reserve_up_to(128, late).unwrap()),
        now + Duration::from_nanos(100_585_938)
    );
}

#[test]
fn test_idle_credit_is_capped_to_one_mtu() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    limiter.configure_mtu(128, now).unwrap();
    assert_eq!(
        commit_send(limiter.reserve_up_to(128, now).unwrap(), now),
        128
    );

    let much_later = now + Duration::from_secs(10);
    limiter.begin_burst(much_later).unwrap();
    assert_eq!(
        commit_send(limiter.reserve_up_to(128, much_later).unwrap(), much_later),
        128
    );
    assert_eq!(
        wait_until(limiter.reserve_up_to(128, much_later).unwrap()),
        much_later + Duration::from_nanos(976_563)
    );
}

#[test]
fn test_repeated_deadline_queries_do_not_drift() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    limiter.configure_mtu(128, now).unwrap();
    assert_eq!(
        commit_send(limiter.reserve_up_to(128, now).unwrap(), now),
        128
    );

    let first = wait_until(limiter.reserve_up_to(64, now).unwrap());
    let half_byte_later = now + Duration::from_nanos(488_281);
    let second = wait_until(limiter.reserve_up_to(64, half_byte_later).unwrap());
    assert_eq!(first, second);
}

#[test]
fn test_reconfigure_preserves_credit_instead_of_granting_a_new_burst() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    limiter.configure_mtu(128, now).unwrap();
    assert_eq!(
        commit_send(limiter.reserve_up_to(128, now).unwrap(), now),
        128
    );

    limiter.configure_mtu(256, now).unwrap();
    assert_eq!(
        wait_until(limiter.reserve_up_to(1, now).unwrap()),
        now + Duration::from_nanos(976_563)
    );

    let refilled = now + Duration::from_millis(125);
    limiter.configure_mtu(256, refilled).unwrap();
    assert_eq!(
        commit_send(limiter.reserve_up_to(256, refilled).unwrap(), refilled),
        128
    );
    assert!(matches!(
        limiter.reserve_up_to(1, refilled).unwrap(),
        EgressDecision::WaitUntil(_)
    ));
}

#[test]
fn test_smaller_mtu_clamps_existing_credit() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    limiter.configure_mtu(256, now).unwrap();
    limiter.configure_mtu(128, now).unwrap();

    assert_eq!(
        commit_send(limiter.reserve_up_to(128, now).unwrap(), now),
        128
    );
    assert!(matches!(
        limiter.reserve_up_to(1, now).unwrap(),
        EgressDecision::WaitUntil(_)
    ));
}

#[test]
fn test_dropped_permit_does_not_charge_a_failed_submission() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    limiter.configure_mtu(128, now).unwrap();

    {
        let permit = send_permit(limiter.reserve_up_to(128, now).unwrap());
        assert_eq!(permit.byte_count(), 128);
    }

    assert_eq!(
        commit_send(limiter.reserve_up_to(128, now).unwrap(), now),
        128
    );
}

#[test]
fn test_permit_charges_at_successful_submission_time() {
    let prepared_at = Instant::now();
    let submitted_at = prepared_at + Duration::from_millis(100);
    let mut limiter = limiter(1);
    limiter.configure_mtu(128, prepared_at).unwrap();

    let permit = send_permit(limiter.reserve_up_to(128, prepared_at).unwrap());
    permit.commit(submitted_at);

    assert_eq!(
        commit_send(
            limiter.reserve_up_to(128, submitted_at).unwrap(),
            submitted_at
        ),
        1
    );
    assert_eq!(
        wait_until(limiter.reserve_up_to(1, submitted_at).unwrap()),
        submitted_at + Duration::from_nanos(953_125)
    );
}

#[test]
fn test_one_millisecond_quantization_does_not_accumulate_for_small_fragments() {
    assert_quantized_transfer_tracks_theoretical_rate(149, 667);
}

#[test]
fn test_one_millisecond_quantization_does_not_accumulate_for_full_mtu_input() {
    assert_quantized_transfer_tracks_theoretical_rate(667, 667);
}

#[test]
fn test_one_millisecond_quantization_does_not_cap_rate_when_mtu_is_small() {
    assert_quantized_transfer_tracks_theoretical_rate(127, 127);
}

#[test]
fn test_limiter_rejects_invalid_configuration_or_packet_limit() {
    let now = Instant::now();
    let mut limiter = limiter(1);
    assert!(limiter.configure_mtu(0, now).is_err());
    assert!(limiter.reserve_up_to(1, now).is_err());

    limiter.configure_mtu(128, now).unwrap();
    assert!(limiter.reserve_up_to(0, now).is_err());
    assert!(limiter.reserve_up_to(129, now).is_err());
}

fn assert_quantized_transfer_tracks_theoretical_rate(maximum_packet: usize, mtu: usize) {
    let started = Instant::now();
    let mut now = started;
    let mut limiter = limiter(200);
    limiter.configure_mtu(mtu, now).unwrap();
    let total = 128 * 1024;
    let mut sent = 0;

    while sent < total {
        let maximum = (total - sent).min(maximum_packet);
        match limiter.reserve_up_to(maximum, now).unwrap() {
            EgressDecision::Send(permit) => {
                let byte_count = permit.byte_count();
                permit.commit(now);
                sent += byte_count;

                let elapsed_nanos = now.duration_since(started).as_nanos();
                let allowed_credit = u128::try_from(mtu).unwrap() * NANOS_PER_SECOND
                    + elapsed_nanos * u128::from(200_u16) * 1024;
                assert!(u128::try_from(sent).unwrap() * NANOS_PER_SECOND <= allowed_credit);
            }
            EgressDecision::WaitUntil(deadline) => {
                let elapsed_nanos = deadline.duration_since(started).as_nanos();
                let rounded_millis = elapsed_nanos.div_ceil(1_000_000);
                now = started + Duration::from_millis(u64::try_from(rounded_millis).unwrap());
            }
        }
    }

    let theoretical_nanos = u64::try_from(total - mtu)
        .unwrap()
        .checked_mul(1_000_000_000)
        .unwrap()
        .div_ceil(200 * 1024);
    let theoretical = Duration::from_nanos(theoretical_nanos);
    let actual = now.duration_since(started);
    assert!(actual >= theoretical);
    assert!(actual <= theoretical + Duration::from_millis(2));
}

fn limiter(kib_per_second: u16) -> EgressLimiter {
    EgressLimiter::new(OutputRate::new(kib_per_second).unwrap())
}

fn commit_send(decision: EgressDecision<'_>, submitted_at: Instant) -> usize {
    let permit = send_permit(decision);
    let byte_count = permit.byte_count();
    permit.commit(submitted_at);
    byte_count
}

fn send_permit(decision: EgressDecision<'_>) -> crate::egress::EgressPermit<'_> {
    match decision {
        EgressDecision::Send(permit) => permit,
        EgressDecision::WaitUntil(deadline) => panic!("unexpected wait until {deadline:?}"),
    }
}

fn wait_until(decision: EgressDecision<'_>) -> Instant {
    match decision {
        EgressDecision::WaitUntil(deadline) => deadline,
        EgressDecision::Send(permit) => {
            panic!("unexpected {}-byte send permit", permit.byte_count())
        }
    }
}
