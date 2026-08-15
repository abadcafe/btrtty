use std::ffi::c_void;

use anyhow::{Context, Result};
use objc2::MainThreadMarker;
use objc2::rc::{Retained as ObjcRetained, autoreleasepool};
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, kCFRunLoopCommonModes,
};
use objc2_io_bluetooth::IOBluetoothHostController;
use tokio::sync::mpsc;

use super::actor::BluetoothActor;
use super::dispatch::{DispatchItem, Dispatcher, MainThreadCommandQueue};
use super::types::{BtCommand, BtEvent, BtInput, DeviceAddress};

struct SourceContext {
    actor: ObjcRetained<BluetoothActor>,
    dispatcher: Dispatcher,
}

pub struct BluetoothMainThread {
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
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
        let mut context = Box::new(SourceContext { actor, dispatcher });
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
        let run_loop = CFRunLoop::main().context("obtain macOS main CFRunLoop")?;
        // SAFETY: This process is running on macOS and Core Foundation exports
        // the standard common-modes constant for the lifetime of the process.
        let common_modes =
            unsafe { kCFRunLoopCommonModes }.context("obtain kCFRunLoopCommonModes")?;

        context.dispatcher.install(&source, &run_loop)?;

        // Publication is the final fallible startup operation. The command
        // source and its stable context already exist, but the source is not
        // installed in the run loop until publication and incoming-listener
        // registration both succeed.
        let endpoint = context
            .actor
            .publish()
            .context("publish Bluetooth SPP service")?;
        run_loop.add_source(Some(&source), Some(common_modes));

        let queue = context.dispatcher.command_queue();
        Ok(Self {
            source,
            run_loop,
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
        self.context.dispatcher.deactivate()?;
        // SAFETY: Same process-lifetime constant used when adding the source.
        if let Some(common_modes) = unsafe { kCFRunLoopCommonModes } {
            self.run_loop
                .remove_source(Some(&self.source), Some(common_modes));
        }
        self.source.invalidate();
        self.context.actor.finish()
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
}
