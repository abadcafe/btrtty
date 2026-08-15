use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::time::Instant;

const KIB: u64 = 1024;
const MIN_RATE_KIB_PER_SECOND: u16 = 1;
const MAX_RATE_KIB_PER_SECOND: u16 = 200;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const MILLIS_PER_SECOND: u64 = 1_000;
const QUEUE_TARGET_MILLIS: u64 = 100;

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

    fn bytes_per_second(self) -> u64 {
        u64::from(self.kib_per_second) * KIB
    }
}

/// Process-lifetime output pacer shared by successive Bluetooth sessions.
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

    /// Size the bounded PTY output queue for approximately 100 ms of data.
    pub(crate) fn queue_capacity(&self, chunk_size: usize) -> Result<usize> {
        if chunk_size == 0 {
            bail!("PTY output chunk size must be non-zero");
        }

        let chunk_size = u64::try_from(chunk_size).context("PTY output chunk size exceeds u64")?;
        let target_bytes_numerator = self
            .bytes_per_second
            .checked_mul(QUEUE_TARGET_MILLIS)
            .context("PTY output queue size overflow")?;
        let divisor = MILLIS_PER_SECOND
            .checked_mul(chunk_size)
            .context("PTY output queue divisor overflow")?;
        let chunks = target_bytes_numerator.div_ceil(divisor).max(1);
        usize::try_from(chunks).context("PTY output queue capacity exceeds usize")
    }

    pub(crate) fn reserve(&mut self, byte_count: usize) -> Result<Instant> {
        self.reserve_at(byte_count, Instant::now())
    }

    pub(crate) fn reserve_at(&mut self, byte_count: usize, now: Instant) -> Result<Instant> {
        if byte_count == 0 {
            bail!("cannot reserve zero output bytes");
        }

        let deadline = self.next_send_at.max(now);
        let byte_count = u128::try_from(byte_count).context("output byte count exceeds u128")?;
        let duration_nanos = byte_count
            .checked_mul(NANOS_PER_SECOND)
            .context("output pacing duration overflow")?
            .div_ceil(u128::from(self.bytes_per_second));
        let duration_nanos =
            u64::try_from(duration_nanos).context("output pacing duration exceeds u64")?;
        self.next_send_at = deadline
            .checked_add(Duration::from_nanos(duration_nanos))
            .context("output rate deadline exceeds Tokio Instant")?;
        Ok(deadline)
    }
}
