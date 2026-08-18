use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, bail};
use objc2_core_foundation::{CFRunLoop, CFRunLoopSource};

use super::types::BtCommand;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NativeAction {
    OpenDelivered {
        channel_key: usize,
    },
    ChannelClosed {
        channel_key: usize,
    },
    WriteCompleted {
        channel_key: usize,
        refcon: usize,
        status: i32,
        bytes_written: Option<usize>,
        observed_at: Instant,
    },
    FlowAvailable {
        channel_key: usize,
    },
    InputOverflow {
        channel_key: usize,
    },
    WithdrawService,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DispatchItem {
    Command(BtCommand),
    Native(NativeAction),
}

pub(super) struct DispatchBuffer {
    items: VecDeque<DispatchItem>,
    pending_flow: HashSet<usize>,
    pending_overflow: HashSet<usize>,
    accepts_commands: bool,
    active: bool,
}

impl DispatchBuffer {
    pub(super) fn new() -> Self {
        Self {
            items: VecDeque::new(),
            pending_flow: HashSet::new(),
            pending_overflow: HashSet::new(),
            accepts_commands: true,
            active: true,
        }
    }

    pub(super) fn push_command(&mut self, command: BtCommand) -> Result<()> {
        if !self.active || !self.accepts_commands {
            bail!("Bluetooth main-thread command queue is closed");
        }
        self.items.push_back(DispatchItem::Command(command));
        Ok(())
    }

    pub(super) fn push_native(&mut self, action: NativeAction) -> Result<()> {
        if !self.active {
            bail!("Bluetooth native-action queue is closed");
        }
        let inserted = match action {
            NativeAction::FlowAvailable { channel_key } => self.pending_flow.insert(channel_key),
            NativeAction::InputOverflow { channel_key } => {
                self.pending_overflow.insert(channel_key)
            }
            NativeAction::OpenDelivered { .. }
            | NativeAction::ChannelClosed { .. }
            | NativeAction::WriteCompleted { .. }
            | NativeAction::WithdrawService => true,
        };
        if !inserted {
            return Ok(());
        }
        self.items.push_back(DispatchItem::Native(action));
        Ok(())
    }

    pub(super) fn drain(&mut self) -> Vec<DispatchItem> {
        self.pending_flow.clear();
        self.pending_overflow.clear();
        self.items.drain(..).collect()
    }

    pub(super) fn close_commands(&mut self) {
        self.accepts_commands = false;
        self.items
            .retain(|item| matches!(item, DispatchItem::Native(_)));
    }

    pub(super) fn deactivate(&mut self) {
        self.active = false;
        self.accepts_commands = false;
        self.items.clear();
        self.pending_flow.clear();
        self.pending_overflow.clear();
    }
}

struct DispatchState {
    buffer: DispatchBuffer,
    source_address: usize,
    run_loop_address: usize,
}

#[derive(Clone)]
pub struct MainThreadCommandQueue {
    state: Arc<Mutex<DispatchState>>,
}

impl MainThreadCommandQueue {
    /// Enqueue a command and wake the macOS main run loop.
    ///
    /// # Errors
    ///
    /// Returns an error after native shutdown starts or if the queue cannot
    /// safely wake its installed run loop.
    pub fn submit(&self, command: BtCommand) -> Result<()> {
        submit(&self.state, DispatchItem::Command(command))
    }
}

#[derive(Clone)]
pub(super) struct NativeActionQueue {
    state: Arc<Mutex<DispatchState>>,
}

impl NativeActionQueue {
    pub(super) fn submit(&self, action: NativeAction) -> Result<()> {
        submit(&self.state, DispatchItem::Native(action))
    }
}

pub(super) struct Dispatcher {
    state: Arc<Mutex<DispatchState>>,
}

impl Dispatcher {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(DispatchState {
                buffer: DispatchBuffer::new(),
                source_address: 0,
                run_loop_address: 0,
            })),
        }
    }

    pub(super) fn install(&self, source: &CFRunLoopSource, run_loop: &CFRunLoop) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bluetooth dispatch queue mutex poisoned"))?;
        state.source_address = std::ptr::from_ref(source) as usize;
        state.run_loop_address = std::ptr::from_ref(run_loop) as usize;
        Ok(())
    }

    pub(super) fn command_queue(&self) -> MainThreadCommandQueue {
        MainThreadCommandQueue {
            state: Arc::clone(&self.state),
        }
    }

    pub(super) fn native_queue(&self) -> NativeActionQueue {
        NativeActionQueue {
            state: Arc::clone(&self.state),
        }
    }

    pub(super) fn drain(&self) -> Result<Vec<DispatchItem>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bluetooth dispatch queue mutex poisoned"))?;
        Ok(state.buffer.drain())
    }

    pub(super) fn close_commands(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bluetooth dispatch queue mutex poisoned"))?;
        state.buffer.close_commands();
        Ok(())
    }

    pub(super) fn deactivate(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bluetooth dispatch queue mutex poisoned"))?;
        state.buffer.deactivate();
        Ok(())
    }
}

fn submit(state: &Mutex<DispatchState>, item: DispatchItem) -> Result<()> {
    let mut state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("Bluetooth dispatch queue mutex poisoned"))?;
    match item {
        DispatchItem::Command(command) => state.buffer.push_command(command)?,
        DispatchItem::Native(action) => state.buffer.push_native(action)?,
    }
    if state.source_address == 0 || state.run_loop_address == 0 {
        bail!("Bluetooth dispatch queue is not installed");
    }

    // SAFETY: Dispatcher installs retained CF objects before callbacks or
    // command handles are exposed. Teardown deactivates the queue while
    // holding this mutex before invalidating either object.
    unsafe {
        let source = &*(state.source_address as *const CFRunLoopSource);
        let run_loop = &*(state.run_loop_address as *const CFRunLoop);
        source.signal();
        run_loop.wake_up();
    }
    Ok(())
}
