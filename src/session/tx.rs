use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Result, bail};
use bytes::{Bytes, BytesMut};

use crate::bluetooth::WriteToken;

static NEXT_WRITE_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(super) struct TxWrite {
    pub(super) token: WriteToken,
    pub(super) data: Bytes,
    pub(super) buffered_since: Instant,
}

struct BufferedSpan {
    remaining: usize,
    received_at: Instant,
}

/// A bounded byte-stream staging area with MTU packetization and write ownership.
pub(super) struct Transmitter {
    mtu: usize,
    staging_limit: usize,
    write_window: usize,
    staging: BytesMut,
    spans: VecDeque<BufferedSpan>,
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
            spans: VecDeque::new(),
            outstanding: HashMap::new(),
        })
    }

    pub(super) fn append(&mut self, data: &[u8], received_at: Instant) -> Result<()> {
        if data.is_empty() {
            bail!("cannot stage empty PTY output");
        }
        if data.len() > self.remaining_capacity() {
            bail!("PTY output exceeds its bounded staging budget");
        }
        self.staging.extend_from_slice(data);
        self.spans.push_back(BufferedSpan {
            remaining: data.len(),
            received_at,
        });
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
    pub(super) fn is_drained(&self) -> bool {
        self.staging.is_empty() && self.outstanding.is_empty()
    }

    pub(super) fn start_write(&mut self) -> Option<TxWrite> {
        if self.window_full() || self.staging.is_empty() {
            return None;
        }

        let length = self.mtu.min(self.staging.len());
        let buffered_since = self.spans.front()?.received_at;
        self.consume_spans(length);
        let data = self.staging.split_to(length).freeze();
        let token = next_write_token();
        self.outstanding.insert(token, length);
        Some(TxWrite {
            token,
            data,
            buffered_since,
        })
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

    fn consume_spans(&mut self, mut length: usize) {
        while length > 0 {
            let span = self
                .spans
                .front_mut()
                .expect("staged bytes always have timestamp spans");
            let consumed = length.min(span.remaining);
            span.remaining -= consumed;
            length -= consumed;
            if span.remaining == 0 {
                self.spans.pop_front();
            }
        }
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
