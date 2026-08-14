use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use anyhow::{Result, bail};
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

use super::sdp;
use super::types::{BtCommand, BtError, BtEvent, DeviceAddress, WriteToken};

struct PendingWrite {
    #[allow(dead_code)]
    token: WriteToken,
    data: Bytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TxState {
    Idle,
    Writing { token: WriteToken },
}

struct ActorState {
    service_record: Option<Retained<IOBluetoothSDPServiceRecord>>,
    notification: Option<Retained<IOBluetoothUserNotification>>,
    channel: Option<Retained<IOBluetoothRFCOMMChannel>>,
    pending_writes: HashMap<WriteToken, PendingWrite>,
    tx: TxState,
    shutting_down: bool,
}

impl Default for ActorState {
    fn default() -> Self {
        Self {
            service_record: None,
            notification: None,
            channel: None,
            pending_writes: HashMap::new(),
            tx: TxState::Idle,
            shutting_down: false,
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
            _channel: Option<&IOBluetoothRFCOMMChannel>,
            data_pointer: *mut c_void,
            data_length: usize,
        ) {
            autoreleasepool(|_| {
                if data_pointer.is_null() || data_length == 0 {
                    return;
                }
                // SAFETY: IOBluetooth guarantees this buffer is valid for the
                // duration of the delegate callback. It is copied immediately.
                let data = unsafe {
                    std::slice::from_raw_parts(data_pointer.cast::<u8>(), data_length)
                };
                let _ = self.ivars().events.send(BtEvent::Data(Bytes::copy_from_slice(data)));
            });
        }

        #[unsafe(method(rfcommChannelClosed:))]
        unsafe fn rfcommChannelClosed(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
        ) {
            autoreleasepool(|_| self.channel_closed());
        }

        #[unsafe(method(rfcommChannelFlowControlChanged:))]
        unsafe fn rfcommChannelFlowControlChanged(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
        ) {
            let _ = self.ivars().events.send(BtEvent::FlowControlChanged);
        }

        #[unsafe(method(rfcommChannelQueueSpaceAvailable:))]
        unsafe fn rfcommChannelQueueSpaceAvailable(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
        ) {
            let _ = self.ivars().events.send(BtEvent::FlowControlChanged);
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:))]
        unsafe fn rfcommChannelWriteComplete_refcon_status(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
            refcon: *mut c_void,
            status: i32,
        ) {
            autoreleasepool(|_| self.write_completed(refcon, status, None));
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:bytesWritten:))]
        unsafe fn rfcommChannelWriteComplete_refcon_status_bytesWritten(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
            refcon: *mut c_void,
            status: i32,
            bytes_written: usize,
        ) {
            autoreleasepool(|_| self.write_completed(refcon, status, Some(bytes_written)));
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
        state.notification = Some(notification);
        Ok(service.channel_id)
    }

    pub(super) fn handle(&self, command: BtCommand) {
        match command {
            BtCommand::Send { token, data } => self.send(token, data),
            BtCommand::Close => self.close(),
            BtCommand::Shutdown => {
                self.shutdown();
            }
        }
    }

    pub(super) fn shutdown(&self) -> bool {
        let (channel, notification, record) = {
            let mut state = self.ivars().state.borrow_mut();
            if state.shutting_down {
                return state.pending_writes.is_empty();
            }
            state.shutting_down = true;
            (
                state.channel.take(),
                state.notification.take(),
                state.service_record.take(),
            )
        };

        let had_channel = channel.is_some();
        let mut close_failed = false;
        if let Some(channel) = channel {
            // SAFETY: The channel is owned by this main-thread actor.
            let status = unsafe { channel.closeChannel() };
            if status != 0 {
                close_failed = true;
                self.emit_error("close RFCOMM channel during shutdown", status);
            }
        }
        if let Some(notification) = notification {
            // SAFETY: The actor owns this live registration.
            unsafe { notification.unregister() };
        }
        if let Some(record) = record {
            // SAFETY: The actor owns this published transient service.
            let status = unsafe { record.removeServiceRecord() };
            if status != 0 {
                self.emit_error("remove SDP service", status);
            }
        }
        let mut state = self.ivars().state.borrow_mut();
        if close_failed || !had_channel {
            state.pending_writes.clear();
            state.tx = TxState::Idle;
        }
        state.pending_writes.is_empty()
    }

    fn channel_opened(
        &self,
        notification: &IOBluetoothUserNotification,
        channel: &IOBluetoothRFCOMMChannel,
    ) {
        {
            let state = self.ivars().state.borrow();
            if state.shutting_down
                || state.notification.as_deref() != Some(notification)
                || state.channel.is_some()
            {
                // SAFETY: This is an incoming channel delivered on the actor's
                // main thread. It was never installed as the active channel.
                unsafe { channel.closeChannel() };
                return;
            }
        }

        // Retain before installing the channel because Apple's notification
        // callback only lends the object for the duration of this call.
        let channel = IOBluetoothRFCOMMChannel::retain(channel);
        // SAFETY: The channel is open and accessed only by the main-thread actor.
        let mtu = usize::from(unsafe { channel.getMTU() });
        // SAFETY: The channel is open and accessed only by the main-thread actor.
        // Reading pairing state is non-blocking. Pairing itself is owned by
        // macOS; this callback never opens UI or requests authentication.
        let remote = unsafe { channel.getDevice() };
        let paired = remote
            .as_deref()
            .is_some_and(|device| unsafe { device.isPaired() });
        let encrypted = remote
            .as_deref()
            .is_some_and(|device| unsafe { device.getEncryptionMode() != 0 });
        let device = remote
            .and_then(|device| unsafe { device.addressString() })
            .and_then(|address| DeviceAddress::new(&address.to_string()).ok());
        let Some(device) = device else {
            // SAFETY: The retained incoming channel is valid and main-thread-owned.
            unsafe { channel.closeChannel() };
            self.emit_error("read client address", -1);
            return;
        };
        if mtu == 0 {
            // SAFETY: The retained incoming channel is valid and main-thread-owned.
            unsafe { channel.closeChannel() };
            self.emit_error("read RFCOMM MTU", -1);
            return;
        }

        self.ivars().state.borrow_mut().channel = Some(channel.clone());
        if self
            .ivars()
            .events
            .send(BtEvent::Connected {
                device,
                mtu,
                paired,
                encrypted,
            })
            .is_err()
        {
            self.close();
            return;
        }

        let object: &AnyObject = self;
        // SAFETY: `self` implements IOBluetoothRFCOMMChannelDelegate, remains
        // retained by BluetoothMainThread, and both objects stay on main.
        let status = unsafe { channel.setDelegate(Some(object)) };
        if status != 0 {
            self.emit_error("set RFCOMM delegate", status);
            self.close();
        }
    }

    fn channel_closed(&self) {
        let shutting_down = {
            let mut state = self.ivars().state.borrow_mut();
            state.channel = None;
            state.pending_writes.clear();
            state.tx = TxState::Idle;
            state.shutting_down
        };
        if shutting_down {
            if let Some(run_loop) = CFRunLoop::main() {
                run_loop.stop();
            }
        } else {
            let _ = self.ivars().events.send(BtEvent::Disconnected);
        }
    }

    fn send(&self, token: WriteToken, data: Bytes) {
        let Ok(refcon) = usize::try_from(token.get()) else {
            self.emit_error("encode RFCOMM write token", -1);
            return;
        };
        let (channel, pointer, length) = {
            let mut state = self.ivars().state.borrow_mut();
            let Some(channel) = state.channel.clone() else {
                drop(state);
                self.emit_error("write without RFCOMM client", -1);
                return;
            };
            if state.tx != TxState::Idle {
                drop(state);
                self.emit_error("overlapping RFCOMM write", -1);
                return;
            }
            let mtu = usize::from(unsafe { channel.getMTU() });
            if data.is_empty() || data.len() > mtu || data.len() > usize::from(u16::MAX) {
                drop(state);
                self.emit_error("invalid RFCOMM write length", -1);
                return;
            }

            let pointer = data.as_ptr().cast_mut().cast::<c_void>();
            let length = u16::try_from(data.len()).expect("length was bounded above");
            state
                .pending_writes
                .insert(token, PendingWrite { token, data });
            state.tx = TxState::Writing { token };
            (channel, pointer, length)
        };

        // SAFETY: `pointer` addresses the Bytes retained in `pending_writes`.
        // It remains alive until the matching delegate completion removes it.
        let status =
            unsafe { channel.writeAsync_length_refcon(pointer, length, refcon as *mut c_void) };
        if status != 0 {
            let mut state = self.ivars().state.borrow_mut();
            state.pending_writes.remove(&token);
            state.tx = TxState::Idle;
            drop(state);
            let _ = self.ivars().events.send(BtEvent::WriteComplete {
                token,
                bytes_written: 0,
                status,
            });
        }
    }

    fn write_completed(&self, refcon: *mut c_void, status: i32, actual: Option<usize>) {
        let token_value = u64::try_from(refcon as usize).expect("usize always fits in u64");
        let Some(token) = WriteToken::new(token_value) else {
            self.emit_error("decode RFCOMM write token", -1);
            return;
        };
        let (pending, current) = {
            let mut state = self.ivars().state.borrow_mut();
            let current = state.tx == (TxState::Writing { token });
            if current {
                state.tx = TxState::Idle;
            }
            (state.pending_writes.remove(&token), current)
        };
        let Some(pending) = pending else {
            return;
        };
        debug_assert_eq!(pending.token, token);
        if !current {
            return;
        }
        let bytes_written =
            actual.unwrap_or_else(|| if status == 0 { pending.data.len() } else { 0 });
        let _ = self.ivars().events.send(BtEvent::WriteComplete {
            token,
            bytes_written,
            status,
        });
    }

    fn close(&self) {
        let channel = {
            let mut state = self.ivars().state.borrow_mut();
            state.channel.take()
        };
        let Some(channel) = channel else {
            let _ = self.ivars().events.send(BtEvent::Disconnected);
            return;
        };
        // SAFETY: The channel is retained locally through a possible
        // synchronous close callback, and the actor is on the main thread.
        let status = unsafe { channel.closeChannel() };
        if status != 0 {
            let mut state = self.ivars().state.borrow_mut();
            state.pending_writes.clear();
            state.tx = TxState::Idle;
            drop(state);
            self.emit_error("close RFCOMM channel", status);
            let _ = self.ivars().events.send(BtEvent::Disconnected);
        }
    }

    fn emit_error(&self, operation: &'static str, status: i32) {
        let _ = self
            .ivars()
            .events
            .send(BtEvent::Error(BtError::new(operation, status)));
    }
}
