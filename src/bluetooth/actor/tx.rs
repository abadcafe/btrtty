use std::ffi::c_void;

use bytes::Bytes;
use objc2::DefinedClass;
use objc2_io_bluetooth::IOBluetoothRFCOMMChannel;

use super::BluetoothActor;
use super::write_ledger::{AdmittedWrite, Completion, PendingWrite};
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
            active.channel.clone()
        };

        // SAFETY: The retained channel is accessed on its main thread.
        let mtu = usize::from(unsafe { channel.getMTU() });
        if data.is_empty() || data.len() > mtu || data.len() > usize::from(u16::MAX) {
            self.emit_connection_error(connection, "invalid RFCOMM write length", -1);
            self.begin_session_close(connection);
            return;
        }

        let admitted = self
            .ivars()
            .state
            .borrow_mut()
            .writes
            .admit(PendingWrite::new(connection, token, data));
        let Ok(admitted) = admitted else {
            self.emit_fatal("admit RFCOMM write token", -1);
            return;
        };

        // SAFETY: This is a non-blocking flow-state query on the main thread.
        if unsafe { channel.isTransmissionPaused() } {
            self.ivars().state.borrow_mut().writes.queue(admitted);
            return;
        }
        self.submit_write(&channel, admitted);
    }

    pub(super) fn flow_available(&self, key: u64) {
        loop {
            let (channel, write) = {
                let mut state = self.ivars().state.borrow_mut();
                let Some(active) = state.active.as_ref() else {
                    return;
                };
                if Self::channel_key(&active.channel) != key {
                    return;
                }
                let channel = active.channel.clone();
                let Some(write) = state.writes.pop_queued() else {
                    return;
                };
                (channel, write)
            };
            // SAFETY: This is a non-blocking flow-state query on the main thread.
            if unsafe { channel.isTransmissionPaused() } {
                self.ivars().state.borrow_mut().writes.push_front(write);
                return;
            }
            self.submit_write(&channel, write);
        }
    }

    fn submit_write(&self, channel: &IOBluetoothRFCOMMChannel, write: AdmittedWrite) {
        let connection = write.connection();
        let Ok(refcon) = usize::try_from(write.token().get()) else {
            self.emit_fatal("encode RFCOMM write token", -1);
            return;
        };
        let native = {
            let mut state = self.ivars().state.borrow_mut();
            if state.lifecycle.write_started().is_err() {
                drop(state);
                self.emit_connection_error(connection, "write on closing RFCOMM channel", -1);
                return;
            }
            state.writes.start(write)
        };

        // SAFETY: `NativeWrite` points into Bytes now retained by WriteLedger
        // until the matching completion or synchronous submission failure.
        let status = unsafe {
            channel.writeAsync_length_refcon(
                native.pointer(),
                native.length(),
                refcon as *mut c_void,
            )
        };
        if status != 0 {
            let action = {
                let mut state = self.ivars().state.borrow_mut();
                if state.writes.fail(native.token()).is_none() {
                    drop(state);
                    self.emit_fatal("roll back missing RFCOMM write", -1);
                    return;
                }
                state.lifecycle.write_completed()
            };
            self.emit_connection_error(native.connection(), "submit RFCOMM async write", status);
            if let Ok(action) = action {
                self.apply_lifecycle_action(action);
            } else {
                self.emit_fatal("roll back RFCOMM write submission", -1);
            }
            self.begin_session_close(native.connection());
        }
    }

    pub(super) fn write_completed(
        &self,
        key: u64,
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
            match state.writes.complete(token) {
                Completion::Current(pending) => {
                    let action = state.lifecycle.write_completed();
                    (pending, action)
                }
                Completion::Retired => return,
                Completion::Queued => {
                    drop(state);
                    self.emit_fatal("complete queued RFCOMM write", -1);
                    return;
                }
                Completion::Unknown => {
                    drop(state);
                    self.emit_fatal("complete unknown RFCOMM write", -1);
                    return;
                }
            }
        };
        let bytes_written =
            actual.unwrap_or_else(|| if status == 0 { pending.data().len() } else { 0 });
        let successful = status == 0 && bytes_written == pending.data().len();
        if successful {
            let _ = self.ivars().events.send(BtEvent::WriteComplete {
                connection: pending.connection(),
                token: pending.token(),
                bytes_written,
                status,
            });
        } else {
            let error_status = if status == 0 { -1 } else { status };
            self.emit_connection_error(
                pending.connection(),
                "complete RFCOMM async write",
                error_status,
            );
        }
        match action {
            Ok(action) => self.apply_lifecycle_action(action),
            Err(_) => self.emit_fatal("complete RFCOMM write lifecycle", -1),
        }
        if !successful {
            self.begin_session_close(pending.connection());
        }
    }
}
