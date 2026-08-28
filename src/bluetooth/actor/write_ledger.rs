use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;

use anyhow::{Result, bail};
use bytes::Bytes;

use crate::bluetooth::types::{ConnectionId, MAX_RFCOMM_WRITE_COMMANDS, WriteToken};

pub(super) struct PendingWrite {
    connection: ConnectionId,
    token: WriteToken,
    data: Bytes,
}

impl PendingWrite {
    pub(super) fn new(connection: ConnectionId, token: WriteToken, data: Bytes) -> Self {
        Self {
            connection,
            token,
            data,
        }
    }

    pub(super) const fn connection(&self) -> ConnectionId {
        self.connection
    }

    pub(super) const fn token(&self) -> WriteToken {
        self.token
    }

    pub(super) const fn data(&self) -> &Bytes {
        &self.data
    }
}

pub(super) struct AdmittedWrite(PendingWrite);

impl AdmittedWrite {
    pub(super) const fn connection(&self) -> ConnectionId {
        self.0.connection()
    }

    pub(super) const fn token(&self) -> WriteToken {
        self.0.token()
    }
}

pub(super) struct NativeWrite {
    connection: ConnectionId,
    token: WriteToken,
    pointer: *mut c_void,
    length: u16,
}

impl NativeWrite {
    pub(super) const fn connection(&self) -> ConnectionId {
        self.connection
    }

    pub(super) const fn token(&self) -> WriteToken {
        self.token
    }

    pub(super) const fn pointer(&self) -> *mut c_void {
        self.pointer
    }

    pub(super) const fn length(&self) -> u16 {
        self.length
    }
}

pub(super) enum Completion {
    Current(PendingWrite),
    Retired,
    Queued,
    Unknown,
}

/// Owns every RFCOMM output buffer and enforces process-wide monotonic token
/// admission without retaining completed tokens.
pub(super) struct WriteLedger {
    queued: VecDeque<AdmittedWrite>,
    pending: HashMap<WriteToken, PendingWrite>,
    last_admitted: Option<WriteToken>,
}

impl WriteLedger {
    pub(super) fn new() -> Self {
        Self {
            queued: VecDeque::new(),
            pending: HashMap::new(),
            last_admitted: None,
        }
    }

    pub(super) fn admit(&mut self, write: PendingWrite) -> Result<AdmittedWrite> {
        if self.len() >= MAX_RFCOMM_WRITE_COMMANDS {
            bail!("RFCOMM write command exceeded its bounded window");
        }
        if self
            .last_admitted
            .is_some_and(|last| write.token().get() <= last.get())
        {
            bail!("RFCOMM write token is duplicate or non-monotonic");
        }
        self.last_admitted = Some(write.token());
        Ok(AdmittedWrite(write))
    }

    pub(super) fn queue(&mut self, write: AdmittedWrite) {
        self.queued.push_back(write);
    }

    pub(super) fn pop_queued(&mut self) -> Option<AdmittedWrite> {
        self.queued.pop_front()
    }

    pub(super) fn push_front(&mut self, write: AdmittedWrite) {
        self.queued.push_front(write);
    }

    pub(super) fn start(&mut self, write: AdmittedWrite) -> NativeWrite {
        let AdmittedWrite(write) = write;
        let native = NativeWrite {
            connection: write.connection(),
            token: write.token(),
            pointer: write.data().as_ptr().cast_mut().cast::<c_void>(),
            length: u16::try_from(write.data().len()).expect("write length was validated"),
        };
        let replaced = self.pending.insert(write.token(), write);
        assert!(replaced.is_none(), "admitted write token must be unique");
        native
    }

    pub(super) fn fail(&mut self, token: WriteToken) -> Option<PendingWrite> {
        self.pending.remove(&token)
    }

    pub(super) fn complete(&mut self, token: WriteToken) -> Completion {
        if let Some(write) = self.pending.remove(&token) {
            return Completion::Current(write);
        }
        if self.queued.iter().any(|write| write.0.token() == token) {
            return Completion::Queued;
        }
        if self
            .last_admitted
            .is_some_and(|last| token.get() <= last.get())
        {
            Completion::Retired
        } else {
            Completion::Unknown
        }
    }

    pub(super) fn clear_queued(&mut self) {
        self.queued.clear();
    }

    pub(super) fn len(&self) -> usize {
        self.queued.len() + self.pending.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.pending.is_empty()
    }
}
