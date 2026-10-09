//! Bounded, aggregate operational telemetry.
//!
//! Every label and metric name in this module is fixed by the application.
//! Visitor-controlled values, content identifiers, network addresses, and
//! per-session timelines have no representation in the telemetry interface.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tracing::{info, warn};

use crate::admission::AdmissionLimits;

const LATENCY_BUCKETS_SECONDS: [f64; 8] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

#[derive(Clone)]
pub(crate) struct Telemetry {
    inner: Arc<TelemetryInner>,
}

struct TelemetryInner {
    maximum_handshakes: u64,
    maximum_sessions: u64,
    connection_attempts: AtomicU64,
    accepted_handshakes: AtomicU64,
    accepted_sessions: AtomicU64,
    completed_sessions: AtomicU64,
    active_handshakes: AtomicU64,
    active_sessions: AtomicU64,
    source_rejections: AtomicU64,
    handshake_rejections: AtomicU64,
    session_rejections: AtomicU64,
    handshake_timeouts: AtomicU64,
    setup_timeouts: AtomicU64,
    input_idle_timeouts: AtomicU64,
    lifetime_timeouts: AtomicU64,
    input_rate_closures: AtomicU64,
    upstream_status: [AtomicU64; UpstreamStatus::COUNT],
    upstream_latency_buckets: [AtomicU64; LATENCY_BUCKETS_SECONDS.len()],
    upstream_latency_count: AtomicU64,
    upstream_latency_micros: AtomicU64,
    upstream_retries: AtomicU64,
    upstream_degraded: AtomicBool,
    cache_hits: AtomicU64,
    cache_stale: AtomicU64,
    cache_coalesced: AtomicU64,
    cache_evictions: AtomicU64,
    render_failures: AtomicU64,
    send_failures: AtomicU64,
    current_daily_visitors: AtomicU64,
    previous_daily_visitors: AtomicU64,
}

#[derive(Clone, Copy)]
pub(crate) enum UpstreamStatus {
    Success = 0,
    RateLimited = 1,
    ServerError = 2,
    ClientError = 3,
    TransportError = 4,
    DecodeError = 5,
}

