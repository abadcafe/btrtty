use objc2::runtime::AnyObject;
use objc2::{DefinedClass, Message, sel};
use objc2_io_bluetooth::{IOBluetoothRFCOMMChannel, IOBluetoothUserNotification};

use super::BluetoothActor;
use super::state::{
    ActiveChannel, DelegateState, DeliveredChannel, DeliveryState, InputState, RejectedChannel,
};
use crate::bluetooth::dispatch::NativeAction;
use crate::bluetooth::types::{BtEvent, ConnectionId, DeviceAddress};

impl BluetoothActor {
    pub(super) fn stage_delivered_channel(
        &self,
        _notification: &IOBluetoothUserNotification,
        channel: &IOBluetoothRFCOMMChannel,
    ) {
        let channel = IOBluetoothRFCOMMChannel::retain(channel);
        let key = Self::channel_key(&channel);
        let connection = {
            let mut state = self.ivars().state.borrow_mut();
            if state.delivered.contains_key(&key)
                || state
                    .active
                    .as_ref()
                    .is_some_and(|active| Self::channel_key(&active.channel) == key)
                || state.rejected.contains_key(&key)
            {
                return;
            }
            let connection = state.next_connection;
            state.next_connection = connection.next();
            state.delivered.insert(
                key,
                DeliveredChannel {
                    connection,
                    channel: channel.clone(),
                    close_notification: None,
                    delegate: DelegateState::Missing,
                    state: DeliveryState::Ready,
                },
            );
            connection
        };

        let object: &AnyObject = self;
        // SAFETY: The staged channel is retained and the selector signature is
        // exact. Registration is native ownership setup, not application work.
        let close_notification = unsafe {
            channel.registerForChannelCloseNotification_selector(
                Some(object),
                Some(sel!(rfcommChannelClosedNotification:channel:)),
            )
        };
        // SAFETY: The actor implements the delegate and is main-thread-only.
        let delegate_status = unsafe { channel.setDelegate(Some(object)) };

        if let Some(delivered) = self.ivars().state.borrow_mut().delivered.get_mut(&key) {
            delivered.close_notification = close_notification;
            delivered.delegate = if delegate_status == 0 {
                DelegateState::Installed
            } else {
                DelegateState::Missing
            };
            debug_assert_eq!(delivered.connection, connection);
        }
        self.defer(NativeAction::OpenDelivered { channel_key: key });
    }

    pub(super) fn open_delivered_channel(&self, key: u64) {
        let delivered = self.ivars().state.borrow_mut().delivered.remove(&key);
        let Some(delivered) = delivered else {
            return;
        };
        if delivered.state == DeliveryState::Closed {
            if let Some(notification) = delivered.close_notification {
                // SAFETY: The close was already observed and this registration
                // remains owned by the delivered channel.
                unsafe { notification.unregister() };
            }
            if self.ivars().state.borrow().lifecycle.is_withdrawing() {
                self.defer(NativeAction::WithdrawService);
            }
            return;
        }

        let tracking_available = delivered.delegate == DelegateState::Installed
            || delivered.close_notification.is_some();
        if !tracking_available {
            self.reject_delivered(delivered, true);
            return;
        }
        if delivered.state == DeliveryState::InputOverflow {
            self.reject_delivered(delivered, false);
            return;
        }

        // SAFETY: These non-blocking channel metadata reads happen in the
        // deferred main-runloop action, outside the IOBluetooth callback stack.
        let mtu = usize::from(unsafe { delivered.channel.getMTU() });
        // SAFETY: The retained channel owns its device for this call.
        let remote = unsafe { delivered.channel.getDevice() };
        let paired = remote
            .as_deref()
            .is_some_and(|device| unsafe { device.isPaired() });
        let device = remote
            .and_then(|device| unsafe { device.addressString() })
            .and_then(|address| DeviceAddress::new(&address.to_string()).ok());
        let can_accept = {
            let state = self.ivars().state.borrow();
            state.acceptance.is_armed()
                && state.active.is_none()
                && delivered.delegate == DelegateState::Installed
        };
        let Some(device) = device else {
            self.reject_delivered(delivered, false);
            return;
        };
        if mtu == 0 || !can_accept {
            self.reject_delivered(delivered, false);
            return;
        }

        let opened = {
            let mut state = self.ivars().state.borrow_mut();
            if !state.acceptance.begin_connection() {
                false
            } else if state.lifecycle.channel_opened().is_ok() {
                true
            } else {
                state.acceptance.cancel_begin();
                false
            }
        };
        if !opened {
            self.reject_delivered(delivered, false);
            return;
        }

        let DeliveredChannel {
            connection,
            channel,
            close_notification,
            ..
        } = delivered;
        {
            let mut state = self.ivars().state.borrow_mut();
            state.active = Some(ActiveChannel {
                connection,
                channel,
                close_notification,
                close_observed: false,
                input: InputState::Open,
            });
        }

        if self
            .ivars()
            .events
            .send(BtEvent::Connected {
                connection,
                device,
                mtu,
                paired,
            })
            .is_err()
        {
            self.begin_session_close(connection);
        }
    }

