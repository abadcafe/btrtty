use std::cell::RefCell;
use std::ffi::c_void;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{NSObject, NSObjectProtocol};
use objc2_io_bluetooth::{
    IOBluetoothRFCOMMChannel, IOBluetoothRFCOMMChannelDelegate, IOBluetoothUserNotification,
    IOBluetoothUserNotificationChannelDirection,
};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::error;

use super::dispatch::{NativeAction, NativeActionQueue};
use super::sdp;
use super::types::{BtCommand, BtError, BtEvent, BtInput, ConnectionId};

mod acceptance;
#[cfg(test)]
mod acceptance_tests;
mod channel;
mod listener;
#[cfg(test)]
mod listener_tests;
mod shutdown;
mod state;
mod tx;

use listener::IncomingListener;
use state::ActorState;

pub(super) const LISTENER_REFRESH_INTERVAL: std::time::Duration = listener::REFRESH_INTERVAL;

pub(super) struct PublishedEndpoint {
    pub(super) record_handle: u32,
    pub(super) channel_id: u8,
}

pub(super) struct ActorIvars {
    events: mpsc::UnboundedSender<BtEvent>,
    input: mpsc::Sender<BtInput>,
    actions: NativeActionQueue,
    state: RefCell<ActorState>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - BluetoothActor does not implement Drop; shutdown is explicit.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ActorIvars]
    pub(super) struct BluetoothActor;

    // SAFETY: NSObjectProtocol adds no implementation invariants.
    unsafe impl NSObjectProtocol for BluetoothActor {}

    // SAFETY: Every method signature exactly matches
    // IOBluetoothRFCOMMChannelDelegate. The class is main-thread-only.
    #[allow(non_snake_case)]
    unsafe impl IOBluetoothRFCOMMChannelDelegate for BluetoothActor {
        #[unsafe(method(rfcommChannelData:data:length:))]
        unsafe fn rfcommChannelData_data_length(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
            data_pointer: *mut c_void,
            data_length: usize,
        ) {
            autoreleasepool(|_| {
                let Some(channel) = channel else {
                    return;
                };
                let Some(connection) = self.routed_connection(channel) else {
                    return;
                };
                if data_pointer.is_null() || data_length == 0 {
                    return;
                }
                // SAFETY: IOBluetooth guarantees this buffer is valid for the
                // duration of the delegate callback. It is copied immediately.
                let data = unsafe {
                    std::slice::from_raw_parts(data_pointer.cast::<u8>(), data_length)
                };
                let input = BtInput::new(connection, Bytes::copy_from_slice(data));
                match self.ivars().input.try_send(input) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_) | TrySendError::Closed(_)) => {
                        self.defer(NativeAction::InputOverflow {
                            channel_key: Self::channel_key(channel),
                        });
                    }
                }
            });
        }

        #[unsafe(method(rfcommChannelClosed:))]
        unsafe fn rfcommChannelClosed(&self, channel: Option<&IOBluetoothRFCOMMChannel>) {
            if let Some(channel) = channel {
                self.defer(NativeAction::ChannelClosed {
                    channel_key: Self::channel_key(channel),
                });
            }
        }

        #[unsafe(method(rfcommChannelFlowControlChanged:))]
        unsafe fn rfcommChannelFlowControlChanged(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
        ) {
            if let Some(channel) = channel {
                self.defer(NativeAction::FlowAvailable {
                    channel_key: Self::channel_key(channel),
                });
            }
        }

        #[unsafe(method(rfcommChannelQueueSpaceAvailable:))]
        unsafe fn rfcommChannelQueueSpaceAvailable(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
        ) {
            if let Some(channel) = channel {
                self.defer(NativeAction::FlowAvailable {
                    channel_key: Self::channel_key(channel),
                });
            }
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:))]
        unsafe fn rfcommChannelWriteComplete_refcon_status(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
            refcon: *mut c_void,
            status: i32,
        ) {
            if let Some(channel) = channel {
                self.defer(NativeAction::WriteCompleted {
                    channel_key: Self::channel_key(channel),
                    refcon: refcon as usize,
                    status,
                    bytes_written: None,
                });
            }
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:bytesWritten:))]
        unsafe fn rfcommChannelWriteComplete_refcon_status_bytesWritten(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
            refcon: *mut c_void,
            status: i32,
            bytes_written: usize,
        ) {
            if let Some(channel) = channel {
                self.defer(NativeAction::WriteCompleted {
                    channel_key: Self::channel_key(channel),
                    refcon: refcon as usize,
                    status,
                    bytes_written: Some(bytes_written),
                });
            }
        }
    }

    impl BluetoothActor {
        #[unsafe(method(newRFCOMMChannelOpened:channel:))]
        fn new_rfcomm_channel_opened(
            &self,
            notification: &IOBluetoothUserNotification,
            channel: &IOBluetoothRFCOMMChannel,
        ) {
            autoreleasepool(|_| self.stage_delivered_channel(notification, channel));
        }

        #[unsafe(method(rfcommChannelClosedNotification:channel:))]
        fn rfcomm_channel_closed_notification(
            &self,
            _notification: &IOBluetoothUserNotification,
            channel: &IOBluetoothRFCOMMChannel,
        ) {
            self.defer(NativeAction::ChannelClosed {
                channel_key: Self::channel_key(channel),
            });
        }
    }
);

