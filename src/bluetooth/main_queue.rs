use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use objc2::MainThreadMarker;
use objc2::rc::{Retained as ObjcRetained, autoreleasepool};
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, kCFRunLoopCommonModes,
};
use objc2_io_bluetooth::IOBluetoothHostController;
use tokio::sync::mpsc;

use super::actor::BluetoothActor;
use super::types::{BtCommand, BtEvent, DeviceAddress};

pub(super) struct CommandBuffer {
    commands: VecDeque<BtCommand>,
    active: bool,
}

impl CommandBuffer {
    pub(super) fn new() -> Self {
        Self {
            commands: VecDeque::new(),
            active: true,
        }
    }

    pub(super) fn push(&mut self, command: BtCommand) -> Result<()> {
        if !self.active {
            bail!("Bluetooth main-thread command queue is closed");
        }
        self.commands.push_back(command);
        Ok(())
    }

    pub(super) fn drain(&mut self) -> Vec<BtCommand> {
        self.commands.drain(..).collect()
    }

    pub(super) fn close(&mut self) {
        self.active = false;
        self.commands.clear();
    }
}

struct QueueState {
    buffer: CommandBuffer,
    source_address: usize,
    run_loop_address: usize,
}

#[derive(Clone)]
pub struct MainThreadCommandQueue {
    state: Arc<Mutex<QueueState>>,
}

impl MainThreadCommandQueue {
    /// Enqueue a command and wake the macOS main run loop.
    ///
    /// # Errors
    ///
    /// Returns an error after native shutdown starts or if the queue mutex was
    /// poisoned by a panic.
    pub fn submit(&self, command: BtCommand) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bluetooth command queue mutex poisoned"))?;
        state.buffer.push(command)?;

        // SAFETY: BluetoothMainThread installs retained CF objects before it
        // exposes this handle. It invalidates them only while holding the same
        // mutex after marking the buffer inactive. Core Foundation documents
        // CFRunLoopSourceSignal and CFRunLoopWakeUp as callable cross-thread.
        unsafe {
            let source = &*(state.source_address as *const CFRunLoopSource);
            let run_loop = &*(state.run_loop_address as *const CFRunLoop);
            source.signal();
            run_loop.wake_up();
        }
        Ok(())
    }
}

struct SourceContext {
    actor: ObjcRetained<BluetoothActor>,
    queue: Arc<Mutex<QueueState>>,
}

pub struct BluetoothMainThread {
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
    context: Box<SourceContext>,
    queue: MainThreadCommandQueue,
    local_address: DeviceAddress,
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
        let actor = BluetoothActor::new(event_sender, mtm);
        let channel_id = actor.publish().context("publish Bluetooth SPP service")?;
        let queue_state = Arc::new(Mutex::new(QueueState {
            buffer: CommandBuffer::new(),
            source_address: 0,
            run_loop_address: 0,
        }));
        let mut context = Box::new(SourceContext {
            actor,
            queue: Arc::clone(&queue_state),
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
        let run_loop = CFRunLoop::main().context("obtain macOS main CFRunLoop")?;
        // SAFETY: This process is running on macOS and Core Foundation exports
        // the standard common-modes constant for the lifetime of the process.
        let common_modes =
            unsafe { kCFRunLoopCommonModes }.context("obtain kCFRunLoopCommonModes")?;
        run_loop.add_source(Some(&source), Some(common_modes));

        {
            let mut state = queue_state
                .lock()
                .map_err(|_| anyhow::anyhow!("Bluetooth command queue mutex poisoned"))?;
            state.source_address = std::ptr::from_ref(&*source) as usize;
            state.run_loop_address = std::ptr::from_ref(&*run_loop) as usize;
        }

        Ok(Self {
            source,
            run_loop,
            context,
            queue: MainThreadCommandQueue { state: queue_state },
            local_address,
            channel_id,
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
        if let Ok(mut state) = self.queue.state.lock() {
            state.buffer.close();
        }
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
    let commands = match context.queue.lock() {
        Ok(mut state) => state.buffer.drain(),
        Err(_) => return,
    };

    for command in commands {
        if command == BtCommand::Shutdown {
            if let Ok(mut state) = context.queue.lock() {
                state.buffer.close();
            }
            autoreleasepool(|_| context.actor.shutdown());
            break;
        }
        autoreleasepool(|_| context.actor.handle(command));
    }
}
