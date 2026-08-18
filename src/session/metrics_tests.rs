use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

use super::metrics::{InputMetrics, LatencyStats, OutputMetrics, PerformanceReporter};
use crate::bluetooth::ConnectionId;

#[derive(Default)]
struct EventLevels {
    debug: AtomicUsize,
    info: AtomicUsize,
}

struct LevelSubscriber {
    levels: Arc<EventLevels>,
}

impl Subscriber for LevelSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        match *event.metadata().level() {
            Level::DEBUG => {
                self.levels.debug.fetch_add(1, Ordering::Relaxed);
            }
            Level::INFO => {
                self.levels.info.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[test]
fn test_latency_stats_reports_bounded_percentiles_without_samples() {
    let stats = LatencyStats::default();
    assert_eq!(stats.count(), 0);
    assert_eq!(stats.percentile(99), Duration::ZERO);
    assert_eq!(stats.maximum(), Duration::ZERO);
}

#[test]
fn test_latency_stats_reports_percentile_and_exact_maximum() {
    let mut stats = LatencyStats::default();
    for micros in [50, 200, 700, 1_500, 20_000] {
        stats.record(Duration::from_micros(micros));
    }

    assert_eq!(stats.count(), 5);
    assert_eq!(stats.percentile(50), Duration::from_millis(1));
    assert_eq!(stats.percentile(99), Duration::from_millis(25));
    assert_eq!(stats.maximum(), Duration::from_millis(20));
}

#[test]
fn test_input_metrics_records_only_counts_bytes_and_latency() {
    let metrics = InputMetrics::default();
    let task_metrics = metrics.clone();
    task_metrics.record(3, Duration::from_micros(250), Duration::from_micros(500));
    task_metrics.record(2, Duration::from_micros(750), Duration::from_micros(1_500));

    assert_eq!(metrics.bytes(), 5);
    assert_eq!(metrics.writes(), 2);
    assert_eq!(
        metrics.dispatch_latency().maximum(),
        Duration::from_micros(750)
    );
    assert_eq!(
        metrics.write_latency().maximum(),
        Duration::from_micros(1_500)
    );
}

#[test]
fn test_output_metrics_records_packet_fill_and_transport_latency() {
    let now = Instant::now();
    let mut metrics = OutputMetrics::new(4);
    metrics.record_packet(4, Duration::from_micros(100), now);
    metrics.record_packet(4, Duration::from_micros(200), now);
    metrics.record_packet(2, Duration::from_micros(300), now);
    metrics.record_completion(
        Duration::from_micros(400),
        Duration::from_micros(500),
        Duration::from_micros(600),
        now + Duration::from_millis(1),
    );

    assert_eq!(metrics.bytes(), 10);
    assert_eq!(metrics.writes(), 3);
    assert_eq!(metrics.full_packet_percent(), 66);
    assert_eq!(
        metrics.buffered_latency().maximum(),
        Duration::from_micros(300)
    );
    assert_eq!(
        metrics.native_queue_latency().maximum(),
        Duration::from_micros(400)
    );
    assert_eq!(
        metrics.flow_control_wait().maximum(),
        Duration::from_micros(500)
    );
    assert_eq!(
        metrics.completion_latency().maximum(),
        Duration::from_micros(600)
    );
}

#[test]
fn test_output_metrics_accumulates_bounded_blocking_intervals() {
    let started = Instant::now();
    let mut metrics = OutputMetrics::new(667);
    metrics.set_staging_blocked(true, started);
    metrics.set_staging_blocked(false, started + Duration::from_millis(10));
    metrics.set_pacing_blocked(true, started + Duration::from_millis(5));
    metrics.set_pacing_blocked(false, started + Duration::from_millis(20));
    metrics.set_window_blocked(true, started + Duration::from_millis(20));

    assert_eq!(
        metrics.staging_blocked_at(started + Duration::from_millis(30)),
        Duration::from_millis(10)
    );
    assert_eq!(
        metrics.window_blocked_at(started + Duration::from_millis(30)),
        Duration::from_millis(10)
    );
    metrics.finish(started + Duration::from_millis(45));

    assert_eq!(metrics.staging_blocked(), Duration::from_millis(10));
    assert_eq!(metrics.pacing_blocked(), Duration::from_millis(15));
    assert_eq!(metrics.window_blocked(), Duration::from_millis(25));
}

#[test]
fn test_performance_reporter_logs_periodic_snapshot_only_after_activity_changes() {
    let started = Instant::now();
    let input = InputMetrics::default();
    let mut output = OutputMetrics::new(667);
    let mut reporter =
        PerformanceReporter::new(ConnectionId::new(1).unwrap(), 667, 20_480, 8, started);

    assert!(!reporter.log_periodic(&input, &output, started));
    output.record_packet(10, Duration::from_micros(100), started);
    assert!(reporter.log_periodic(&input, &output, started + Duration::from_secs(5)));
    assert!(!reporter.log_periodic(&input, &output, started + Duration::from_secs(10)));

    output.record_completion(
        Duration::from_micros(100),
        Duration::ZERO,
        Duration::from_micros(100),
        started + Duration::from_secs(11),
    );
    assert!(reporter.log_periodic(&input, &output, started + Duration::from_secs(12)));

    output.set_window_blocked(true, started + Duration::from_secs(13));
    assert!(reporter.log_periodic(&input, &output, started + Duration::from_secs(14)));
    assert!(reporter.log_periodic(&input, &output, started + Duration::from_secs(15)));

    input.record(1, Duration::from_micros(100), Duration::from_micros(100));
    assert!(reporter.log_periodic(&input, &output, started + Duration::from_secs(16)));
}

#[test]
fn test_output_metrics_effective_rate_includes_final_completion() {
    let started = Instant::now();
    let mut metrics = OutputMetrics::new(512);
    metrics.record_packet(512, Duration::ZERO, started);
    metrics.record_packet(512, Duration::ZERO, started + Duration::from_millis(5));
    metrics.record_completion(
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        started + Duration::from_millis(10),
    );

    assert_eq!(metrics.effective_kib_per_second(), 100);
}

#[test]
fn test_performance_reporter_emits_only_debug_events() {
    let started = Instant::now();
    let input = InputMetrics::default();
    let mut output = OutputMetrics::new(512);
    let mut reporter =
        PerformanceReporter::new(ConnectionId::new(1).unwrap(), 512, 20_480, 8, started);
    output.record_packet(10, Duration::ZERO, started);

    let levels = Arc::new(EventLevels::default());
    tracing::subscriber::with_default(
        LevelSubscriber {
            levels: Arc::clone(&levels),
        },
        || {
            assert!(reporter.log_periodic(&input, &output, started + Duration::from_secs(5)));
            reporter.log_pre_cleanup(&input, &output, started + Duration::from_secs(6));
            reporter.log_final(&input, &output, started + Duration::from_secs(7));
        },
    );

    assert_eq!(levels.debug.load(Ordering::Relaxed), 3);
    assert_eq!(levels.info.load(Ordering::Relaxed), 0);
}
