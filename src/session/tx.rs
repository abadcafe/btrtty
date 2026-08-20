use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use bytes::{Bytes, BytesMut};

use crate::bluetooth::{MAX_RFCOMM_WRITE_COMMANDS, WriteToken};

static NEXT_WRITE_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(super) struct TxWrite {
    pub(super) token: WriteToken,
    pub(super) data: Bytes,
}

/// A bounded byte-stream staging area with MTU packetization and write ownership.
pub(super) struct Transmitter {
    mtu: usize,
    staging: BytesMut,
    outstanding: HashMap<WriteToken, usize>,
}

impl Transmitter {
    pub(super) fn new(mtu: usize) -> Result<Self> {
        if mtu == 0 || mtu > usize::from(u16::MAX) {
            bail!("invalid RFCOMM MTU {mtu}");
        }
        Ok(Self {
            mtu,
            staging: BytesMut::with_capacity(mtu),
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
        self.mtu - self.staging.len()
    }

    #[must_use]
    pub(super) fn window_full(&self) -> bool {
        self.outstanding.len() >= MAX_RFCOMM_WRITE_COMMANDS
    }

    #[must_use]
    pub(super) fn is_drained(&self) -> bool {
        self.staging.is_empty() && self.outstanding.is_empty()
    }

    pub(super) fn start_write(&mut self) -> Option<TxWrite> {
        if self.window_full() || self.staging.is_empty() {
            return None;
        }
        let length = self.mtu.min(self.staging.len());
        let data = self.staging.split_to(length).freeze();
        let token = next_write_token();
        self.outstanding.insert(token, length);
        Some(TxWrite { token, data })
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
