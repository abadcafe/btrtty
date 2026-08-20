use super::lifecycle::{Lifecycle, LifecycleAction};
use super::types::MAX_RFCOMM_WRITE_COMMANDS;

#[test]
fn test_begin_shutdown_without_channel_withdraws_service() {
    let mut lifecycle = published_lifecycle();

    assert_eq!(
        lifecycle.begin_shutdown().unwrap(),
        LifecycleAction::WithdrawService
    );
    assert_eq!(
        lifecycle.service_withdrawn().unwrap(),
        LifecycleAction::StopRunLoop
    );
    lifecycle.ensure_stopped().unwrap();
}

#[test]
fn test_begin_shutdown_waits_for_channel_close_before_service_withdrawal() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();

    assert_eq!(
        lifecycle.begin_shutdown().unwrap(),
        LifecycleAction::CloseChannel
    );
    assert_eq!(
        lifecycle.channel_closed().unwrap(),
        LifecycleAction::WithdrawService
    );
}

#[test]
fn test_begin_shutdown_waits_for_write_before_channel_close() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();
    lifecycle.write_started().unwrap();

    assert_eq!(lifecycle.begin_shutdown().unwrap(), LifecycleAction::Wait);
    assert_eq!(
        lifecycle.write_completed().unwrap(),
        LifecycleAction::CloseChannel
    );
    assert_eq!(
        lifecycle.channel_closed().unwrap(),
        LifecycleAction::WithdrawService
    );
}

#[test]
fn test_begin_shutdown_waits_for_all_outstanding_writes() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();
    for _ in 0..MAX_RFCOMM_WRITE_COMMANDS {
        lifecycle.write_started().unwrap();
    }
    assert!(lifecycle.write_started().is_err());

    assert_eq!(lifecycle.begin_shutdown().unwrap(), LifecycleAction::Wait);
    for _ in 1..MAX_RFCOMM_WRITE_COMMANDS {
        assert_eq!(lifecycle.write_completed().unwrap(), LifecycleAction::Wait);
    }
    assert_eq!(
        lifecycle.write_completed().unwrap(),
        LifecycleAction::CloseChannel
    );
}

#[test]
fn test_begin_session_close_waits_for_channel_notification() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();

    assert_eq!(
        lifecycle.begin_session_close().unwrap(),
        LifecycleAction::CloseChannel
    );
    assert_eq!(
        lifecycle.channel_closed().unwrap(),
        LifecycleAction::NotifyDisconnected
    );
    lifecycle.channel_opened().unwrap();
}

#[test]
fn test_remote_channel_close_notifies_and_allows_reopen() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();

    assert_eq!(
        lifecycle.channel_closed().unwrap(),
        LifecycleAction::NotifyDisconnected
    );
    lifecycle.channel_opened().unwrap();
}

#[test]
fn test_remote_close_during_write_waits_for_write_completion() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();
    lifecycle.write_started().unwrap();

    assert_eq!(lifecycle.channel_closed().unwrap(), LifecycleAction::Wait);
    assert_eq!(
        lifecycle.write_completed().unwrap(),
        LifecycleAction::NotifyDisconnected
    );
    lifecycle.channel_opened().unwrap();
}

#[test]
fn test_remote_close_waits_for_all_outstanding_writes() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();
    for _ in 0..MAX_RFCOMM_WRITE_COMMANDS {
        lifecycle.write_started().unwrap();
    }

    assert_eq!(lifecycle.channel_closed().unwrap(), LifecycleAction::Wait);
    for _ in 1..MAX_RFCOMM_WRITE_COMMANDS {
        assert_eq!(lifecycle.write_completed().unwrap(), LifecycleAction::Wait);
    }
    assert_eq!(
        lifecycle.write_completed().unwrap(),
        LifecycleAction::NotifyDisconnected
    );
    lifecycle.channel_opened().unwrap();
}

