use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use bytes::Bytes;
use thiserror::Error;

pub(crate) const MAX_RFCOMM_WRITE_COMMANDS: usize = 32;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DeviceAddress(String);

impl DeviceAddress {
    /// Parse and normalize a Bluetooth Classic MAC address.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceAddressError`] unless the value contains six
    /// colon- or hyphen-separated hexadecimal octets.
    pub fn new(value: &str) -> Result<Self, DeviceAddressError> {
        value.parse()
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for DeviceAddress {
    type Err = DeviceAddressError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.replace('-', ":").to_ascii_uppercase();
        let groups: Vec<_> = normalized.split(':').collect();
        let valid = groups.len() == 6
            && groups.iter().all(|group| {
                group.len() == 2 && group.bytes().all(|byte| byte.is_ascii_hexdigit())
            });

        if !valid {
            return Err(DeviceAddressError(value.to_owned()));
        }

        Ok(Self(normalized))
    }
}

#[derive(Debug, Error)]
#[error("invalid Bluetooth address `{0}` (expected AA:BB:CC:DD:EE:FF)")]
pub struct DeviceAddressError(String);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConnectionId(NonZeroU64);

impl ConnectionId {
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match Self::new(self.get().wrapping_add(1)) {
            Some(next) => next,
            None => Self(NonZeroU64::MIN),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WriteToken(NonZeroU64);

impl WriteToken {
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match Self::new(self.get().wrapping_add(1)) {
            Some(next) => next,
            None => Self(NonZeroU64::MIN),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BtEvent {
    Connected {
        connection: ConnectionId,
        device: DeviceAddress,
        mtu: usize,
        paired: bool,
    },
    WriteComplete {
        connection: ConnectionId,
        token: WriteToken,
        bytes_written: usize,
        status: i32,
    },
    ConnectionError {
        connection: ConnectionId,
        error: BtError,
    },
    Disconnected {
        connection: ConnectionId,
    },
    Fatal(BtError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BtInput {
    pub connection: ConnectionId,
    pub data: Bytes,
}

impl BtInput {
    #[must_use]
    pub fn new(connection: ConnectionId, data: Bytes) -> Self {
        Self { connection, data }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BtCommand {
    AcceptNext {
        after: ConnectionId,
    },
    Send {
        connection: ConnectionId,
        token: WriteToken,
        data: Bytes,
    },
    Close {
        connection: ConnectionId,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("Bluetooth {operation} failed with IOReturn {status:#x}")]
pub struct BtError {
    operation: &'static str,
    status: i32,
}

impl BtError {
    #[must_use]
    pub const fn new(operation: &'static str, status: i32) -> Self {
        Self { operation, status }
    }

    #[must_use]
    pub const fn status(&self) -> i32 {
        self.status
    }
}
