use anyhow::{Result, bail};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bluetooth::{RFCOMM_WRITE_WINDOW, WriteToken};

static NEXT_WRITE_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(crate) struct TxWrite {
    pub(crate) token: WriteToken,
    pub(crate) data: Bytes,
}

pub(crate) struct TxWindow {
    mtu: usize,
    chunk: Option<Bytes>,
    offset: usize,
    outstanding: HashMap<WriteToken, usize>,
}

impl TxWindow {
    pub(crate) fn new(mtu: usize) -> Result<Self> {
        if mtu == 0 || mtu > usize::from(u16::MAX) {
            bail!("invalid RFCOMM MTU {mtu}");
        }
        Ok(Self {
            mtu,
            chunk: None,
            offset: 0,
            outstanding: HashMap::new(),
        })
    }

    pub(crate) fn is_ready_for_chunk(&self) -> bool {
        self.chunk.is_none() && self.outstanding.len() < RFCOMM_WRITE_WINDOW
    }

    pub(crate) fn is_drained(&self) -> bool {
        self.chunk.is_none() && self.outstanding.is_empty()
    }

    pub(crate) fn enqueue(&mut self, chunk: Bytes) -> Result<()> {
        if chunk.is_empty() {
            bail!("cannot enqueue an empty PTY output chunk");
        }
        if self.chunk.is_some() {
            bail!("RFCOMM transmit window already owns a PTY chunk");
        }
        self.chunk = Some(chunk);
        self.offset = 0;
        Ok(())
    }

    pub(crate) fn start_write(&mut self) -> Option<TxWrite> {
        if self.outstanding.len() >= RFCOMM_WRITE_WINDOW {
            return None;
        }
        let chunk = self.chunk.as_ref()?;
        let end = (self.offset + self.mtu).min(chunk.len());
        let data = chunk.slice(self.offset..end);
        let token = next_write_token();
        self.outstanding.insert(token, data.len());
        if end == chunk.len() {
            self.chunk = None;
            self.offset = 0;
        } else {
            self.offset = end;
        }
        Some(TxWrite { token, data })
    }

    pub(crate) fn complete(&mut self, token: WriteToken, bytes_written: usize) -> Result<()> {
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
