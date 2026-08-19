use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};

use crate::bluetooth::WriteToken;

static NEXT_WRITE_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(super) struct TxWrite {
    pub(super) token: WriteToken,
    pub(super) data: Bytes,
}

/// A bounded byte-stream staging area with MTU packetization and write ownership.
pub(super) struct Transmitter {
    mtu: usize,
    staging_limit: usize,
    write_window: usize,
    staging: BytesMut,
    outstanding: HashMap<WriteToken, usize>,
}

impl Transmitter {
    pub(super) fn new(mtu: usize, staging_limit: usize, write_window: usize) -> Result<Self> {
        if mtu == 0 || mtu > usize::from(u16::MAX) {
            bail!("invalid RFCOMM MTU {mtu}");
        }
        if staging_limit < mtu {
            bail!("PTY output staging limit must be at least one RFCOMM MTU");
        }
        if write_window == 0 {
            bail!("RFCOMM write window must be non-zero");
        }
        Ok(Self {
            mtu,
            staging_limit,
            write_window,
            staging: BytesMut::with_capacity(staging_limit),
            outstanding: HashMap::new(),
        })
    }

    pub(super) fn append(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            bail!("cannot stage empty PTY output");
        }
        if data.len() > self.remaining_capacity() {
            bail!("PTY output exceeds its bounded staging budget");
        }
        self.staging.extend_from_slice(data);
        Ok(())
    }

    #[must_use]
    pub(super) fn remaining_capacity(&self) -> usize {
        self.staging_limit - self.staging.len()
    }

    #[must_use]
    pub(super) fn has_buffered_data(&self) -> bool {
        !self.staging.is_empty()
    }

    #[must_use]
    pub(super) fn window_full(&self) -> bool {
        self.outstanding.len() >= self.write_window
    }

    #[must_use]
    pub(super) fn next_write_len(&self) -> Option<usize> {
        (!self.window_full() && !self.staging.is_empty()).then(|| self.mtu.min(self.staging.len()))
    }

    #[must_use]
    pub(super) fn is_drained(&self) -> bool {
        self.staging.is_empty() && self.outstanding.is_empty()
    }

    pub(super) fn start_write(&mut self, length: usize) -> Result<TxWrite> {
        let maximum = self
            .next_write_len()
            .context("transmitter cannot start an RFCOMM write")?;
        if length == 0 || length > maximum {
            bail!("invalid RFCOMM write length {length}; available maximum is {maximum}");
        }
        let data = self.staging.split_to(length).freeze();
        let token = next_write_token();
        self.outstanding.insert(token, length);
        Ok(TxWrite { token, data })
    }

    pub(super) fn complete(&mut self, token: WriteToken, bytes_written: usize) -> Result<()> {
        let expected = self
            .outstanding
            .get(&token)
            .ok_or_else(|| anyhow::anyhow!("unexpected RFCOMM write completion {token:?}"))?;
        if *expected != bytes_written {
            bail!(
                "RFCOMM completion length mismatch: expected {expected}, received {bytes_written}"
            );
        }
        self.outstanding.remove(&token);
        Ok(())
    }
}

fn next_write_token() -> WriteToken {
    loop {
        let value = NEXT_WRITE_TOKEN.fetch_add(1, Ordering::Relaxed);
        if let Some(token) = WriteToken::new(value) {
            return token;
        }
    }
}