impl BluetoothActor {
    pub(super) fn new(
        events: mpsc::UnboundedSender<BtEvent>,
        input: mpsc::Sender<BtInput>,
        actions: NativeActionQueue,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ActorIvars {
            events,
            input,
            actions,
            state: RefCell::new(ActorState::default()),
        });
        // SAFETY: This is NSObject's designated initialization path and all
        // Rust ivars were installed before calling `init`.
        unsafe { msg_send![super(this), init] }
    }

    pub(super) fn publish(&self) -> Result<PublishedEndpoint> {
        let service = sdp::publish()?;
        let notification = self.register_incoming_notification(service.channel_id);
        let Some(notification) = notification else {
            // SAFETY: This process owns the freshly published record.
            let rollback_status = unsafe { service.record.removeServiceRecord() };
            if rollback_status != 0 {
                bail!(
                    "register incoming RFCOMM channel notification; SDP rollback failed with IOReturn {rollback_status:#x}"
                );
            }
            bail!("register incoming RFCOMM channel notification");
        };

        let mut state = self.ivars().state.borrow_mut();
        if let Err(error) = state.lifecycle.service_published() {
            drop(state);
            // SAFETY: These resources were not installed in actor state.
            unsafe { notification.unregister() };
            // SAFETY: This process owns the freshly published record.
            let rollback_status = unsafe { service.record.removeServiceRecord() };
            if rollback_status != 0 {
                bail!("{error}; SDP rollback failed with IOReturn {rollback_status:#x}");
            }
            return Err(error);
        }
        state.service_record = Some(service.record);
        state.incoming_listener = Some(IncomingListener::new(
            service.channel_id,
            notification,
            Instant::now(),
        ));
        Ok(PublishedEndpoint {
            record_handle: service.record_handle,
            channel_id: service.channel_id,
        })
    }

    pub(super) fn register_incoming_notification(
        &self,
        channel_id: u8,
    ) -> Option<Retained<IOBluetoothUserNotification>> {
        let object: &AnyObject = self;
        // SAFETY: The selector is implemented above and the actor remains
        // retained until the returned notification is explicitly unregistered.
        unsafe {
            IOBluetoothRFCOMMChannel::registerForChannelOpenNotifications_selector_withChannelID_direction(
                Some(object),
                Some(sel!(newRFCOMMChannelOpened:channel:)),
                channel_id,
                IOBluetoothUserNotificationChannelDirection::Incoming,
            )
        }
    }

    /// Whether the native compatibility timer should schedule another public
    /// RFCOMM notification registration.
    pub(super) fn listener_refresh_due(&self) -> bool {
        let state = self.ivars().state.borrow();
        state.fatal_error.is_none()
            && state.incoming_listener.is_some()
            && state.acceptance.is_armed()
            && state.active.is_none()
            && state.delivered.is_empty()
            && state.rejected.is_empty()
    }

    /// Re-announce readiness to bluetoothd while no connection is active.
    ///
    /// macOS 15 can discard the daemon-side accept session created by an
    /// otherwise live `IOBluetoothUserNotification`. Repeating the public
    /// registration while waiting lets the next generation consume an
    /// incoming request that bluetoothd has parked in the meantime.
    pub(super) fn refresh_incoming_listener(&self) {
        let channel_id = {
            let state = self.ivars().state.borrow();
            if !self.listener_refresh_due() {
                return;
            }
            state
                .incoming_listener
                .as_ref()
                .expect("listener refresh eligibility was checked")
                .channel_id()
        };

        let Some(notification) = self.register_incoming_notification(channel_id) else {
            self.emit_fatal("refresh incoming RFCOMM channel notification", -1);
            return;
        };

        let retired = {
            let mut state = self.ivars().state.borrow_mut();
            let still_waiting = state.fatal_error.is_none()
                && state.acceptance.is_armed()
                && state.active.is_none()
                && state.delivered.is_empty()
                && state.rejected.is_empty();
            if !still_waiting {
                drop(state);
                // SAFETY: This generation was never installed in actor state.
                unsafe { notification.unregister() };
                return;
            }
            let Some(listener) = state.incoming_listener.as_mut() else {
                drop(state);
                // SAFETY: Shutdown removed the listener while registration was
                // in progress; this returned generation remains actor-owned.
                unsafe { notification.unregister() };
                return;
            };
            listener.install(notification, Instant::now())
        };

        for notification in retired {
            // SAFETY: The callback grace period elapsed and a newer generation
            // is already retained by the main-thread actor.
            unsafe { notification.unregister() };
        }
    }

    pub(super) fn handle(&self, command: BtCommand) {
        match command {
            BtCommand::AcceptNext { after } => self.accept_next(after),
            BtCommand::Send {
                connection,
                token,
                data,
            } => self.send(connection, token, data),
            BtCommand::Close { connection } => self.begin_session_close(connection),
            BtCommand::Shutdown => self.begin_shutdown(),
        }
    }

    pub(super) fn handle_native(&self, action: NativeAction) {
        match action {
            NativeAction::OpenDelivered { channel_key } => {
                self.open_delivered_channel(channel_key);
            }
            NativeAction::ChannelClosed { channel_key } => self.channel_closed(channel_key),
            NativeAction::WriteCompleted {
                channel_key,
                refcon,
                status,
                bytes_written,
            } => self.write_completed(channel_key, refcon, status, bytes_written),
            NativeAction::FlowAvailable { channel_key } => {
                self.flow_available(channel_key);
            }
            NativeAction::InputOverflow { channel_key } => self.input_overflow(channel_key),
            NativeAction::WithdrawService => self.withdraw_service(),
        }
    }

    pub(super) fn finish(&self) -> Result<()> {
        let state = self.ivars().state.borrow();
        state.lifecycle.ensure_stopped()?;
        if state.incoming_listener.is_some() {
            bail!("Bluetooth shutdown ended with an incoming listener registered");
        }
        if state.service_record.is_some() {
            bail!("Bluetooth shutdown ended with the SDP service published");
        }
        if !state.delivered.is_empty() {
            bail!("Bluetooth shutdown ended with delivered channels pending dispatch");
        }
        if state.active.is_some() {
            bail!("Bluetooth shutdown ended before RFCOMM channel-close notification");
        }
        if !state.rejected.is_empty() {
            bail!("Bluetooth shutdown ended with rejected RFCOMM channels still closing");
        }
        if !state.queued_writes.is_empty()
            || !state.pending_writes.is_empty()
            || !state.retired_writes.is_empty()
        {
            bail!("Bluetooth shutdown ended with RFCOMM writes outstanding");
        }
        if !state.acceptance.is_stopped() {
            bail!("Bluetooth shutdown ended with its acceptance gate open");
        }
        if let Some(error) = &state.fatal_error {
            return Err(anyhow!(error.clone()));
        }
        Ok(())
    }

    pub(super) fn dispatch_failed(&self, operation: &'static str) {
        self.emit_fatal(operation, -1);
        Self::stop_run_loop();
    }

    fn emit_connection_error(
        &self,
        connection: ConnectionId,
        operation: &'static str,
        status: i32,
    ) {
        let _ = self.ivars().events.send(BtEvent::ConnectionError {
            connection,
            error: BtError::new(operation, status),
        });
    }

    fn emit_fatal(&self, operation: &'static str, status: i32) {
        let error = BtError::new(operation, status);
        self.ivars()
            .state
            .borrow_mut()
            .fatal_error
            .get_or_insert_with(|| error.clone());
        error!(%error, "fatal native Bluetooth error");
        let _ = self.ivars().events.send(BtEvent::Fatal(error));
    }

    fn defer(&self, action: NativeAction) {
        if self.ivars().actions.submit(action).is_err() {
            self.emit_fatal("enqueue deferred Bluetooth action", -1);
            Self::stop_run_loop();
        }
    }
}