    fn reject_delivered(&self, delivered: DeliveredChannel, fatal: bool) {
        let key = Self::channel_key(&delivered.channel);
        let connection = delivered.connection;
        let channel = delivered.channel;
        let close_notification = delivered.close_notification;
        let input_overflowed = delivered.state == DeliveryState::InputOverflow;
        let tracking_available =
            delivered.delegate == DelegateState::Installed || close_notification.is_some();
        let tracked = {
            let mut state = self.ivars().state.borrow_mut();
            if state.lifecycle.rejected_channel_opened().is_err() {
                false
            } else {
                state.rejected.insert(
                    key,
                    RejectedChannel {
                        _channel: channel.clone(),
                        close_notification,
                    },
                );
                true
            }
        };
        if !tracked {
            self.emit_fatal("track rejected RFCOMM channel", -1);
            return;
        }

        // SAFETY: The channel is retained in the rejected registry before the
        // close request; any synchronous callback is deferred through actions.
        let status = unsafe { channel.closeChannel() };
        if status != 0 {
            self.emit_fatal("close rejected RFCOMM channel", status);
        }
        if fatal || !tracking_available {
            self.emit_fatal("observe rejected RFCOMM channel close", -1);
        } else if input_overflowed {
            self.emit_connection_error(connection, "RFCOMM input buffer overflow", -1);
        }
    }

    pub(super) fn accept_next(&self, after: ConnectionId) {
        {
            let mut state = self.ivars().state.borrow_mut();
            if !state.acceptance.is_awaiting(after) {
                return;
            }
            if state.incoming_listener.is_none() {
                drop(state);
                self.emit_fatal("renew missing RFCOMM listener notification", -1);
                return;
            }
            if !state.acceptance.arm(after) {
                drop(state);
                self.emit_fatal("arm renewed RFCOMM listener", -1);
                return;
            }
        }
        self.refresh_incoming_listener();
    }

    pub(super) fn channel_closed(&self, key: u64) {
        let (notification, action) = {
            let mut state = self.ivars().state.borrow_mut();
            if let Some(delivered) = state.delivered.get_mut(&key) {
                if delivered.state == DeliveryState::Closed {
                    return;
                }
                delivered.state = DeliveryState::Closed;
                (delivered.close_notification.take(), None)
            } else if state
                .active
                .as_ref()
                .is_some_and(|active| Self::channel_key(&active.channel) == key)
            {
                let notification = {
                    let active = state.active.as_mut().expect("active channel was checked");
                    if active.close_observed {
                        return;
                    }
                    active.close_observed = true;
                    active.close_notification.take()
                };
                state.queued_writes.clear();
                (notification, Some(state.lifecycle.channel_closed()))
            } else if let Some(rejected) = state.rejected.remove(&key) {
                (
                    rejected.close_notification,
                    Some(state.lifecycle.rejected_channel_closed()),
                )
            } else {
                return;
            }
        };

        if let Some(notification) = notification {
            // SAFETY: The native close action runs outside the callback and the
            // observed channel owns this registration.
            unsafe { notification.unregister() };
        }
        if let Some(action) = action {
            match action {
                Ok(action) => self.apply_lifecycle_action(action),
                Err(_) => self.emit_fatal("complete RFCOMM close lifecycle", -1),
            }
        }
    }

    pub(super) fn input_overflow(&self, key: u64) {
        let connection = {
            let mut state = self.ivars().state.borrow_mut();
            if let Some(delivered) = state.delivered.get_mut(&key) {
                if delivered.state == DeliveryState::Ready {
                    delivered.state = DeliveryState::InputOverflow;
                }
                return;
            }
            state.active.as_mut().and_then(|active| {
                if Self::channel_key(&active.channel) == key && active.input == InputState::Open {
                    active.input = InputState::Closed;
                    Some(active.connection)
                } else {
                    None
                }
            })
        };
        if let Some(connection) = connection {
            self.emit_connection_error(connection, "RFCOMM input buffer overflow", -1);
            self.begin_session_close(connection);
        }
    }

    pub(super) fn begin_session_close(&self, connection: ConnectionId) {
        let action = {
            let mut state = self.ivars().state.borrow_mut();
            let Some(active) = state.active.as_mut() else {
                return;
            };
            if active.connection != connection {
                return;
            }
            active.input = InputState::Closed;
            state.queued_writes.clear();
            state.lifecycle.begin_session_close()
        };
        match action {
            Ok(action) => self.apply_lifecycle_action(action),
            Err(_) => self.emit_fatal("begin RFCOMM session close", -1),
        }
    }

    pub(super) fn routed_connection(
        &self,
        channel: &IOBluetoothRFCOMMChannel,
    ) -> Option<ConnectionId> {
        let key = Self::channel_key(channel);
        let state = self.ivars().state.borrow();
        state
            .active
            .as_ref()
            .filter(|active| {
                Self::channel_key(&active.channel) == key && active.input == InputState::Open
            })
            .map(|active| active.connection)
            .or_else(|| {
                state
                    .delivered
                    .get(&key)
                    .filter(|delivered| delivered.state == DeliveryState::Ready)
                    .map(|delivered| delivered.connection)
            })
    }

    pub(super) fn channel_key(channel: &IOBluetoothRFCOMMChannel) -> u64 {
        // SAFETY: The callback or actor-owned Retained keeps the native
        // channel alive for this metadata read. IOBluetooth may construct a
        // distinct Objective-C wrapper for every matching user notification;
        // getObjectID is the documented process-independent identity of the
        // underlying RFCOMM channel, so all those wrappers must route to one
        // lifecycle entry.
        unsafe { channel.getObjectID() }
    }
}
