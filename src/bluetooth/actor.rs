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

use super::close_barrier::{CloseAction, CloseBarrier, CloseCompletion, CloseRequest};
use super::sdp;
use super::types::{BtCommand, BtError, BtEvent, DeviceAddress, WriteToken};

struct PendingWrite {
    token: WriteToken,
    data: Bytes,
}

struct ActiveChannel {
    channel: Retained<IOBluetoothRFCOMMChannel>,
    close_notification: Retained<IOBluetoothUserNotification>,
}

struct ActorState {
    service_record: Option<Retained<IOBluetoothSDPServiceRecord>>,
    incoming_notification: Option<Retained<IOBluetoothUserNotification>>,
    active: Option<ActiveChannel>,
    pending_writes: HashMap<WriteToken, PendingWrite>,
    close_barrier: CloseBarrier,
    fatal_error: Option<BtError>,
}

impl Default for ActorState {
    fn default() -> Self {
        Self {
            service_record: None,
            incoming_notification: None,
            active: None,
            pending_writes: HashMap::new(),
            close_barrier: CloseBarrier::new(),
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

    pub(super) fn publish(&self) -> Result<u8> {
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
            unsafe { service.record.removeServiceRecord() };
            bail!("register incoming RFCOMM channel notification");
        };

        let mut state = self.ivars().state.borrow_mut();
        state.service_record = Some(service.record);
        state.incoming_notification = Some(notification);
        Ok(service.channel_id)
    }

    pub(super) fn handle(&self, command: BtCommand) {
        match command {
            BtCommand::Send { token, data } => self.send(token, data),
            BtCommand::Close => self.begin_close(CloseRequest::Session),
            BtCommand::Shutdown => self.shutdown(),
        }
    }

    pub(super) fn shutdown(&self) {
        let (notification, record) = {
            let mut state = self.ivars().state.borrow_mut();
            (
                state.incoming_notification.take(),
                state.service_record.clone(),
            )
        };

        // Withdraw the listener before touching the active channel. This makes
        // it impossible for a new incoming channel to enter during teardown.
        if let Some(notification) = notification {
            // SAFETY: The actor owns this live registration.
            unsafe { notification.unregister() };
        }
        if let Some(record) = record {
            // SAFETY: The actor owns this published transient service.
            let status = unsafe { record.removeServiceRecord() };
            if status == 0 {
                self.ivars().state.borrow_mut().service_record = None;
            } else {
                self.emit_error("remove SDP service", status);
            }
        }

        self.begin_close(CloseRequest::Shutdown);
    }

    pub(super) fn finish(&self) -> Result<()> {
        let state = self.ivars().state.borrow();
        if state.incoming_notification.is_some() {
            bail!("Bluetooth shutdown ended with an incoming listener registered");
        }
        if state.service_record.is_some() {
            bail!("Bluetooth shutdown ended with the SDP service published");
        }
        if state.active.is_some() {
            bail!("Bluetooth shutdown ended before RFCOMM channel-close notification");
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
            if state.incoming_notification.as_deref() != Some(notification)
                || state.active.is_some()
            {
                // SAFETY: This incoming channel was never installed as active.
                unsafe { channel.closeChannel() };
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
            // SAFETY: The retained channel is valid and main-thread-owned.
            unsafe { channel.closeChannel() };
            self.emit_error("read client address", -1);
            return;
        };
        if mtu == 0 {
            // SAFETY: The retained channel is valid and main-thread-owned.
            unsafe { channel.closeChannel() };
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
            // SAFETY: The channel was not exposed to the application.
            unsafe { channel.closeChannel() };
            self.emit_error("register RFCOMM channel-close notification", -1);
            return;
        };

        // SAFETY: `self` implements the delegate, both objects remain on the
        // native main thread, and the close registration is already live.
        let status = unsafe { channel.setDelegate(Some(object)) };
        if status != 0 {
            // SAFETY: This actor owns the close notification registration.
            unsafe { close_notification.unregister() };
            // SAFETY: The channel was not exposed to the application.
            unsafe { channel.closeChannel() };
            self.emit_error("set RFCOMM delegate", status);
            return;
        }

        if self
            .ivars()
            .state
            .borrow_mut()
            .close_barrier
            .channel_opened()
            .is_err()
        {
            // SAFETY: This actor still owns the unused registration.
            unsafe { close_notification.unregister() };
            // SAFETY: This duplicate channel was never installed as active.
            unsafe { channel.closeChannel() };
            self.emit_error("open overlapping RFCOMM channel", -1);
            return;
        }
        self.ivars().state.borrow_mut().active = Some(ActiveChannel {
            channel: channel.clone(),
            close_notification,
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
            self.begin_close(CloseRequest::Session);
        }
    }

    fn channel_closed(&self, channel: Option<&IOBluetoothRFCOMMChannel>) {
        let Some(channel) = channel else {
            self.emit_error("receive RFCOMM close without channel", -1);
            return;
        };
        let completion = {
            let mut state = self.ivars().state.borrow_mut();
            let is_current = state
                .active
                .as_ref()
                .is_some_and(|active| std::ptr::eq(active.channel.as_ref(), channel));
            if !is_current {
                return;
            }
            let active = state.active.take().expect("active channel was checked");
            // SAFETY: The callback proves the channel is closed, so this
            // actor-owned registration is no longer needed.
            unsafe { active.close_notification.unregister() };
            state.pending_writes.clear();
            state.close_barrier.channel_closed()
        };

        match completion {
            Ok(completion) => self.complete_close(completion),
            Err(_) => self.emit_error("complete RFCOMM close barrier", -1),
        }
    }

    fn send(&self, token: WriteToken, data: Bytes) {
        let Ok(refcon) = usize::try_from(token.get()) else {
            self.emit_error("encode RFCOMM write token", -1);
            return;
        };
        let prepared = {
            let mut state = self.ivars().state.borrow_mut();
            let Some(channel) = state.active.as_ref().map(|active| active.channel.clone()) else {
                drop(state);
                self.emit_error("write without RFCOMM client", -1);
                return;
            };
            // SAFETY: The channel is open and main-thread-owned.
            let mtu = usize::from(unsafe { channel.getMTU() });
            if data.is_empty() || data.len() > mtu || data.len() > usize::from(u16::MAX) {
                drop(state);
                self.emit_error("invalid RFCOMM write length", -1);
                return;
            }
            if state.close_barrier.write_started().is_err() {
                drop(state);
                self.emit_error("overlapping or closing RFCOMM write", -1);
                return;
            }

            let pointer = data.as_ptr().cast_mut().cast::<c_void>();
            let length = u16::try_from(data.len()).expect("length was bounded above");
            state
                .pending_writes
                .insert(token, PendingWrite { token, data });
            (channel, pointer, length)
        };

        let (channel, pointer, length) = prepared;
        // SAFETY: `pointer` addresses Bytes retained in `pending_writes` until
        // the matching completion or actual channel-close callback.
        let status =
            unsafe { channel.writeAsync_length_refcon(pointer, length, refcon as *mut c_void) };
        if status != 0 {
            let action = {
                let mut state = self.ivars().state.borrow_mut();
                state.pending_writes.remove(&token);
                state.close_barrier.write_completed()
            };
            let _ = self.ivars().events.send(BtEvent::WriteComplete {
                token,
                bytes_written: 0,
                status,
            });
            match action {
                Ok(action) => self.apply_close_action(action),
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
            let action = pending
                .as_ref()
                .map(|_| state.close_barrier.write_completed());
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
            Ok(action) => self.apply_close_action(action),
            Err(_) => self.emit_error("complete RFCOMM write barrier", -1),
        }
    }

    fn begin_close(&self, request: CloseRequest) {
        let action = self
            .ivars()
            .state
            .borrow_mut()
            .close_barrier
            .begin_close(request);
        self.apply_close_action(action);
    }

    fn apply_close_action(&self, action: CloseAction) {
        match action {
            CloseAction::Wait => {}
            CloseAction::CloseChannel => self.submit_channel_close(),
            CloseAction::Complete(completion) => self.complete_close(completion),
        }
    }

    fn submit_channel_close(&self) {
        let channel = self
            .ivars()
            .state
            .borrow()
            .active
            .as_ref()
            .map(|active| active.channel.clone());
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

        // A failed RFCOMM close submission cannot satisfy the barrier. Ask the
        // owning device connection to close, then still wait for the channel
        // callback. If even that request fails, exit explicitly with failure.
        let fallback =
            unsafe { channel.getDevice() }.map_or(-1, |device| unsafe { device.closeConnection() });
        if fallback != 0 {
            self.emit_error("close Bluetooth device connection", fallback);
            Self::stop_run_loop();
        }
    }

    fn complete_close(&self, completion: CloseCompletion) {
        match completion {
            CloseCompletion::Remote | CloseCompletion::Session => {
                let _ = self.ivars().events.send(BtEvent::Disconnected);
            }
            CloseCompletion::Shutdown => Self::stop_run_loop(),
        }
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
