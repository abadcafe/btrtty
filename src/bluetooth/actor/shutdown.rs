use objc2::DefinedClass;
use objc2_core_foundation::CFRunLoop;

use super::BluetoothActor;
use super::state::InputState;
use crate::bluetooth::dispatch::NativeAction;
use crate::bluetooth::lifecycle::LifecycleAction;
use crate::bluetooth::types::{BtEvent, ConnectionId};

impl BluetoothActor {
    pub(super) fn begin_shutdown(&self) {
        let action = {
            let mut state = self.ivars().state.borrow_mut();
            state.acceptance.stop();
            state.queued_write = None;
            if let Some(active) = state.active.as_mut() {
                active.input = InputState::Closed;
            }
            state.lifecycle.begin_shutdown()
        };
        match action {
            Ok(action) => self.apply_lifecycle_action(action),
            Err(_) => self.emit_fatal("begin Bluetooth shutdown", -1),
        }
    }

    pub(super) fn apply_lifecycle_action(&self, action: LifecycleAction) {
        match action {
            LifecycleAction::Wait => {}
            LifecycleAction::CloseChannel => self.submit_channel_close(),
            LifecycleAction::NotifyDisconnected => match self.release_closed_active() {
                Ok(connection) => {
                    let _ = self
                        .ivars()
                        .events
                        .send(BtEvent::Disconnected { connection });
                }
                Err(error) => self.emit_fatal(error, -1),
            },
            LifecycleAction::WithdrawService => {
                if self.ivars().state.borrow().active.is_some()
                    && let Err(error) = self.release_closed_active()
                {
                    self.emit_fatal(error, -1);
                    return;
                }
                self.defer(NativeAction::WithdrawService);
            }
            LifecycleAction::StopRunLoop => Self::stop_run_loop(),
        }
    }

    fn submit_channel_close(&self) {
        let channel = {
            let state = self.ivars().state.borrow();
            if !state.pending_writes.is_empty() {
                drop(state);
                self.emit_fatal("close RFCOMM channel with write outstanding", -1);
                return;
            }
            let Some(active) = state.active.as_ref() else {
                drop(state);
                self.emit_fatal("close missing RFCOMM channel", -1);
                return;
            };
            if active.close_observed {
                drop(state);
                self.emit_fatal("close already-observed RFCOMM channel", -1);
                return;
            }
            active.channel.clone()
        };
        // SAFETY: The actor retains the channel until its close action.
        let status = unsafe { channel.closeChannel() };
        if status != 0 {
            self.emit_fatal("close RFCOMM channel", status);
        }
    }

    pub(super) fn withdraw_service(&self) {
        let state = self.ivars().state.borrow();
        if !state.lifecycle.is_withdrawing() || !state.delivered.is_empty() {
            return;
        }
        let record = state.service_record.clone();
        drop(state);
        let Some(record) = record else {
            self.emit_fatal("remove missing SDP service", -1);
            return;
        };
        // SAFETY: The lifecycle permits withdrawal only after every known
        // channel close and write-completion action has been processed.
        let status = unsafe { record.removeServiceRecord() };
        if status != 0 {
            self.emit_fatal("remove SDP service", status);
            return;
        }

        let notification = {
            let mut state = self.ivars().state.borrow_mut();
            state.service_record = None;
            state.incoming_notification.take()
        };
        let Some(notification) = notification else {
            self.emit_fatal("unregister missing RFCOMM listener", -1);
            return;
        };
        // SAFETY: Service removal succeeded and the actor owns this live
        // incoming-channel registration.
        unsafe { notification.unregister() };
        let action = self
            .ivars()
            .state
            .borrow_mut()
            .lifecycle
            .service_withdrawn();
        match action {
            Ok(action) => self.apply_lifecycle_action(action),
            Err(_) => self.emit_fatal("complete Bluetooth service withdrawal", -1),
        }
    }

    fn release_closed_active(&self) -> Result<ConnectionId, &'static str> {
        let mut state = self.ivars().state.borrow_mut();
        let Some(active) = state.active.as_ref() else {
            return Err("release missing closed RFCOMM channel");
        };
        if !active.close_observed {
            return Err("release RFCOMM channel before close callback");
        }
        if state.queued_write.is_some() || !state.pending_writes.is_empty() {
            return Err("release RFCOMM channel before write completion");
        }
        let connection = active.connection;
        state.acceptance.connection_closed(connection)?;
        state.active = None;
        Ok(connection)
    }

    pub(super) fn stop_run_loop() {
        if let Some(run_loop) = CFRunLoop::main() {
            run_loop.stop();
        }
    }
}
