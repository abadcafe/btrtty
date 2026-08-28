use std::ffi::c_void;
use std::time::Duration;

use anyhow::{Context, Result};
use objc2::MainThreadMarker;
use objc2::rc::{Retained as ObjcRetained, autoreleasepool};
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext,
    CFRunLoopTimer, CFRunLoopTimerContext,
};
use objc2_io_bluetooth::IOBluetoothHostController;
use tokio::sync::mpsc;

use super::actor::BluetoothActor;
use super::dispatch::{DispatchItem, Dispatcher, MainThreadCommandQueue};
use super::types::{BtCommand, BtEvent, BtInput, DeviceAddress};

mod callbacks;
#[cfg(test)]
mod callbacks_tests;

use callbacks::RunLoopCallbacks;

struct SourceContext {
    actor: ObjcRetained<BluetoothActor>,
    dispatcher: Dispatcher,
    listener_timer: Option<CFRetained<CFRunLoopTimer>>,
}

const LISTENER_DORMANT_INTERVAL: Duration = Duration::from_hours(24);

impl SourceContext {
    fn schedule_listener_timer(&self) {
        let Some(timer) = self.listener_timer.as_ref() else {
            return;
        };
        let delay = if self.actor.listener_refresh_due() {
            super::actor::LISTENER_REFRESH_INTERVAL
        } else {
            LISTENER_DORMANT_INTERVAL
        };
        timer.set_next_fire_date(CFAbsoluteTimeGetCurrent() + delay.as_secs_f64());
    }
}

pub struct BluetoothMainThread {
    callbacks: RunLoopCallbacks,
    context: Box<SourceContext>,
    queue: MainThreadCommandQueue,
    local_address: DeviceAddress,
    service_record_handle: u32,
    channel_id: u8,
}

