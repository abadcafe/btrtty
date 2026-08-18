use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::time::Instant;

const KIB: u64 = 1024;
const MIN_RATE_KIB_PER_SECOND: u16 = 1;
const MAX_RATE_KIB_PER_SECOND: u16 = 200;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const MILLIS_PER_SECOND: u64 = 1_000;
const STAGING_TARGET_MILLIS: u64 = 100;
const INFLIGHT_TARGET_MILLIS: u64 = 25;

/// Validated maximum rate for bytes sent from the Mac to the Bluetooth client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputRate {
    kib_per_second: u16,
}

impl OutputRate {
    /// Construct a rate in KiB per second.
    ///
    /// # Errors
    ///
    /// Returns an error unless `kib_per_second` is in `1..=200`.
    pub fn new(kib_per_second: u16) -> Result<Self> {
        if !(MIN_RATE_KIB_PER_SECOND..=MAX_RATE_KIB_PER_SECOND).contains(&kib_per_second) {
            bail!(
                "maximum output rate must be between {MIN_RATE_KIB_PER_SECOND} and {MAX_RATE_KIB_PER_SECOND} KiB/s"
            );
        }
        Ok(Self { kib_per_second })
    }

    #[must_use]
    pub const fn kib_per_second(self) -> u16 {
        self.kib_per_second
    }

    const fn bytes_per_second(self) -> u64 {
        self.kib_per_second as u64 * KIB
    }
}

/// Strict process-lifetime output pacer shared by successive sessions.
pub struct EgressLimiter {
    bytes_per_second: u64,
    next_send_at: Instant,
}

impl EgressLimiter {
    #[must_use]
    pub fn new(rate: OutputRate) -> Self {
        Self {
            bytes_per_second: rate.bytes_per_second(),
            next_send_at: Instant::now(),
        }
    }

    /// Bound unsent PTY output to approximately 100 ms of configured traffic.
    pub(crate) fn staging_budget(&self, mtu: usize) -> Result<usize> {
        if mtu == 0 {
            bail!("RFCOMM MTU must be non-zero");
        }
        Ok(self.byte_budget(STAGING_TARGET_MILLIS)?.max(mtu))
    }

    /// Derive the write window from 25 ms of configured traffic and the MTU.
    pub(crate) fn write_window(&self, mtu: usize, maximum: usize) -> Result<usize> {
        if mtu == 0 {
            bail!("RFCOMM MTU must be non-zero");
        }
        if maximum == 0 {
            bail!("RFCOMM maximum write window must be non-zero");
        }
        let inflight_bytes = self.byte_budget(INFLIGHT_TARGET_MILLIS)?.max(mtu);
        Ok(inflight_bytes.div_ceil(mtu).clamp(1, maximum))
    }

    #[must_use]
    pub(crate) fn ready_at(&self, now: Instant) -> Instant {
        self.next_send_at.max(now)
    }

    /// Record a write at its actual submission time.
    ///
    /// This deliberately discards timer lateness and idle time, so neither can
    /// turn into accumulated burst credit.
    pub(crate) fn record_sent_at(&mut self, byte_count: usize, now: Instant) -> Result<()> {
        if byte_count == 0 {
            bail!("cannot record an empty output write");
        }
        if now < self.next_send_at {
            bail!("output write submitted before its strict pacing deadline");
        }

        let byte_count = u128::try_from(byte_count).context("output byte count exceeds u128")?;
        let duration_nanos = byte_count
            .checked_mul(NANOS_PER_SECOND)
            .context("output pacing duration overflow")?
            .div_ceil(u128::from(self.bytes_per_second));
        let duration_nanos =
            u64::try_from(duration_nanos).context("output pacing duration exceeds u64")?;
        self.next_send_at = now
            .checked_add(Duration::from_nanos(duration_nanos))
            .context("output rate deadline exceeds Tokio Instant")?;
        Ok(())
    }

    fn byte_budget(&self, milliseconds: u64) -> Result<usize> {
        let numerator = self
            .bytes_per_second
            .checked_mul(milliseconds)
            .context("output byte budget overflow")?;
        usize::try_from(numerator.div_ceil(MILLIS_PER_SECOND))
            .context("output byte budget exceeds usize")
    }
}
