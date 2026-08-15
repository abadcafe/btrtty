use super::close_barrier::{CloseAction, CloseBarrier, CloseCompletion, CloseRequest};

#[test]
fn test_begin_close_waits_for_channel_notification() {
    let mut barrier = CloseBarrier::new();
    barrier.channel_opened().unwrap();

    assert_eq!(
        barrier.begin_close(CloseRequest::Session),
        CloseAction::CloseChannel
    );
    assert_eq!(barrier.channel_closed().unwrap(), CloseCompletion::Session);
}

#[test]
fn test_begin_close_waits_for_outstanding_write_before_channel_close() {
    let mut barrier = CloseBarrier::new();
    barrier.channel_opened().unwrap();
    barrier.write_started().unwrap();

    assert_eq!(
        barrier.begin_close(CloseRequest::Shutdown),
        CloseAction::Wait
    );
    assert_eq!(
        barrier.write_completed().unwrap(),
        CloseAction::CloseChannel
    );
    assert_eq!(barrier.channel_closed().unwrap(), CloseCompletion::Shutdown);
}

#[test]
fn test_begin_shutdown_without_channel_completes_immediately() {
    let mut barrier = CloseBarrier::new();

    assert_eq!(
        barrier.begin_close(CloseRequest::Shutdown),
        CloseAction::Complete(CloseCompletion::Shutdown)
    );
}

#[test]
fn test_begin_shutdown_upgrades_session_close_in_progress() {
    let mut barrier = CloseBarrier::new();
    barrier.channel_opened().unwrap();
    assert_eq!(
        barrier.begin_close(CloseRequest::Session),
        CloseAction::CloseChannel
    );

    assert_eq!(
        barrier.begin_close(CloseRequest::Shutdown),
        CloseAction::Wait
    );
    assert_eq!(barrier.channel_closed().unwrap(), CloseCompletion::Shutdown);
}

#[test]
fn test_remote_channel_close_completes_disconnect_and_allows_reopen() {
    let mut barrier = CloseBarrier::new();
    barrier.channel_opened().unwrap();

    assert_eq!(barrier.channel_closed().unwrap(), CloseCompletion::Remote);
    barrier.channel_opened().unwrap();
}