impl UpstreamStatus {
    const COUNT: usize = 6;
    const ALL: [Self; Self::COUNT] = [
        Self::Success,
        Self::RateLimited,
        Self::ServerError,
        Self::ClientError,
        Self::TransportError,
        Self::DecodeError,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::ClientError => "client_error",
            Self::TransportError => "transport_error",
            Self::DecodeError => "decode_error",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SetupTimeoutKind {
    Handshake,
    PtyAndShell,
}

#[derive(Clone, Copy)]
pub(crate) enum SessionClosureKind {
    InputIdle,
    MaximumLifetime,
    InputRate,
}

impl Telemetry {
    pub(crate) fn new(limits: &AdmissionLimits) -> Self {
        Self {
            inner: Arc::new(TelemetryInner {
                maximum_handshakes: limits.maximum_handshakes as u64,
                maximum_sessions: limits.maximum_browsing_sessions as u64,
                connection_attempts: AtomicU64::new(0),
                accepted_handshakes: AtomicU64::new(0),
                accepted_sessions: AtomicU64::new(0),
                completed_sessions: AtomicU64::new(0),
                active_handshakes: AtomicU64::new(0),
                active_sessions: AtomicU64::new(0),
                source_rejections: AtomicU64::new(0),
                handshake_rejections: AtomicU64::new(0),
                session_rejections: AtomicU64::new(0),
                handshake_timeouts: AtomicU64::new(0),
                setup_timeouts: AtomicU64::new(0),
                input_idle_timeouts: AtomicU64::new(0),
                lifetime_timeouts: AtomicU64::new(0),
                input_rate_closures: AtomicU64::new(0),
                upstream_status: std::array::from_fn(|_| AtomicU64::new(0)),
                upstream_latency_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
                upstream_latency_count: AtomicU64::new(0),
                upstream_latency_micros: AtomicU64::new(0),
                upstream_retries: AtomicU64::new(0),
                upstream_degraded: AtomicBool::new(false),
                cache_hits: AtomicU64::new(0),
                cache_stale: AtomicU64::new(0),
                cache_coalesced: AtomicU64::new(0),
                cache_evictions: AtomicU64::new(0),
                render_failures: AtomicU64::new(0),
                send_failures: AtomicU64::new(0),
                current_daily_visitors: AtomicU64::new(0),
                previous_daily_visitors: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn connection_attempted(&self) {
        increment(&self.inner.connection_attempts);
    }

    pub(crate) fn handshake_accepted(&self) {
        increment(&self.inner.accepted_handshakes);
        increment(&self.inner.active_handshakes);
    }

    pub(crate) fn handshake_ended(&self) {
        decrement(&self.inner.active_handshakes);
    }

    pub(crate) fn session_accepted(&self) {
        increment(&self.inner.accepted_sessions);
        increment(&self.inner.active_sessions);
    }

    pub(crate) fn session_ended(&self) {
        decrement(&self.inner.active_sessions);
        increment(&self.inner.completed_sessions);
    }

    pub(crate) fn source_rejected(&self) {
        increment(&self.inner.source_rejections);
    }

    pub(crate) fn handshake_rejected(&self) {
        increment(&self.inner.handshake_rejections);
    }

    pub(crate) fn session_rejected(&self) {
        increment(&self.inner.session_rejections);
    }

    pub(crate) fn setup_timed_out(&self, kind: SetupTimeoutKind) {
        match kind {
            SetupTimeoutKind::Handshake => increment(&self.inner.handshake_timeouts),
            SetupTimeoutKind::PtyAndShell => increment(&self.inner.setup_timeouts),
        }
    }

    pub(crate) fn session_closed(&self, kind: SessionClosureKind) {
        match kind {
            SessionClosureKind::InputIdle => increment(&self.inner.input_idle_timeouts),
            SessionClosureKind::MaximumLifetime => increment(&self.inner.lifetime_timeouts),
            SessionClosureKind::InputRate => increment(&self.inner.input_rate_closures),
        }
    }

    pub(crate) fn upstream_request(&self, status: UpstreamStatus, latency: Duration) {
        increment(&self.inner.upstream_status[status as usize]);
        increment(&self.inner.upstream_latency_count);
        self.inner.upstream_latency_micros.fetch_add(
            latency.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        for (index, upper_bound) in LATENCY_BUCKETS_SECONDS.iter().enumerate() {
            if latency.as_secs_f64() <= *upper_bound {
                increment(&self.inner.upstream_latency_buckets[index]);
            }
        }

        if matches!(status, UpstreamStatus::Success) {
            if self.inner.upstream_degraded.swap(false, Ordering::Relaxed) {
                info!("Hacker News upstream recovered");
            }
        } else if !self.inner.upstream_degraded.swap(true, Ordering::Relaxed) {
            warn!(
                upstream.status = status.label(),
                "Hacker News upstream degraded"
            );
        }
    }

    pub(crate) fn upstream_retry(&self) {
        increment(&self.inner.upstream_retries);
    }

    pub(crate) fn cache_hit(&self) {
        increment(&self.inner.cache_hits);
    }

    pub(crate) fn cache_stale(&self) {
        increment(&self.inner.cache_stale);
    }

    pub(crate) fn cache_coalesced(&self) {
        increment(&self.inner.cache_coalesced);
    }

    pub(crate) fn cache_evicted(&self) {
        increment(&self.inner.cache_evictions);
    }

    pub(crate) fn render_failed(&self) {
        increment(&self.inner.render_failures);
    }

    pub(crate) fn send_failed(&self) {
        increment(&self.inner.send_failures);
    }

    pub(crate) fn set_daily_visitors(&self, current: u64, previous: u64) {
        self.inner
            .current_daily_visitors
            .store(current, Ordering::Relaxed);
        self.inner
            .previous_daily_visitors
            .store(previous, Ordering::Relaxed);
    }

    pub(crate) fn is_upstream_degraded(&self) -> bool {
        self.inner.upstream_degraded.load(Ordering::Relaxed)
    }

    pub(crate) fn render_prometheus(&self, ready: bool) -> String {
        let mut output = String::with_capacity(4_096);
        metric_header(
            &mut output,
            "hnssh_build_info",
            "HNSSH build identity.",
            "gauge",
        );
        let _ = writeln!(
            output,
            "hnssh_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );

        metric_header(
            &mut output,
            "hnssh_runtime_ready",
            "Whether the SSH session runtime is accepting work.",
            "gauge",
        );
        let _ = writeln!(output, "hnssh_runtime_ready {}", u8::from(ready));
        metric_header(
            &mut output,
            "hnssh_upstream_degraded",
            "Whether the most recent Hacker News upstream operation failed.",
            "gauge",
        );
        let _ = writeln!(
            output,
            "hnssh_upstream_degraded {}",
            u8::from(self.is_upstream_degraded())
        );

        metric_header(
            &mut output,
            "hnssh_active_handshakes",
            "SSH handshakes and PTY/shell setups currently charged.",
            "gauge",
        );
        atomic_metric(
            &mut output,
            "hnssh_active_handshakes",
            &self.inner.active_handshakes,
        );
        metric_header(
            &mut output,
            "hnssh_active_sessions",
            "Established browsing sessions currently charged.",
            "gauge",
        );
        atomic_metric(
            &mut output,
            "hnssh_active_sessions",
            &self.inner.active_sessions,
        );
        metric_header(
            &mut output,
            "hnssh_capacity",
            "Configured admission capacity by phase.",
            "gauge",
        );
        let _ = writeln!(
            output,
            "hnssh_capacity{{phase=\"handshake\"}} {}",
            self.inner.maximum_handshakes
        );
        let _ = writeln!(
            output,
            "hnssh_capacity{{phase=\"session\"}} {}",
            self.inner.maximum_sessions
        );

        metric_header(
            &mut output,
            "hnssh_connections_total",
            "Aggregate connection and browsing-session lifecycle outcomes.",
            "counter",
        );
        labeled_atomic_metric(
            &mut output,
            "hnssh_connections_total",
            "outcome",
            "attempted",
            &self.inner.connection_attempts,
        );
        labeled_atomic_metric(
            &mut output,
            "hnssh_connections_total",
            "outcome",
            "handshake_accepted",
            &self.inner.accepted_handshakes,
        );
        labeled_atomic_metric(
            &mut output,
            "hnssh_connections_total",
            "outcome",
            "session_accepted",
            &self.inner.accepted_sessions,
        );
        labeled_atomic_metric(
            &mut output,
            "hnssh_connections_total",
            "outcome",
            "session_completed",
            &self.inner.completed_sessions,
        );

        metric_header(
            &mut output,
            "hnssh_admission_rejections_total",
            "Connections rejected by bounded admission phase.",
            "counter",
        );
        for (reason, value) in [
            ("source", &self.inner.source_rejections),
            ("handshake_capacity", &self.inner.handshake_rejections),
            ("session_capacity", &self.inner.session_rejections),
        ] {
            labeled_atomic_metric(
                &mut output,
                "hnssh_admission_rejections_total",
                "reason",
                reason,
                value,
            );
        }

        metric_header(
            &mut output,
            "hnssh_timeouts_total",
            "SSH setup and browsing-session timeout closures.",
            "counter",
        );
        for (kind, value) in [
            ("handshake", &self.inner.handshake_timeouts),
            ("pty_shell_setup", &self.inner.setup_timeouts),
            ("input_idle", &self.inner.input_idle_timeouts),
            ("maximum_lifetime", &self.inner.lifetime_timeouts),
        ] {
            labeled_atomic_metric(&mut output, "hnssh_timeouts_total", "kind", kind, value);
        }
        metric_header(
            &mut output,
            "hnssh_input_rate_closures_total",
            "Browsing sessions closed for sustained excess input.",
            "counter",
        );
        atomic_metric(
            &mut output,
            "hnssh_input_rate_closures_total",
            &self.inner.input_rate_closures,
        );

        metric_header(
            &mut output,
            "hnssh_upstream_requests_total",
            "Hacker News upstream attempts by bounded status category.",
            "counter",
        );
        for status in UpstreamStatus::ALL {
            labeled_atomic_metric(
                &mut output,
                "hnssh_upstream_requests_total",
                "status",
                status.label(),
                &self.inner.upstream_status[status as usize],
            );
        }
        metric_header(
            &mut output,
            "hnssh_upstream_request_duration_seconds",
            "Hacker News upstream request latency.",
            "histogram",
        );
        for (index, upper_bound) in LATENCY_BUCKETS_SECONDS.iter().enumerate() {
            let _ = writeln!(
                output,
                "hnssh_upstream_request_duration_seconds_bucket{{le=\"{upper_bound}\"}} {}",
                load(&self.inner.upstream_latency_buckets[index])
            );
        }
        let count = load(&self.inner.upstream_latency_count);
        let _ = writeln!(
            output,
            "hnssh_upstream_request_duration_seconds_bucket{{le=\"+Inf\"}} {count}"
        );
        let _ = writeln!(
            output,
            "hnssh_upstream_request_duration_seconds_sum {}",
            load(&self.inner.upstream_latency_micros) as f64 / 1_000_000.0
        );
        let _ = writeln!(
            output,
            "hnssh_upstream_request_duration_seconds_count {count}"
        );
        metric_header(
            &mut output,
            "hnssh_upstream_retries_total",
            "Hacker News upstream retries.",
            "counter",
        );
        atomic_metric(
            &mut output,
            "hnssh_upstream_retries_total",
            &self.inner.upstream_retries,
        );

        metric_header(
            &mut output,
            "hnssh_cache_events_total",
            "Shared content-cache outcomes.",
            "counter",
        );
        for (outcome, value) in [
            ("hit", &self.inner.cache_hits),
            ("stale", &self.inner.cache_stale),
            ("coalesced", &self.inner.cache_coalesced),
            ("eviction", &self.inner.cache_evictions),
        ] {
            labeled_atomic_metric(
                &mut output,
                "hnssh_cache_events_total",
                "outcome",
                outcome,
                value,
            );
        }

        metric_header(
            &mut output,
            "hnssh_render_failures_total",
            "Terminal frame rendering or encoding failures.",
            "counter",
        );
        atomic_metric(
            &mut output,
            "hnssh_render_failures_total",
            &self.inner.render_failures,
        );
        metric_header(
            &mut output,
            "hnssh_send_failures_total",
            "SSH terminal-output send failures.",
            "counter",
        );
        atomic_metric(
            &mut output,
            "hnssh_send_failures_total",
            &self.inner.send_failures,
        );

        metric_header(
            &mut output,
            "hnssh_daily_visitors_estimate",
            "Approximate distinct network sources for the current and completed UTC day.",
            "gauge",
        );
        labeled_atomic_metric(
            &mut output,
            "hnssh_daily_visitors_estimate",
            "period",
            "current",
            &self.inner.current_daily_visitors,
        );
        labeled_atomic_metric(
            &mut output,
            "hnssh_daily_visitors_estimate",
            "period",
            "previous",
            &self.inner.previous_daily_visitors,
        );
        output
    }
}

fn increment(value: &AtomicU64) {
    value.fetch_add(1, Ordering::Relaxed);
}

fn decrement(value: &AtomicU64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(1))
    });
}

fn load(value: &AtomicU64) -> u64 {
    value.load(Ordering::Relaxed)
}

fn metric_header(output: &mut String, name: &str, help: &str, metric_type: &str) {
    let _ = writeln!(output, "# HELP {name} {help}");
    let _ = writeln!(output, "# TYPE {name} {metric_type}");
}

fn atomic_metric(output: &mut String, name: &str, value: &AtomicU64) {
    let _ = writeln!(output, "{name} {}", load(value));
}

fn labeled_atomic_metric(
    output: &mut String,
    name: &str,
    label_name: &str,
    label_value: &str,
    value: &AtomicU64,
) {
    let _ = writeln!(
        output,
        "{name}{{{label_name}=\"{label_value}\"}} {}",
        load(value)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_schema_contains_only_fixed_aggregate_dimensions() {
        let telemetry = Telemetry::new(&AdmissionLimits::default());
        telemetry.connection_attempted();
        telemetry.source_rejected();
        telemetry.set_daily_visitors(12, 9);
        telemetry.upstream_request(UpstreamStatus::ServerError, Duration::from_millis(125));
        let metrics = telemetry.render_prometheus(true);

        assert!(metrics.contains("hnssh_runtime_ready 1"));
        assert!(metrics.contains("hnssh_upstream_degraded 1"));
        assert!(metrics.contains("status=\"server_error\""));
        assert!(metrics.contains("period=\"current\"} 12"));
        assert!(metrics.contains("reason=\"source\"} 1"));
        assert!(!metrics.contains("127.0.0.1"));

        telemetry.upstream_request(UpstreamStatus::Success, Duration::from_millis(10));
        assert!(!telemetry.is_upstream_degraded());
    }
}