#[test]
fn test_shutdown_remote_close_during_write_waits_for_both_barriers() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();
    lifecycle.write_started().unwrap();

    assert_eq!(lifecycle.begin_shutdown().unwrap(), LifecycleAction::Wait);
    assert_eq!(lifecycle.channel_closed().unwrap(), LifecycleAction::Wait);
    assert_eq!(
        lifecycle.write_completed().unwrap(),
        LifecycleAction::WithdrawService
    );
}

#[test]
fn test_begin_shutdown_upgrades_session_close() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();
    assert_eq!(
        lifecycle.begin_session_close().unwrap(),
        LifecycleAction::CloseChannel
    );

    assert_eq!(lifecycle.begin_shutdown().unwrap(), LifecycleAction::Wait);
    assert_eq!(
        lifecycle.channel_closed().unwrap(),
        LifecycleAction::WithdrawService
    );
}

#[test]
fn test_begin_shutdown_waits_for_known_rejected_channel() {
    let mut lifecycle = published_lifecycle();
    lifecycle.rejected_channel_opened().unwrap();

    assert_eq!(lifecycle.begin_shutdown().unwrap(), LifecycleAction::Wait);
    assert_eq!(
        lifecycle.rejected_channel_closed().unwrap(),
        LifecycleAction::WithdrawService
    );
}

#[test]
fn test_begin_shutdown_waits_for_active_and_rejected_channels() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();
    lifecycle.rejected_channel_opened().unwrap();

    assert_eq!(
        lifecycle.begin_shutdown().unwrap(),
        LifecycleAction::CloseChannel
    );
    assert_eq!(lifecycle.channel_closed().unwrap(), LifecycleAction::Wait);
    assert_eq!(
        lifecycle.rejected_channel_closed().unwrap(),
        LifecycleAction::WithdrawService
    );
}

#[test]
fn test_shutdown_tracks_rejected_channels_delivered_while_closing() {
    let mut lifecycle = published_lifecycle();
    lifecycle.channel_opened().unwrap();

    assert_eq!(
        lifecycle.begin_shutdown().unwrap(),
        LifecycleAction::CloseChannel
    );
    lifecycle.rejected_channel_opened().unwrap();
    assert_eq!(lifecycle.channel_closed().unwrap(), LifecycleAction::Wait);

    lifecycle.rejected_channel_opened().unwrap();
    assert_eq!(
        lifecycle.rejected_channel_closed().unwrap(),
        LifecycleAction::Wait
    );
    assert_eq!(
        lifecycle.rejected_channel_closed().unwrap(),
        LifecycleAction::WithdrawService
    );
}

#[test]
fn test_delivered_channel_cancels_queued_service_withdrawal() {
    let mut lifecycle = published_lifecycle();

    assert_eq!(
        lifecycle.begin_shutdown().unwrap(),
        LifecycleAction::WithdrawService
    );
    assert!(lifecycle.is_withdrawing());

    lifecycle.rejected_channel_opened().unwrap();
    assert!(!lifecycle.is_withdrawing());
    assert_eq!(
        lifecycle.rejected_channel_closed().unwrap(),
        LifecycleAction::WithdrawService
    );
    assert!(lifecycle.is_withdrawing());
}

#[test]
fn test_lifecycle_rejects_channel_before_publication_and_after_stop() {
    let mut lifecycle = Lifecycle::new();
    assert!(lifecycle.channel_opened().is_err());

    lifecycle.service_published().unwrap();
    assert_eq!(
        lifecycle.begin_shutdown().unwrap(),
        LifecycleAction::WithdrawService
    );
    assert_eq!(
        lifecycle.service_withdrawn().unwrap(),
        LifecycleAction::StopRunLoop
    );
    assert!(lifecycle.channel_opened().is_err());
}

#[test]
fn test_ensure_stopped_rejects_partial_shutdown() {
    let mut lifecycle = published_lifecycle();
    assert!(lifecycle.ensure_stopped().is_err());

    assert_eq!(
        lifecycle.begin_shutdown().unwrap(),
        LifecycleAction::WithdrawService
    );
    assert!(lifecycle.ensure_stopped().is_err());
}

fn published_lifecycle() -> Lifecycle {
    let mut lifecycle = Lifecycle::new();
    lifecycle.service_published().unwrap();
    lifecycle
}
