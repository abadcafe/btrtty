use anyhow::{Context, Result, bail};
use tokio::time::Instant;

const KIB: u64 = 1024;
const MIN_RATE_KIB_PER_SECOND: u16 = 1;
const MAX_RATE_KIB_PER_SECOND: u16 = 200;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const TIMER_GRANULARITY_NANOS: u128 = 1_000_000;
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

struct Bucket {
    capacity: u128,
    credit: u128,
    last_refill: Instant,
}

/// A single-use admission tied to the limiter that produced it.
pub(crate) struct EgressPermit<'a> {
    limiter: &'a mut EgressLimiter,
    byte_count: usize,
    remaining_credit: u128,
}

pub(crate) enum EgressDecision<'a> {
    Send(EgressPermit<'a>),
    WaitUntil(Instant),
}

/// Process-lifetime output limiter shared by successive RFCOMM sessions.
///
/// Credit uses byte-nanosecond units so fractional bytes survive every refill.
/// Idle credit is capped at one RFCOMM MTU. While output remains staged, at
/// most one timer tick of additional earned credit is retained. When less than
/// a full packet is available, the limiter sends the available prefix instead
/// of sleeping for the rest, so timer quantization cannot accumulate per
/// RFCOMM packet.
pub struct EgressLimiter {
    bytes_per_second: u64,
    bucket: Option<Bucket>,
}

impl EgressLimiter {
    #[must_use]
    pub fn new(rate: OutputRate) -> Self {
        Self {
            bytes_per_second: rate.bytes_per_second(),
            bucket: None,
        }
    }

    /// Configure the active RFCOMM MTU without granting reconnect credit.
    ///
    /// The first session starts with one MTU of credit. Later sessions retain
    /// credit accrued since the previous session, clamped to the new MTU.
    pub(crate) fn configure_mtu(&mut self, mtu: usize, now: Instant) -> Result<()> {
        let capacity = credit_for_bytes(validate_mtu(mtu)?)?;
        match self.bucket.as_mut() {
            None => {
                self.bucket = Some(Bucket {
                    capacity,
                    credit: capacity,
                    last_refill: now,
                });
            }
            Some(bucket) => {
                refill(bucket, self.bytes_per_second, now, bucket.capacity);
                bucket.capacity = capacity;
                bucket.credit = bucket.credit.min(capacity);
            }
        }
        Ok(())
    }

    /// Clamp earned credit to the idle one-MTU boundary before new data starts.
    pub(crate) fn begin_burst(&mut self, now: Instant) -> Result<()> {
        let bucket = self
            .bucket
            .as_mut()
            .context("RFCOMM MTU is not configured")?;
        refill(bucket, self.bytes_per_second, now, bucket.capacity);
        Ok(())
    }

    /// Bound unsent PTY output to approximately 100 ms of configured traffic.
    pub(crate) fn staging_budget(&self, mtu: usize) -> Result<usize> {
        if mtu == 0 {
            bail!("RFCOMM MTU must be non-zero");
        }
        Ok(self.byte_budget(STAGING_TARGET_MILLIS)?.max(mtu))
    }

    /// Derive the command window from 25 ms of traffic and the expected packet
    /// size produced by the one-millisecond pacing clock.
    pub(crate) fn write_window(&self, mtu: usize, maximum: usize) -> Result<usize> {
        if mtu == 0 {
            bail!("RFCOMM MTU must be non-zero");
        }
        if maximum == 0 {
            bail!("RFCOMM maximum write window must be non-zero");
        }
        let pacing_packet = usize::try_from(self.bytes_per_second / MILLIS_PER_SECOND)
            .context("output pacing packet exceeds usize")?
            .max(1)
            .min(mtu);
        let inflight_bytes = self.byte_budget(INFLIGHT_TARGET_MILLIS)?.max(pacing_packet);
        Ok(inflight_bytes.div_ceil(pacing_packet).clamp(1, maximum))
    }

    /// Admit up to `maximum_bytes` immediately, or return the deadline at which
    /// at least one byte can be admitted.
    pub(crate) fn reserve_up_to(
        &mut self,
        maximum_bytes: usize,
        now: Instant,
    ) -> Result<EgressDecision<'_>> {
        let (byte_count, remaining_credit, deadline) = {
            let bucket = self
                .bucket
                .as_mut()
                .context("RFCOMM MTU is not configured")?;
            validate_packet(maximum_bytes, bucket.capacity)?;
            let credit_limit = active_credit_limit(bucket.capacity, self.bytes_per_second);
            refill(bucket, self.bytes_per_second, now, credit_limit);

            let available_bytes = usize::try_from(bucket.credit / NANOS_PER_SECOND)
                .context("output pacing credit exceeds usize")?;
            if available_bytes > 0 {
                let byte_count = maximum_bytes.min(available_bytes);
                let cost = credit_for_bytes(byte_count)?;
                (byte_count, bucket.credit - cost, None)
            } else {
                let deficit = NANOS_PER_SECOND - bucket.credit;
                (0, 0, Some(deficit))
            }
        };

        let Some(deficit) = deadline else {
            return Ok(EgressDecision::Send(EgressPermit {
                limiter: self,
                byte_count,
                remaining_credit,
            }));
        };
        let wait_nanos = deficit.div_ceil(u128::from(self.bytes_per_second));
        let wait_nanos = u64::try_from(wait_nanos).context("output pacing delay exceeds u64")?;
        let deadline = now
            .checked_add(std::time::Duration::from_nanos(wait_nanos))
            .context("output rate deadline exceeds Tokio Instant")?;
        Ok(EgressDecision::WaitUntil(deadline))
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

impl EgressPermit<'_> {
    #[must_use]
    pub(crate) const fn byte_count(&self) -> usize {
        self.byte_count
    }

    /// Charge this admission at the successful queue-submission boundary.
    pub(crate) fn commit(self, submitted_at: Instant) {
        let bucket = self
            .limiter
            .bucket
            .as_mut()
            .expect("an egress permit requires a configured bucket");
        bucket.credit = self.remaining_credit;
        bucket.last_refill = submitted_at;
    }
}

fn validate_mtu(mtu: usize) -> Result<usize> {
    if mtu == 0 || mtu > usize::from(u16::MAX) {
        bail!("invalid RFCOMM MTU {mtu}");
    }
    Ok(mtu)
}

fn validate_packet(byte_count: usize, capacity: u128) -> Result<usize> {
    if byte_count == 0 {
        bail!("cannot reserve an empty output write");
    }
    let cost = credit_for_bytes(byte_count)?;
    if cost > capacity {
        bail!("output write exceeds the configured RFCOMM MTU");
    }
    Ok(byte_count)
}

fn credit_for_bytes(byte_count: usize) -> Result<u128> {
    u128::try_from(byte_count)
        .context("output byte count exceeds u128")?
        .checked_mul(NANOS_PER_SECOND)
        .context("output pacing credit overflow")
}

fn active_credit_limit(capacity: u128, bytes_per_second: u64) -> u128 {
    capacity.saturating_add(u128::from(bytes_per_second).saturating_mul(TIMER_GRANULARITY_NANOS))
}

fn refill(bucket: &mut Bucket, bytes_per_second: u64, now: Instant, credit_limit: u128) {
    let elapsed = now.saturating_duration_since(bucket.last_refill);
    let replenished = elapsed
        .as_nanos()
        .saturating_mul(u128::from(bytes_per_second));
    bucket.credit = bucket.credit.saturating_add(replenished).min(credit_limit);
    bucket.last_refill = now;
}
