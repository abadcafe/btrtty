mod actor;
mod dispatch;
mod lifecycle;
mod main_queue;
mod sdp;
mod types;

pub const INPUT_QUEUE_CAPACITY: usize = 128;

pub use dispatch::MainThreadCommandQueue;
pub use main_queue::BluetoothMainThread;
pub(crate) use types::MAX_RFCOMM_WRITE_COMMANDS;
pub use types::{BtCommand, BtError, BtEvent, BtInput, ConnectionId, DeviceAddress, WriteToken};

#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod sdp_tests;
#[cfg(test)]
mod types_tests;
