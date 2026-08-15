use std::ffi::c_void;

use bytes::Bytes;
use objc2::DefinedClass;
use objc2_io_bluetooth::IOBluetoothRFCOMMChannel;

use super::BluetoothActor;
use super::state::PendingWrite;
use crate::bluetooth::types::{BtEvent, ConnectionId, WriteToken};

impl BluetoothActor {
    pub(super) fn send(&self, connection: ConnectionId, token: WriteToken, data: Bytes) {
        let channel = {
            let state = self.ivars().state.borrow();
            let Some(active) = state.active.as_ref() else {
                return;
            };
            if active.connection != connection {
                return;
            }
            if state.queued_write.is_some() || !state.pending_writes.is_empty() {
                drop(state);
                self.emit_fatal("overlapping RFCOMM write command", -1);
                return;
            }
            active.channel.clone()
        };

        // SAFETY: The retained channel is accessed on its main thread.
        let mtu = usize::from(unsafe { channel.getMTU() });
        if data.is_empty() || data.len() > mtu || data.len() > usize::from(u16::MAX) {
            self.emit_connection_error(connection, "invalid RFCOMM write length", -1);
            self.begin_session_close(connection);
            return;
        }
        let write = PendingWrite {
            connection,
            token,
            data,
        };
        // SAFETY: This is a non-blocking flow-state query on the main thread.
        if unsafe { channel.isTransmissionPaused() } {
            self.ivars().state.borrow_mut().queued_write = Some(write);
            return;
        }
        self.submit_write(&channel, write);
    }

    pub(super) fn flow_available(&self, key: usize) {
        let (channel, write) = {
            let mut state = self.ivars().state.borrow_mut();
            let Some(active) = state.active.as_ref() else {
                return;
            };
            if Self::channel_key(&active.channel) != key {
                return;
            }
            let channel = active.channel.clone();
            let Some(write) = state.queued_write.take() else {
                return;
            };
            (channel, write)
        };
        // SAFETY: This is a non-blocking flow-state query on the main thread.
        if unsafe { channel.isTransmissionPaused() } {
            self.ivars().state.borrow_mut().queued_write = Some(write);
        } else {
            self.submit_write(&channel, write);
        }
    }

    fn submit_write(&self, channel: &IOBluetoothRFCOMMChannel, write: PendingWrite) {
        let Ok(refcon) = usize::try_from(write.token.get()) else {
            self.emit_fatal("encode RFCOMM write token", -1);
            return;
        };
        let connection = write.connection;
        let token = write.token;
        let pointer = write.data.as_ptr().cast_mut().cast::<c_void>();
        let length = u16::try_from(write.data.len()).expect("write length was validated");
        {
            let mut state = self.ivars().state.borrow_mut();
            if state.lifecycle.write_started().is_err() {
                drop(state);
                self.emit_connection_error(connection, "write on closing RFCOMM channel", -1);
                return;
            }
            state.pending_writes.insert(token, write);
        }

        // SAFETY: The pointer addresses Bytes retained until the matching
        // completion action. The write size is non-zero and no larger than MTU.
        let status =
            unsafe { channel.writeAsync_length_refcon(pointer, length, refcon as *mut c_void) };
        if status != 0 {
            let action = {
                let mut state = self.ivars().state.borrow_mut();
                state.pending_writes.remove(&token);
                state.lifecycle.write_completed()
            };
            self.emit_connection_error(connection, "submit RFCOMM async write", status);
            if let Ok(action) = action {
                self.apply_lifecycle_action(action);
            } else {
                self.emit_fatal("roll back RFCOMM write submission", -1);
            }
            self.begin_session_close(connection);
        }
    }

    pub(super) fn write_completed(
        &self,
        key: usize,
        refcon: usize,
        status: i32,
        actual: Option<usize>,
    ) {
        let Some(token_value) = u64::try_from(refcon).ok() else {
            self.emit_fatal("decode RFCOMM write token", -1);
            return;
        };
        let Some(token) = WriteToken::new(token_value) else {
            self.emit_fatal("decode RFCOMM write token", -1);
            return;
        };
        let (pending, action) = {
            let mut state = self.ivars().state.borrow_mut();
            let active_matches = state
                .active
                .as_ref()
                .is_some_and(|active| Self::channel_key(&active.channel) == key);
            if !active_matches {
                return;
            }
            let pending = state.pending_writes.remove(&token);
            let action = pending.as_ref().map(|_| state.lifecycle.write_completed());
            (pending, action)
        };
        let Some(pending) = pending else {
            self.emit_fatal("complete unknown RFCOMM write", -1);
            return;
        };
        let bytes_written =
            actual.unwrap_or_else(|| if status == 0 { pending.data.len() } else { 0 });
        let successful = status == 0 && bytes_written == pending.data.len();
        if successful {
            let _ = self.ivars().events.send(BtEvent::WriteComplete {
                connection: pending.connection,
                token: pending.token,
                bytes_written,
                status,
            });
        } else {
            let error_status = if status == 0 { -1 } else { status };
            self.emit_connection_error(
                pending.connection,
                "complete RFCOMM async write",
                error_status,
            );
        }
        match action.expect("pending write has a lifecycle transition") {
            Ok(action) => self.apply_lifecycle_action(action),
            Err(_) => self.emit_fatal("complete RFCOMM write lifecycle", -1),
        }
        if !successful {
            self.begin_session_close(pending.connection);
        }
    }
}
