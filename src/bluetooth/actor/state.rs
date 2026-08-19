use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use objc2::rc::Retained;
use objc2_io_bluetooth::{
    IOBluetoothRFCOMMChannel, IOBluetoothSDPServiceRecord, IOBluetoothUserNotification,
};

use crate::bluetooth::lifecycle::Lifecycle;
use crate::bluetooth::types::{BtError, ConnectionId, WriteToken};

use super::acceptance::AcceptanceGate;

pub(super) struct PendingWrite {
    pub(super) connection: ConnectionId,
    pub(super) token: WriteToken,
    pub(super) data: Bytes,
}

pub(super) struct ActiveChannel {
    pub(super) connection: ConnectionId,
    pub(super) channel: Retained<IOBluetoothRFCOMMChannel>,
    pub(super) close_notification: Option<Retained<IOBluetoothUserNotification>>,
    pub(super) close_observed: bool,
    pub(super) input: InputState,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum InputState {
    Open,
    Closed,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum DelegateState {
    Installed,
    Missing,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum DeliveryState {
    Ready,
    InvalidListener,
    InputOverflow,
    Closed,
}

pub(super) struct DeliveredChannel {
    pub(super) connection: ConnectionId,
    pub(super) channel: Retained<IOBluetoothRFCOMMChannel>,
    pub(super) close_notification: Option<Retained<IOBluetoothUserNotification>>,
    pub(super) delegate: DelegateState,
    pub(super) state: DeliveryState,
}

pub(super) struct RejectedChannel {
    pub(super) _channel: Retained<IOBluetoothRFCOMMChannel>,
    pub(super) close_notification: Option<Retained<IOBluetoothUserNotification>>,
}

pub(super) struct ActorState {
    pub(super) service_record: Option<Retained<IOBluetoothSDPServiceRecord>>,
    pub(super) incoming_notification: Option<Retained<IOBluetoothUserNotification>>,
    pub(super) delivered: HashMap<usize, DeliveredChannel>,
    pub(super) active: Option<ActiveChannel>,
    pub(super) rejected: HashMap<usize, RejectedChannel>,
    pub(super) queued_writes: VecDeque<PendingWrite>,
    pub(super) pending_writes: HashMap<WriteToken, PendingWrite>,
    pub(super) retired_writes: std::collections::HashSet<WriteToken>,
    pub(super) lifecycle: Lifecycle,
    pub(super) next_connection: ConnectionId,
    pub(super) acceptance: AcceptanceGate,
    pub(super) fatal_error: Option<BtError>,
}

impl Default for ActorState {
    fn default() -> Self {
        Self {
            service_record: None,
            incoming_notification: None,
            delivered: HashMap::new(),
            active: None,
            rejected: HashMap::new(),
            queued_writes: VecDeque::new(),
            pending_writes: HashMap::new(),
            retired_writes: std::collections::HashSet::new(),
            lifecycle: Lifecycle::new(),
            next_connection: ConnectionId::new(1).expect("one is a non-zero connection ID"),
            acceptance: AcceptanceGate::new(),
            fatal_error: None,
        }
    }
}