impl BluetoothMainThread {
    /// Publish the SPP service and install the event-driven command source.
    ///
    /// # Errors
    ///
    /// Returns an error if SDP publication, RFCOMM notification registration,
    /// or `CFRunLoop` source installation fails.
    pub fn start(
        event_sender: mpsc::UnboundedSender<BtEvent>,
        input_sender: mpsc::Sender<BtInput>,
        mtm: MainThreadMarker,
    ) -> Result<Self> {
        // SAFETY: This runs on the macOS main thread and only reads the active
        // host controller's immutable address.
        let controller = unsafe { IOBluetoothHostController::defaultController() }
            .context("obtain default Bluetooth controller")?;
        // SAFETY: `controller` is retained for this call and returns an owned
        // NSString when the active controller has a readable address.
        let address = unsafe { controller.addressAsString() }
            .context("read local Bluetooth controller address")?;
        let local_address = DeviceAddress::new(&address.to_string())
            .context("parse local Bluetooth controller address")?;
        let dispatcher = Dispatcher::new();
        let actor = BluetoothActor::new(event_sender, input_sender, dispatcher.native_queue(), mtm);
        let mut context = Box::new(SourceContext {
            actor,
            dispatcher,
            listener_timer: None,
        });
        let mut source_context = CFRunLoopSourceContext {
            version: 0,
            info: std::ptr::from_mut(&mut *context).cast::<c_void>(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(perform_commands),
        };
        // SAFETY: `context` is boxed at a stable address and outlives `source`.
        // The source is invalidated before the box is dropped.
        let source =
            unsafe { CFRunLoopSource::new(None, 0, std::ptr::from_mut(&mut source_context)) }
                .context("create CFRunLoop command source")?;
        let mut timer_context = CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::from_mut(&mut *context).cast::<c_void>(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: `context` has a stable boxed address and outlives the timer.
        // The timer is removed and invalidated before that box is dropped.
        let listener_timer = unsafe {
            CFRunLoopTimer::new(
                None,
                CFAbsoluteTimeGetCurrent() + super::actor::LISTENER_REFRESH_INTERVAL.as_secs_f64(),
                LISTENER_DORMANT_INTERVAL.as_secs_f64(),
                0,
                0,
                Some(refresh_listener),
                std::ptr::from_mut(&mut timer_context),
            )
        }
        .context("create RFCOMM listener refresh timer")?;
        context.listener_timer = Some(listener_timer.clone());
        let run_loop = CFRunLoop::main().context("obtain macOS main CFRunLoop")?;
        let mut callbacks = RunLoopCallbacks::new(source, listener_timer, run_loop)?;

        context
            .dispatcher
            .install(callbacks.source(), callbacks.run_loop())?;

        // Publication is the final fallible startup operation. The command
        // source and its stable context already exist, but the source is not
        // installed in the run loop until publication and incoming-listener
        // registration both succeed.
        let endpoint = context
            .actor
            .publish()
            .context("publish Bluetooth SPP service")?;
        callbacks.activate();
        context.schedule_listener_timer();

        let queue = context.dispatcher.command_queue();
        Ok(Self {
            callbacks,
            context,
            queue,
            local_address,
            service_record_handle: endpoint.record_handle,
            channel_id: endpoint.channel_id,
        })
    }

    #[must_use]
    pub fn command_queue(&self) -> MainThreadCommandQueue {
        self.queue.clone()
    }

    #[must_use]
    pub fn local_address(&self) -> &DeviceAddress {
        &self.local_address
    }

    #[must_use]
    pub const fn channel_id(&self) -> u8 {
        self.channel_id
    }

    #[must_use]
    pub const fn service_record_handle(&self) -> u32 {
        self.service_record_handle
    }

    /// Run the native event loop until the actor's strict shutdown barrier is
    /// complete.
    ///
    /// # Errors
    ///
    /// Returns an error if native teardown ended with a listener, service,
    /// channel, or asynchronous write still alive.
    pub fn run(mut self) -> Result<()> {
        CFRunLoop::run();
        self.teardown()
    }

    fn teardown(&mut self) -> Result<()> {
        self.context.dispatcher.deactivate();
        self.callbacks.detach();
        self.context.listener_timer = None;
        self.context.actor.finish()
    }
}

impl Drop for BluetoothMainThread {
    fn drop(&mut self) {
        self.context.dispatcher.deactivate();
        self.callbacks.detach();
        self.context.listener_timer = None;
    }
}

unsafe extern "C-unwind" fn perform_commands(info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: `info` points at BluetoothMainThread's boxed SourceContext,
    // which outlives and is not moved while the source is registered.
    let context = unsafe { &*(info.cast::<SourceContext>()) };
    let Ok(items) = context.dispatcher.drain() else {
        context
            .actor
            .dispatch_failed("drain Bluetooth main-thread dispatch queue");
        return;
    };

    let mut accepts_commands = true;
    for item in items {
        match item {
            DispatchItem::Command(BtCommand::Shutdown) if accepts_commands => {
                accepts_commands = false;
                if context.dispatcher.close_commands().is_err() {
                    context
                        .actor
                        .dispatch_failed("close Bluetooth main-thread command queue");
                    return;
                }
                autoreleasepool(|_| context.actor.handle(BtCommand::Shutdown));
            }
            DispatchItem::Command(command) if accepts_commands => {
                autoreleasepool(|_| context.actor.handle(command));
            }
            DispatchItem::Command(_) => {}
            DispatchItem::Native(action) => {
                autoreleasepool(|_| context.actor.handle_native(action));
            }
        }
    }
    context.schedule_listener_timer();
}

unsafe extern "C-unwind" fn refresh_listener(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: `info` points at BluetoothMainThread's boxed SourceContext,
    // which is stable until the timer is removed and invalidated.
    let context = unsafe { &*(info.cast::<SourceContext>()) };
    autoreleasepool(|_| context.actor.refresh_incoming_listener());
    context.schedule_listener_timer();
}
