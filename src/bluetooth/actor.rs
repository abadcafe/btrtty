use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_core_foundation::CFRunLoop;
use objc2_foundation::{NSObject, NSObjectProtocol};
use objc2_io_bluetooth::{
    IOBluetoothRFCOMMChannel, IOBluetoothRFCOMMChannelDelegate, IOBluetoothSDPServiceRecord,
    IOBluetoothUserNotification, IOBluetoothUserNotificationChannelDirection,
};
use tokio::sync::mpsc;

use super::lifecycle::{Lifecycle, LifecycleAction};
use super::sdp;
use super::types::{BtCommand, BtError, BtEvent, DeviceAddress, WriteToken};

struct PendingWrite {
    token: WriteToken,
    data: Bytes,
}

struct ActiveChannel {
    channel: Retained<IOBluetoothRFCOMMChannel>,
    close_notification: Option<Retained<IOBluetoothUserNotification>>,
}

struct RejectedChannel {
    channel: Retained<IOBluetoothRFCOMMChannel>,
    close_notification: Retained<IOBluetoothUserNotification>,
}

pub(super) struct PublishedEndpoint {
    pub(super) record_handle: u32,
    pub(super) channel_id: u8,
}

struct ActorState {
    service_record: Option<Retained<IOBluetoothSDPServiceRecord>>,
    incoming_notification: Option<Retained<IOBluetoothUserNotification>>,
    active: Option<ActiveChannel>,
    rejected: HashMap<usize, RejectedChannel>,
    pending_writes: HashMap<WriteToken, PendingWrite>,
    lifecycle: Lifecycle,
    fatal_error: Option<BtError>,
}

impl Default for ActorState {
    fn default() -> Self {
        Self {
            service_record: None,
            incoming_notification: None,
            active: None,
            rejected: HashMap::new(),
            pending_writes: HashMap::new(),
            lifecycle: Lifecycle::new(),
            fatal_error: None,
        }
    }
}

pub(super) struct ActorIvars {
    events: mpsc::UnboundedSender<BtEvent>,
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
                if !self.is_active_channel(channel) || data_pointer.is_null() || data_length == 0 {
                    return;
                }
                // SAFETY: IOBluetooth guarantees this buffer is valid for the
                // duration of the delegate callback. It is copied immediately.
                let data = unsafe {
                    std::slice::from_raw_parts(data_pointer.cast::<u8>(), data_length)
                };
                let _ = self
                    .ivars()
                    .events
                    .send(BtEvent::Data(Bytes::copy_from_slice(data)));
            });
        }

        #[unsafe(method(rfcommChannelClosed:))]
        unsafe fn rfcommChannelClosed(&self, channel: Option<&IOBluetoothRFCOMMChannel>) {
            autoreleasepool(|_| self.channel_closed(channel));
        }

        #[unsafe(method(rfcommChannelFlowControlChanged:))]
        unsafe fn rfcommChannelFlowControlChanged(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
        ) {
            if self.is_active_channel(channel) {
                let _ = self.ivars().events.send(BtEvent::FlowControlChanged);
            }
        }

        #[unsafe(method(rfcommChannelQueueSpaceAvailable:))]
        unsafe fn rfcommChannelQueueSpaceAvailable(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
        ) {
            if self.is_active_channel(channel) {
                let _ = self.ivars().events.send(BtEvent::FlowControlChanged);
            }
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:))]
        unsafe fn rfcommChannelWriteComplete_refcon_status(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
            refcon: *mut c_void,
            status: i32,
        ) {
            autoreleasepool(|_| {
                if self.is_active_channel(channel) {
                    self.write_completed(refcon, status, None);
                }
            });
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:bytesWritten:))]
        unsafe fn rfcommChannelWriteComplete_refcon_status_bytesWritten(
            &self,
            channel: Option<&IOBluetoothRFCOMMChannel>,
            refcon: *mut c_void,
            status: i32,
            bytes_written: usize,
        ) {
            autoreleasepool(|_| {
                if self.is_active_channel(channel) {
                    self.write_completed(refcon, status, Some(bytes_written));
                }
            });
        }
    }

    impl BluetoothActor {
        #[unsafe(method(newRFCOMMChannelOpened:channel:))]
        fn new_rfcomm_channel_opened(
            &self,
            notification: &IOBluetoothUserNotification,
            channel: &IOBluetoothRFCOMMChannel,
        ) {
            autoreleasepool(|_| self.channel_opened(notification, channel));
        }

        #[unsafe(method(rfcommChannelClosedNotification:channel:))]
        fn rfcomm_channel_closed_notification(
            &self,
            _notification: &IOBluetoothUserNotification,
            channel: &IOBluetoothRFCOMMChannel,
        ) {
            autoreleasepool(|_| self.channel_closed(Some(channel)));
        }
    }
);

