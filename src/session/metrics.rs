use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tracing::debug;

use crate::bluetooth::ConnectionId;

const LATENCY_BUCKETS_MICROS: [u64; 16] = [
    100,
    250,
    500,
    1_000,
    2_000,
    5_000,
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    u64::MAX,
];

#[derive(Clone, Default)]
pub(super) struct LatencyStats {
    buckets: [u64; LATENCY_BUCKETS_MICROS.len()],
    count: u64,
    maximum: Duration,
}

impl LatencyStats {
    pub(super) fn record(&mut self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let bucket = LATENCY_BUCKETS_MICROS
            .iter()
            .position(|upper| micros <= *upper)
            .expect("latency histogram has an unbounded final bucket");
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.maximum = self.maximum.max(duration);
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn count(&self) -> u64 {
        self.count
    }

    #[must_use]
    pub(super) fn percentile(&self, percentile: u8) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        let percentile = u64::from(percentile.clamp(1, 100));
        let rank = self.count.saturating_mul(percentile).div_ceil(100);
        let mut observed = 0_u64;
        for (index, count) in self.buckets.iter().enumerate() {
            observed = observed.saturating_add(*count);
            if observed >= rank {
                return Duration::from_micros(LATENCY_BUCKETS_MICROS[index]);
            }
        }
        self.maximum
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn maximum(&self) -> Duration {
        self.maximum
    }
}

#[derive(Clone, Default)]
struct InputSnapshot {
    bytes: u64,
    writes: u64,
    dispatch_latency: LatencyStats,
    write_latency: LatencyStats,
}

#[derive(Clone, Default)]
pub(super) struct InputMetrics {
    snapshot: Arc<Mutex<InputSnapshot>>,
}

impl InputMetrics {
    pub(super) fn record(&self, bytes: usize, dispatch_latency: Duration, write_latency: Duration) {
        let mut snapshot = self.lock();
        snapshot.bytes = snapshot
            .bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        snapshot.writes = snapshot.writes.saturating_add(1);
        snapshot.dispatch_latency.record(dispatch_latency);
        snapshot.write_latency.record(write_latency);
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn bytes(&self) -> u64 {
        self.lock().bytes
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn writes(&self) -> u64 {
        self.lock().writes
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn dispatch_latency(&self) -> LatencyStats {
        self.lock().dispatch_latency.clone()
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn write_latency(&self) -> LatencyStats {
        self.lock().write_latency.clone()
    }

    fn snapshot(&self) -> InputSnapshot {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, InputSnapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Default)]
struct BlockedInterval {
    started: Option<Instant>,
    accumulated: Duration,
}

impl BlockedInterval {
    fn set(&mut self, blocked: bool, now: Instant) {
        match (blocked, self.started) {
            (true, None) => self.started = Some(now),
            (false, Some(started)) => {
                self.accumulated = self
                    .accumulated
                    .saturating_add(now.saturating_duration_since(started));
                self.started = None;
            }
            (true, Some(_)) | (false, None) => {}
        }
    }

    fn finish(&mut self, now: Instant) {
        self.set(false, now);
    }

    fn total_at(&self, now: Instant) -> Duration {
        self.started.map_or(self.accumulated, |started| {
            self.accumulated
                .saturating_add(now.saturating_duration_since(started))
        })
    }
}

pub(super) struct OutputMetrics {
    mtu: usize,
    bytes: u64,
    writes: u64,
    full_packets: u64,
    buffered_latency: LatencyStats,
    native_queue_latency: LatencyStats,
    flow_control_wait: LatencyStats,
    completion_latency: LatencyStats,
    pacing_blocked: BlockedInterval,
    staging_blocked: BlockedInterval,
    window_blocked: BlockedInterval,
    first_packet: Option<Instant>,
    last_completion: Option<Instant>,
}

impl OutputMetrics {
    pub(super) fn new(mtu: usize) -> Self {
        Self {
            mtu,
            bytes: 0,
            writes: 0,
            full_packets: 0,
            buffered_latency: LatencyStats::default(),
            native_queue_latency: LatencyStats::default(),
            flow_control_wait: LatencyStats::default(),
            completion_latency: LatencyStats::default(),
            pacing_blocked: BlockedInterval::default(),
            staging_blocked: BlockedInterval::default(),
            window_blocked: BlockedInterval::default(),
            first_packet: None,
            last_completion: None,
        }
    }

    pub(super) fn record_packet(&mut self, bytes: usize, buffered_latency: Duration, now: Instant) {
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        self.writes = self.writes.saturating_add(1);
        if bytes == self.mtu {
            self.full_packets = self.full_packets.saturating_add(1);
        }
        self.buffered_latency.record(buffered_latency);
        self.first_packet.get_or_insert(now);
    }

    pub(super) fn record_completion(
        &mut self,
        native_queue_latency: Duration,
        flow_control_wait: Duration,
        completion_latency: Duration,
        observed_at: Instant,
    ) {
        self.native_queue_latency.record(native_queue_latency);
        self.flow_control_wait.record(flow_control_wait);
        self.completion_latency.record(completion_latency);
        self.last_completion = Some(
            self.last_completion
                .map_or(observed_at, |last| last.max(observed_at)),
        );
    }

    pub(super) fn set_pacing_blocked(&mut self, blocked: bool, now: Instant) {
        self.pacing_blocked.set(blocked, now);
    }

    pub(super) fn set_staging_blocked(&mut self, blocked: bool, now: Instant) {
        self.staging_blocked.set(blocked, now);
    }

    pub(super) fn set_window_blocked(&mut self, blocked: bool, now: Instant) {
        self.window_blocked.set(blocked, now);
    }

    pub(super) fn finish(&mut self, now: Instant) {
        self.pacing_blocked.finish(now);
        self.staging_blocked.finish(now);
        self.window_blocked.finish(now);
    }

    #[must_use]
    pub(super) const fn bytes(&self) -> u64 {
        self.bytes
    }

    #[must_use]
    pub(super) const fn writes(&self) -> u64 {
        self.writes
    }

    #[must_use]
    pub(super) fn full_packet_percent(&self) -> u64 {
        self.full_packets
            .saturating_mul(100)
            .checked_div(self.writes)
            .unwrap_or(0)
    }

    #[must_use]
    pub(super) const fn buffered_latency(&self) -> &LatencyStats {
        &self.buffered_latency
    }

    #[must_use]
    pub(super) const fn native_queue_latency(&self) -> &LatencyStats {
        &self.native_queue_latency
    }

    #[must_use]
    pub(super) const fn flow_control_wait(&self) -> &LatencyStats {
        &self.flow_control_wait
    }

    #[must_use]
    pub(super) const fn completion_latency(&self) -> &LatencyStats {
        &self.completion_latency
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn staging_blocked(&self) -> Duration {
        self.staging_blocked.total_at(Instant::now())
    }

    #[must_use]
    pub(super) fn staging_blocked_at(&self, now: Instant) -> Duration {
        self.staging_blocked.total_at(now)
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn pacing_blocked(&self) -> Duration {
        self.pacing_blocked.total_at(Instant::now())
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn window_blocked(&self) -> Duration {
        self.window_blocked.total_at(Instant::now())
    }

    #[must_use]
    pub(super) fn window_blocked_at(&self, now: Instant) -> Duration {
        self.window_blocked.total_at(now)
    }

    pub(super) fn effective_kib_per_second(&self) -> u64 {
        let (Some(first), Some(last)) = (self.first_packet, self.last_completion) else {
            return 0;
        };
        let nanos = last.saturating_duration_since(first).as_nanos();
        if nanos == 0 {
            return 0;
        }
        let bytes_per_second = u128::from(self.bytes)
            .saturating_mul(1_000_000_000)
            .checked_div(nanos)
            .unwrap_or(0);
        u64::try_from(bytes_per_second / 1024).unwrap_or(u64::MAX)
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct ActivityCounts {
    input_bytes: u64,
    input_writes: u64,
    output_bytes: u64,
    output_writes: u64,
    output_completions: u64,
}

impl ActivityCounts {
    fn new(input: &InputSnapshot, output: &OutputMetrics) -> Self {
        Self {
            input_bytes: input.bytes,
            input_writes: input.writes,
            output_bytes: output.bytes(),
            output_writes: output.writes(),
            output_completions: output.completion_latency.count,
        }
    }
}

pub(super) struct PerformanceReporter {
    connection: ConnectionId,
    mtu: usize,
    staging_budget: usize,
    write_window: usize,
    started_at: Instant,
    last_periodic: ActivityCounts,
}

impl PerformanceReporter {
    pub(super) fn new(
        connection: ConnectionId,
        mtu: usize,
        staging_budget: usize,
        write_window: usize,
        started_at: Instant,
    ) -> Self {
        Self {
            connection,
            mtu,
            staging_budget,
            write_window,
            started_at,
            last_periodic: ActivityCounts::default(),
        }
    }

    pub(super) fn log_periodic(
        &mut self,
        input: &InputMetrics,
        output: &OutputMetrics,
        now: Instant,
    ) -> bool {
        let input = input.snapshot();
        let activity = ActivityCounts::new(&input, output);
        if activity == self.last_periodic && !output.has_active_blocker() {
            return false;
        }
        self.last_periodic = activity;
        self.log("periodic", &input, output, now);
        true
    }

    pub(super) fn log_pre_cleanup(
        &mut self,
        input: &InputMetrics,
        output: &OutputMetrics,
        now: Instant,
    ) {
        let input = input.snapshot();
        self.last_periodic = ActivityCounts::new(&input, output);
        self.log("pre_cleanup", &input, output, now);
    }

    pub(super) fn log_final(&self, input: &InputMetrics, output: &OutputMetrics, now: Instant) {
        self.log("final", &input.snapshot(), output, now);
    }

    fn log(
        &self,
        phase: &'static str,
        input: &InputSnapshot,
        output: &OutputMetrics,
        now: Instant,
    ) {
        let session_age = now.saturating_duration_since(self.started_at);
        debug!(
            phase,
            connection = self.connection.get(),
            session_age_ms = duration_millis(session_age),
            mtu = self.mtu,
            staging_budget = self.staging_budget,
            write_window = self.write_window,
            input_bytes = input.bytes,
            input_writes = input.writes,
            input_dispatch_p99_us = duration_micros(input.dispatch_latency.percentile(99)),
            input_write_p99_us = duration_micros(input.write_latency.percentile(99)),
            output_bytes = output.bytes(),
            output_writes = output.writes(),
            full_mtu_percent = output.full_packet_percent(),
            output_buffered_p99_us = duration_micros(output.buffered_latency().percentile(99)),
            native_queue_p99_us = duration_micros(output.native_queue_latency().percentile(99)),
            flow_wait_p99_us = duration_micros(output.flow_control_wait().percentile(99)),
            completion_p99_us = duration_micros(output.completion_latency().percentile(99)),
            staging_blocked_ms = duration_millis(output.staging_blocked_at(now)),
            window_blocked_ms = duration_millis(output.window_blocked_at(now)),
            pacing_blocked_ms = duration_millis(output.pacing_blocked_at(now)),
            effective_output_kib_s = output.effective_kib_per_second(),
            "Bluetooth session performance"
        );
    }
}

impl OutputMetrics {
    fn has_active_blocker(&self) -> bool {
        self.pacing_blocked.started.is_some()
            || self.staging_blocked.started.is_some()
            || self.window_blocked.started.is_some()
    }

    #[must_use]
    fn pacing_blocked_at(&self, now: Instant) -> Duration {
        self.pacing_blocked.total_at(now)
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