impl BluetoothActor {
    pub(super) fn new(
        events: mpsc::UnboundedSender<BtEvent>,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ActorIvars {
            events,
            state: RefCell::new(ActorState::default()),
        });
        // SAFETY: This is NSObject's designated initialization path and all
        // Rust ivars were installed before calling `init`.
        unsafe { msg_send![super(this), init] }
    }

    pub(super) fn publish(&self) -> Result<PublishedEndpoint> {
        let service = sdp::publish()?;
        let object: &AnyObject = self;
        // SAFETY: The selector is implemented above with Apple's required
        // `(notification, channel)` signature, and `object` remains retained
        // by BluetoothMainThread until after unregistration.
        let notification = unsafe {
            IOBluetoothRFCOMMChannel::registerForChannelOpenNotifications_selector_withChannelID_direction(
                Some(object),
                Some(sel!(newRFCOMMChannelOpened:channel:)),
                service.channel_id,
                IOBluetoothUserNotificationChannelDirection::Incoming,
            )
        };
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
            // SAFETY: Neither freshly created registration has been installed
            // in actor state, so this stack frame still owns both resources.
            unsafe { notification.unregister() };
            // SAFETY: This process owns the freshly published record.
            let rollback_status = unsafe { service.record.removeServiceRecord() };
            if rollback_status != 0 {
                bail!("{error}; SDP rollback failed with IOReturn {rollback_status:#x}");
            }
            return Err(error);
        }
        state.service_record = Some(service.record);
        state.incoming_notification = Some(notification);
        Ok(PublishedEndpoint {
            record_handle: service.record_handle,
            channel_id: service.channel_id,
        })
    }

    pub(super) fn handle(&self, command: BtCommand) {
        match command {
            BtCommand::Send { token, data } => self.send(token, data),
            BtCommand::Close => self.begin_session_close(),
            BtCommand::Shutdown => self.begin_shutdown(),
        }
    }

    pub(super) fn finish(&self) -> Result<()> {
        let state = self.ivars().state.borrow();
        state.lifecycle.ensure_stopped()?;
        if state.incoming_notification.is_some() {
            bail!("Bluetooth shutdown ended with an incoming listener registered");
        }
        if state.service_record.is_some() {
            bail!("Bluetooth shutdown ended with the SDP service published");
        }
        if state.active.is_some() {
            bail!("Bluetooth shutdown ended before RFCOMM channel-close notification");
        }
        if !state.rejected.is_empty() {
            bail!("Bluetooth shutdown ended with rejected RFCOMM channels still closing");
        }
        if !state.pending_writes.is_empty() {
            bail!("Bluetooth shutdown ended with RFCOMM writes outstanding");
        }
        if let Some(error) = &state.fatal_error {
            return Err(anyhow!(error.clone()));
        }
        Ok(())
    }

    fn channel_opened(
        &self,
        notification: &IOBluetoothUserNotification,
        channel: &IOBluetoothRFCOMMChannel,
    ) {
        {
            let state = self.ivars().state.borrow();
            if state.incoming_notification.as_deref() != Some(notification) {
                drop(state);
                self.reject_channel(channel);
                self.emit_error("receive RFCOMM channel from unknown listener", -1);
                return;
            }
            if state.active.is_some() {
                drop(state);
                self.reject_channel(channel);
                return;
            }
        }

        let channel = IOBluetoothRFCOMMChannel::retain(channel);
        // SAFETY: The channel is open and accessed only by the main-thread actor.
        let mtu = usize::from(unsafe { channel.getMTU() });
        // SAFETY: Pairing state and address reads are non-blocking. Pairing UI
        // remains entirely owned by macOS.
        let remote = unsafe { channel.getDevice() };
        let paired = remote
            .as_deref()
            .is_some_and(|device| unsafe { device.isPaired() });
        let device = remote
            .and_then(|device| unsafe { device.addressString() })
            .and_then(|address| DeviceAddress::new(&address.to_string()).ok());
        let Some(device) = device else {
            self.reject_channel(&channel);
            self.emit_error("read client address", -1);
            return;
        };
        if mtu == 0 {
            self.reject_channel(&channel);
            self.emit_error("read RFCOMM MTU", -1);
            return;
        }

        let object: &AnyObject = self;
        // SAFETY: The selector has Apple's `(notification, channel)` shape and
        // this main-thread actor outlives the registration.
        let close_notification = unsafe {
            channel.registerForChannelCloseNotification_selector(
                Some(object),
                Some(sel!(rfcommChannelClosedNotification:channel:)),
            )
        };
        let Some(close_notification) = close_notification else {
            self.close_untracked_channel(
                &channel,
                "close RFCOMM channel without close notification",
            );
            self.emit_error("register RFCOMM channel-close notification", -1);
            return;
        };

        // SAFETY: `self` implements the delegate, both objects remain on the
        // native main thread, and the close registration is already live.
        let status = unsafe { channel.setDelegate(Some(object)) };
        if status != 0 {
            // SAFETY: This actor owns the close notification registration.
            unsafe { close_notification.unregister() };
            self.reject_channel(&channel);
            self.emit_error("set RFCOMM delegate", status);
            return;
        }

        if self
            .ivars()
            .state
            .borrow_mut()
            .lifecycle
            .channel_opened()
            .is_err()
        {
            // SAFETY: This actor still owns the unused registration.
            unsafe { close_notification.unregister() };
            self.reject_channel(&channel);
            self.emit_error("open overlapping RFCOMM channel", -1);
            return;
        }
        self.ivars().state.borrow_mut().active = Some(ActiveChannel {
            channel: channel.clone(),
            close_notification: Some(close_notification),
        });

        if self
            .ivars()
            .events
            .send(BtEvent::Connected {
                device,
                mtu,
                paired,
            })
            .is_err()
        {
            self.begin_session_close();
        }
    }

    fn channel_closed(&self, channel: Option<&IOBluetoothRFCOMMChannel>) {
        let Some(channel) = channel else {
            self.emit_error("receive RFCOMM close without channel", -1);
            return;
        };
        let closed = {
            let mut state = self.ivars().state.borrow_mut();
            let is_current = state
                .active
                .as_ref()
                .is_some_and(|active| std::ptr::eq(active.channel.as_ref(), channel));
            if is_current {
                let active = state.active.as_mut().expect("active channel was checked");
                let Some(close_notification) = active.close_notification.take() else {
                    return;
                };
                (close_notification, state.lifecycle.channel_closed())
            } else {
                let key = Self::channel_key(channel);
                let Some(rejected) = state.rejected.remove(&key) else {
                    return;
                };
                debug_assert!(std::ptr::eq(rejected.channel.as_ref(), channel));
                (
                    rejected.close_notification,
                    state.lifecycle.rejected_channel_closed(),
                )
            }
        };

        // SAFETY: The callback proves the channel is closed. The RefCell
        // borrow was released first so framework unregistration may not
        // re-enter Rust while mutable actor state is borrowed.
        unsafe { closed.0.unregister() };

        match closed.1 {
            Ok(action) => self.apply_lifecycle_action(action),
            Err(_) => self.emit_error("complete RFCOMM close lifecycle", -1),
        }
    }

    fn send(&self, token: WriteToken, data: Bytes) {
        let Ok(refcon) = usize::try_from(token.get()) else {
            self.emit_error("encode RFCOMM write token", -1);
            return;
        };
        let channel = self
            .ivars()
            .state
            .borrow()
            .active
            .as_ref()
            .map(|active| active.channel.clone());
        let Some(channel) = channel else {
            self.emit_error("write without RFCOMM client", -1);
            return;
        };
        // SAFETY: The retained channel is main-thread-owned. No Rust actor
        // state is borrowed while invoking the framework.
        let mtu = usize::from(unsafe { channel.getMTU() });
        if data.is_empty() || data.len() > mtu || data.len() > usize::from(u16::MAX) {
            self.emit_error("invalid RFCOMM write length", -1);
            return;
        }

        let prepared = {
            let mut state = self.ivars().state.borrow_mut();
            if state.lifecycle.write_started().is_err() {
                drop(state);
                self.emit_error("overlapping or closing RFCOMM write", -1);
                return;
            }

            let pointer = data.as_ptr().cast_mut().cast::<c_void>();
            let length = u16::try_from(data.len()).expect("length was bounded above");
            state
                .pending_writes
                .insert(token, PendingWrite { token, data });
            (pointer, length)
        };

        let (pointer, length) = prepared;
        // SAFETY: `pointer` addresses Bytes retained in `pending_writes` until
        // the matching write-completion callback. A channel-close callback by
        // itself never releases the buffer.
        let status =
            unsafe { channel.writeAsync_length_refcon(pointer, length, refcon as *mut c_void) };
        if status != 0 {
            let action = {
                let mut state = self.ivars().state.borrow_mut();
                state.pending_writes.remove(&token);
                state.lifecycle.write_completed()
            };
            let _ = self.ivars().events.send(BtEvent::WriteComplete {
                token,
                bytes_written: 0,
                status,
            });
            match action {
                Ok(action) => self.apply_lifecycle_action(action),
                Err(_) => self.emit_error("roll back rejected RFCOMM write", -1),
            }
        }
    }

    fn write_completed(&self, refcon: *mut c_void, status: i32, actual: Option<usize>) {
        let token_value = u64::try_from(refcon as usize).expect("usize always fits in u64");
        let Some(token) = WriteToken::new(token_value) else {
            self.emit_error("decode RFCOMM write token", -1);
            return;
        };
        let (pending, action) = {
            let mut state = self.ivars().state.borrow_mut();
            let pending = state.pending_writes.remove(&token);
            let action = pending.as_ref().map(|_| state.lifecycle.write_completed());
            (pending, action)
        };
        let Some(pending) = pending else {
            return;
        };
        debug_assert_eq!(pending.token, token);
        let bytes_written =
            actual.unwrap_or_else(|| if status == 0 { pending.data.len() } else { 0 });
        let _ = self.ivars().events.send(BtEvent::WriteComplete {
            token,
            bytes_written,
            status,
        });
        match action.expect("pending write has a barrier transition") {
            Ok(action) => self.apply_lifecycle_action(action),
            Err(_) => self.emit_error("complete RFCOMM write barrier", -1),
        }
    }

    fn begin_session_close(&self) {
        let action = self
            .ivars()
            .state
            .borrow_mut()
            .lifecycle
            .begin_session_close();
        match action {
            Ok(action) => self.apply_lifecycle_action(action),
            Err(_) => self.emit_error("begin RFCOMM session close", -1),
        }
    }

    fn begin_shutdown(&self) {
        let action = self.ivars().state.borrow_mut().lifecycle.begin_shutdown();
        if let Ok(action) = action {
            self.apply_lifecycle_action(action);
        } else {
            self.emit_error("begin Bluetooth shutdown", -1);
            Self::stop_run_loop();
        }
    }

    fn apply_lifecycle_action(&self, action: LifecycleAction) {
        match action {
            LifecycleAction::Wait => {}
            LifecycleAction::CloseChannel => self.submit_channel_close(),
            LifecycleAction::NotifyDisconnected => {
                if let Err(error) = self.release_closed_active() {
                    self.emit_error(error, -1);
                    Self::stop_run_loop();
                    return;
                }
                let _ = self.ivars().events.send(BtEvent::Disconnected);
            }
            LifecycleAction::WithdrawService => {
                if self.ivars().state.borrow().active.is_some()
                    && let Err(error) = self.release_closed_active()
                {
                    self.emit_error(error, -1);
                    Self::stop_run_loop();
                    return;
                }
                self.withdraw_service();
            }
            LifecycleAction::StopRunLoop => Self::stop_run_loop(),
        }
    }

    fn submit_channel_close(&self) {
        let channel = {
            let state = self.ivars().state.borrow();
            if !state.pending_writes.is_empty() {
                drop(state);
                self.emit_error("close RFCOMM channel with write outstanding", -1);
                Self::stop_run_loop();
                return;
            }
            state.active.as_ref().and_then(|active| {
                active
                    .close_notification
                    .as_ref()
                    .map(|_| active.channel.clone())
            })
        };
        let Some(channel) = channel else {
            self.emit_error("close missing RFCOMM channel", -1);
            Self::stop_run_loop();
            return;
        };

        // SAFETY: The channel is retained across a possible synchronous close
        // callback. Completion still requires the registered close callback.
        let status = unsafe { channel.closeChannel() };
        if status == 0 {
            return;
        }
        self.emit_error("close RFCOMM channel", status);

        // Do not close the device-wide ACL as a fallback: it is shared with
        // unrelated profiles such as HFP and A2DP. Keep ownership and wait for
        // a real remote close callback instead of pretending the close barrier
        // completed. The process exits with this recorded error after cleanup.
    }

    fn withdraw_service(&self) {
        let record = self.ivars().state.borrow().service_record.clone();
        let Some(record) = record else {
            self.emit_error("remove missing SDP service", -1);
            Self::stop_run_loop();
            return;
        };

        // SAFETY: Every application-visible and rejected RFCOMM channel has
        // produced its close callback before the lifecycle allows this action.
        let status = unsafe { record.removeServiceRecord() };
        if status != 0 {
            self.emit_error("remove SDP service", status);
            Self::stop_run_loop();
            return;
        }

        let notification = {
            let mut state = self.ivars().state.borrow_mut();
            state.service_record = None;
            let Some(notification) = state.incoming_notification.take() else {
                drop(state);
                self.emit_error("unregister missing RFCOMM listener", -1);
                Self::stop_run_loop();
                return;
            };
            notification
        };

        // SAFETY: Service removal succeeded and the actor owns this live
        // incoming-channel registration. The RefCell borrow was released
        // before the framework call. Human operation guarantees no new remote
        // connection attempt during final shutdown.
        unsafe { notification.unregister() };
        let action = self
            .ivars()
            .state
            .borrow_mut()
            .lifecycle
            .service_withdrawn();

        if let Ok(action) = action {
            self.apply_lifecycle_action(action);
        } else {
            self.emit_error("complete Bluetooth service withdrawal", -1);
            Self::stop_run_loop();
        }
    }

    fn release_closed_active(&self) -> Result<(), &'static str> {
        let mut state = self.ivars().state.borrow_mut();
        let Some(active) = state.active.as_ref() else {
            return Err("release missing closed RFCOMM channel");
        };
        if active.close_notification.is_some() {
            return Err("release RFCOMM channel before close callback");
        }
        if !state.pending_writes.is_empty() {
            return Err("release RFCOMM channel before write completion");
        }
        state.active = None;
        Ok(())
    }

    fn reject_channel(&self, channel: &IOBluetoothRFCOMMChannel) {
        let channel = IOBluetoothRFCOMMChannel::retain(channel);
        let object: &AnyObject = self;
        // SAFETY: The selector has Apple's `(notification, channel)` shape and
        // the rejected channel is retained until its close callback.
        let close_notification = unsafe {
            channel.registerForChannelCloseNotification_selector(
                Some(object),
                Some(sel!(rfcommChannelClosedNotification:channel:)),
            )
        };
        let Some(close_notification) = close_notification else {
            self.close_untracked_channel(
                &channel,
                "close rejected RFCOMM channel without close notification",
            );
            self.emit_error("register rejected RFCOMM close notification", -1);
            return;
        };

        let key = Self::channel_key(&channel);
        let mut close_notification = Some(close_notification);
        let installed = {
            let mut state = self.ivars().state.borrow_mut();
            if state.rejected.contains_key(&key)
                || state.lifecycle.rejected_channel_opened().is_err()
            {
                false
            } else {
                state.rejected.insert(
                    key,
                    RejectedChannel {
                        channel: channel.clone(),
                        close_notification: close_notification
                            .take()
                            .expect("close notification is consumed once"),
                    },
                );
                true
            }
        };
        if !installed {
            if let Some(close_notification) = close_notification {
                // SAFETY: The rejected channel was never installed, so this
                // registration remains locally owned.
                unsafe { close_notification.unregister() };
            }
            self.close_untracked_channel(&channel, "close untracked duplicate RFCOMM channel");
            self.emit_error("track rejected RFCOMM channel", -1);
            return;
        }

        // SAFETY: Ownership and close notification were installed before the
        // close request, including if the callback is delivered synchronously.
        let status = unsafe { channel.closeChannel() };
        if status != 0 {
            self.emit_error("close rejected RFCOMM channel", status);
        }
    }

    fn close_untracked_channel(&self, channel: &IOBluetoothRFCOMMChannel, operation: &'static str) {
        // SAFETY: This is a borrowed open channel delivered on the actor's
        // native main thread. No global ACL fallback is attempted.
        let status = unsafe { channel.closeChannel() };
        if status != 0 {
            self.emit_error(operation, status);
        }
    }

    fn channel_key(channel: &IOBluetoothRFCOMMChannel) -> usize {
        std::ptr::from_ref(channel) as usize
    }

    fn is_active_channel(&self, channel: Option<&IOBluetoothRFCOMMChannel>) -> bool {
        let Some(channel) = channel else {
            return false;
        };
        self.ivars()
            .state
            .borrow()
            .active
            .as_ref()
            .is_some_and(|active| std::ptr::eq(active.channel.as_ref(), channel))
    }

    fn emit_error(&self, operation: &'static str, status: i32) {
        let error = BtError::new(operation, status);
        self.ivars()
            .state
            .borrow_mut()
            .fatal_error
            .get_or_insert_with(|| error.clone());
        let _ = self.ivars().events.send(BtEvent::Error(error));
    }

    fn stop_run_loop() {
        if let Some(run_loop) = CFRunLoop::main() {
            run_loop.stop();
        }
    }
}
