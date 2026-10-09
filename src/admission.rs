//! Privacy-preserving connection admission.
//!
//! Raw peer addresses enter this module only through [`Admission::admit`]. The
//! address is normalized and immediately transformed with an ephemeral,
//! hour-derived HMAC key. Only the resulting opaque value can enter the
//! in-memory rate-limit table; neither addresses nor keyed values are exposed
//! to callers, logs, metrics, errors, or persistence.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio::time::Instant;

use crate::telemetry::Telemetry;

const MAXIMUM_SOURCE_BUCKETS: usize = 65_536;
const SOURCE_KEY_ROTATION: Duration = Duration::from_secs(60 * 60);
const SECONDS_PER_UTC_DAY: u64 = 24 * 60 * 60;
const HYPERLOGLOG_PRECISION: u32 = 10;
const HYPERLOGLOG_REGISTERS: usize = 1 << HYPERLOGLOG_PRECISION;

#[derive(Clone, Debug)]
pub struct AdmissionLimits {
    pub maximum_handshakes: usize,
    pub maximum_browsing_sessions: usize,
    pub maximum_connections_per_source: usize,
    pub maximum_source_buckets: usize,
    pub source_attempts_per_window: u32,
    pub source_attempt_burst: u32,
    pub source_attempt_window: Duration,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            maximum_handshakes: 32,
            maximum_browsing_sessions: 200,
            maximum_connections_per_source: 8,
            maximum_source_buckets: 4_096,
            source_attempts_per_window: 30,
            source_attempt_burst: 10,
            source_attempt_window: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Error)]
pub enum AdmissionError {
    #[error("admission limit {0} must be greater than zero")]
    ZeroLimit(&'static str),
    #[error("invalid admission configuration: {0}")]
    InvalidLimit(&'static str),
    #[error("could not initialize the ephemeral admission key")]
    KeyInitialization,
}

#[derive(Clone)]
pub(crate) struct Admission {
    source: Arc<SourceAdmission>,
    handshakes: Arc<Semaphore>,
    sessions: Arc<Semaphore>,
    telemetry: Telemetry,
}

impl Admission {
    #[cfg(test)]
    pub(crate) fn new(limits: AdmissionLimits) -> Result<Self, AdmissionError> {
        let telemetry = Telemetry::new(&limits);
        Self::new_with_telemetry(limits, telemetry)
    }

    pub(crate) fn new_with_telemetry(
        limits: AdmissionLimits,
        telemetry: Telemetry,
    ) -> Result<Self, AdmissionError> {
        validate_limits(&limits)?;
        let source = Arc::new(SourceAdmission::new(&limits)?);
        Ok(Self {
            source,
            handshakes: Arc::new(Semaphore::new(limits.maximum_handshakes)),
            sessions: Arc::new(Semaphore::new(limits.maximum_browsing_sessions)),
            telemetry,
        })
    }

    /// Admit one TCP peer without allowing its address to leave this call.
    pub(crate) fn admit(&self, peer: SocketAddr) -> Option<ConnectionAdmission> {
        self.telemetry.connection_attempted();
        self.source
            .observe_visitor(peer.ip(), SystemTime::now(), &self.telemetry);

        // Charge source policy even when global capacity is exhausted. The
        // source table has its own hard cap, so this ordering cannot recreate
        // unbounded state under a distributed flood.
        let source = match self.source.admit(peer.ip(), Instant::now()) {
            Some(source) => source,
            None => {
                self.telemetry.source_rejected();
                return None;
            }
        };
        let handshake = match Arc::clone(&self.handshakes).try_acquire_owned() {
            Ok(handshake) => handshake,
            Err(_) => {
                self.telemetry.handshake_rejected();
                return None;
            }
        };
        let handshake = HandshakePermitOwner::new(handshake, self.telemetry.clone());
        let established = SessionPermitOwner::new(self.telemetry.clone());
        Some(ConnectionAdmission {
            _source: source,
            session: SessionAdmission {
                handshake: handshake.release_handle(),
                established: established.install_handle(),
                sessions: Arc::clone(&self.sessions),
                telemetry: self.telemetry.clone(),
            },
            _handshake: handshake,
            _established: established,
        })
    }

    pub(crate) fn prune_expired(&self) {
        self.source.prune_expired(Instant::now());
    }

    pub(crate) fn cleanup_interval(&self) -> Duration {
        self.source.cleanup_interval
    }
}

fn validate_limits(limits: &AdmissionLimits) -> Result<(), AdmissionError> {
    for (name, value) in [
        ("maximum_handshakes", limits.maximum_handshakes),
        (
            "maximum_browsing_sessions",
            limits.maximum_browsing_sessions,
        ),
        (
            "maximum_connections_per_source",
            limits.maximum_connections_per_source,
        ),
        ("maximum_source_buckets", limits.maximum_source_buckets),
        (
            "source_attempts_per_window",
            limits.source_attempts_per_window as usize,
        ),
        ("source_attempt_burst", limits.source_attempt_burst as usize),
    ] {
        if value == 0 {
            return Err(AdmissionError::ZeroLimit(name));
        }
    }
    if limits.maximum_handshakes > Semaphore::MAX_PERMITS {
        return Err(AdmissionError::InvalidLimit(
            "maximum_handshakes exceeds the runtime semaphore bound",
        ));
    }
    if limits.maximum_browsing_sessions > Semaphore::MAX_PERMITS {
        return Err(AdmissionError::InvalidLimit(
            "maximum_browsing_sessions exceeds the runtime semaphore bound",
        ));
    }
    if limits.maximum_connections_per_source > Semaphore::MAX_PERMITS {
        return Err(AdmissionError::InvalidLimit(
            "maximum_connections_per_source exceeds the supported bound",
        ));
    }
    if limits.maximum_source_buckets > MAXIMUM_SOURCE_BUCKETS {
        return Err(AdmissionError::InvalidLimit(
            "maximum_source_buckets exceeds the supported bound",
        ));
    }
    if limits.source_attempt_burst > limits.source_attempts_per_window {
        return Err(AdmissionError::InvalidLimit(
            "source_attempt_burst must not exceed source_attempts_per_window",
        ));
    }
    if limits.source_attempt_window.is_zero()
        || limits.source_attempt_window > Duration::from_secs(60 * 60)
    {
        return Err(AdmissionError::InvalidLimit(
            "source_attempt_window must be between one nanosecond and one hour",
        ));
    }
    Ok(())
}

/// Owns all permits associated with one accepted connection. No source value
/// is exposed by this type.
pub(crate) struct ConnectionAdmission {
    _source: SourcePermit,
    _handshake: HandshakePermitOwner,
    _established: SessionPermitOwner,
    session: SessionAdmission,
}

impl ConnectionAdmission {
    pub(crate) fn session_admission(&self) -> SessionAdmission {
        self.session.clone()
    }
}

struct HandshakePermitOwner {
    permit: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    telemetry: Telemetry,
}

impl HandshakePermitOwner {
    fn new(permit: OwnedSemaphorePermit, telemetry: Telemetry) -> Self {
        telemetry.handshake_accepted();
        Self {
            permit: Arc::new(Mutex::new(Some(permit))),
            telemetry,
        }
    }

    fn release_handle(&self) -> HandshakeRelease {
        HandshakeRelease {
            permit: Arc::clone(&self.permit),
            telemetry: self.telemetry.clone(),
        }
    }
}

impl Drop for HandshakePermitOwner {
    fn drop(&mut self) {
        if lock_unpoisoned(&self.permit).take().is_some() {
            self.telemetry.handshake_ended();
        }
    }
}

#[derive(Clone)]
struct HandshakeRelease {
    permit: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    telemetry: Telemetry,
}

impl HandshakeRelease {
    fn release(&self) {
        if lock_unpoisoned(&self.permit).take().is_some() {
            self.telemetry.handshake_ended();
        }
    }
}

struct SessionPermitOwner {
    permit: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    telemetry: Telemetry,
}

impl SessionPermitOwner {
    fn new(telemetry: Telemetry) -> Self {
        Self {
            permit: Arc::new(Mutex::new(None)),
            telemetry,
        }
    }

    fn install_handle(&self) -> SessionPermitInstall {
        SessionPermitInstall {
            permit: Arc::clone(&self.permit),
            telemetry: self.telemetry.clone(),
        }
    }
}

impl Drop for SessionPermitOwner {
    fn drop(&mut self) {
        if lock_unpoisoned(&self.permit).take().is_some() {
            self.telemetry.session_ended();
        }
    }
}

#[derive(Clone)]
struct SessionPermitInstall {
    permit: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    telemetry: Telemetry,
}

impl SessionPermitInstall {
    fn install(&self, permit: OwnedSemaphorePermit) -> bool {
        let mut slot = lock_unpoisoned(&self.permit);
        if slot.is_some() {
            return false;
        }
        self.telemetry.session_accepted();
        *slot = Some(permit);
        true
    }
}

#[derive(Clone)]
pub(crate) struct SessionAdmission {
    handshake: HandshakeRelease,
    established: SessionPermitInstall,
    sessions: Arc<Semaphore>,
    telemetry: Telemetry,
}

impl SessionAdmission {
    /// Reserve established-session capacity without opening a permit gap. The
    /// handshake remains charged until [`Self::establish`] runs after the actor
    /// has been fully installed in its lifecycle scope.
    pub(crate) fn try_reserve(&self) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        let result = Arc::clone(&self.sessions).try_acquire_owned();
        if result.is_err() {
            self.telemetry.session_rejected();
        }
        result
    }

    pub(crate) fn establish(&self, permit: OwnedSemaphorePermit) -> bool {
        if !self.established.install(permit) {
            return false;
        }
        self.handshake.release();
        true
    }
}

/// Opaque per-generation derived source value. It deliberately implements
/// neither `Debug` nor `Display`, so it cannot be formatted into logs, errors,
/// or telemetry without adding an explicit conversion that review would catch.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct SourceKey([u8; 32]);

struct SourceBucket {
    hour: u64,
    active: usize,
    tokens: f64,
    updated_at: Instant,
    expires_at: Instant,
}

struct SourceState {
    buckets: HashMap<SourceKey, SourceBucket>,
    active_hours: HashMap<u64, usize>,
}

struct SourceAdmission {
    root_key: hmac::Key,
    state: Mutex<SourceState>,
    daily_visitors: Mutex<DailyVisitors>,
    maximum_connections: usize,
    maximum_buckets: usize,
    attempts_per_window: f64,
    burst: f64,
    window: Duration,
    cleanup_interval: Duration,
    rotation_started_at: Instant,
}

impl SourceAdmission {
    fn new(limits: &AdmissionLimits) -> Result<Self, AdmissionError> {
        Self::new_at(limits, Instant::now())
    }

    fn new_at(
        limits: &AdmissionLimits,
        rotation_started_at: Instant,
    ) -> Result<Self, AdmissionError> {
        let mut secret = [0_u8; 32];
        SystemRandom::new()
            .fill(&mut secret)
            .map_err(|_| AdmissionError::KeyInitialization)?;
        let root_key = hmac::Key::new(hmac::HMAC_SHA256, &secret);
        secret.fill(0);
        let cleanup_interval = limits
            .source_attempt_window
            .min(Duration::from_secs(1))
            .max(Duration::from_millis(10));
        Ok(Self {
            root_key,
            state: Mutex::new(SourceState {
                buckets: HashMap::new(),
                active_hours: HashMap::new(),
            }),
            daily_visitors: Mutex::new(DailyVisitors::new()),
            maximum_connections: limits.maximum_connections_per_source,
            maximum_buckets: limits.maximum_source_buckets,
            attempts_per_window: f64::from(limits.source_attempts_per_window),
            burst: f64::from(limits.source_attempt_burst),
            window: limits.source_attempt_window,
            cleanup_interval,
            rotation_started_at,
        })
    }

    fn admit(self: &Arc<Self>, source: IpAddr, now: Instant) -> Option<SourcePermit> {
        self.admit_in_generation(source, now, self.generation_at(now))
    }

    fn generation_at(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.rotation_started_at)
            .as_secs()
            / SOURCE_KEY_ROTATION.as_secs()
    }

    fn admit_in_generation(
        self: &Arc<Self>,
        source: IpAddr,
        now: Instant,
        hour: u64,
    ) -> Option<SourcePermit> {
        let expires_at = now.checked_add(self.window)?;
        let key = self.transform(source, hour);
        let mut state = lock_unpoisoned(&self.state);

        if !state.buckets.contains_key(&key) && state.buckets.len() >= self.maximum_buckets {
            // Expiry is periodic; never scan the full table on the hostile
            // admission path merely because it has reached its hard cap.
            return None;
        }

        // Active connections can span hour rotation. The number of represented
        // active hours is bounded by transport lifetime, so this performs a few
        // HMAC + hash lookups rather than scanning every source bucket.
        let active_hours = state.active_hours.keys().copied().collect::<Vec<_>>();
        let active = active_hours
            .into_iter()
            .filter_map(|active_hour| {
                state
                    .buckets
                    .get(&self.transform(source, active_hour))
                    .map(|bucket| bucket.active)
            })
            .sum::<usize>();

        // A validated rate window is at most one hour. Only the immediately
        // preceding hourly value can still carry unrefilled rate state.
        let inherited_rate = hour.checked_sub(1).and_then(|previous_hour| {
            state
                .buckets
                .get(&self.transform(source, previous_hour))
                .and_then(|bucket| {
                    (bucket.expires_at > now).then_some((
                        bucket.tokens,
                        bucket.updated_at,
                        bucket.expires_at,
                    ))
                })
        });
        let bucket = state.buckets.entry(key).or_insert_with(|| {
            let (tokens, updated_at, inherited_expiry) =
                inherited_rate.unwrap_or((self.burst, now, expires_at));
            SourceBucket {
                hour,
                active: 0,
                tokens,
                updated_at,
                expires_at: inherited_expiry,
            }
        });

        let elapsed = now.saturating_duration_since(bucket.updated_at);
        let refill = elapsed.as_secs_f64() / self.window.as_secs_f64() * self.attempts_per_window;
        bucket.tokens = (bucket.tokens + refill).min(self.burst);
        bucket.updated_at = now;
        bucket.expires_at = expires_at;
        if bucket.tokens < 1.0 {
            return None;
        }
        bucket.tokens -= 1.0;
        if active >= self.maximum_connections {
            return None;
        }
        bucket.active += 1;
        *state.active_hours.entry(hour).or_default() += 1;
        drop(state);

        Some(SourcePermit {
            admission: Arc::clone(self),
            key: Some(key),
        })
    }

    fn transform(&self, source: IpAddr, hour: u64) -> SourceKey {
        let hour_key = hmac::sign(&self.root_key, &hour.to_be_bytes());
        let hour_key = hmac::Key::new(hmac::HMAC_SHA256, hour_key.as_ref());
        let (mut normalized, length) = normalized_source(source);
        let tag = hmac::sign(&hour_key, &normalized[..length]);
        let mut value = [0_u8; 32];
        value.copy_from_slice(tag.as_ref());
        normalized.fill(0);
        SourceKey(value)
    }

    fn observe_visitor(
        &self,
        source: IpAddr,
        now: SystemTime,
        telemetry: &Telemetry,
    ) -> DailyVisitorCounts {
        let mut visitors = lock_unpoisoned(&self.daily_visitors);
        let counts = visitors.observe(&self.root_key, source, now);
        // Publish before releasing the sketch lock so concurrent observations
        // cannot make the aggregate estimate move backwards.
        telemetry.set_daily_visitors(counts.current, counts.previous);
        counts
    }

    fn release(&self, key: SourceKey) {
        let now = Instant::now();
        let mut state = lock_unpoisoned(&self.state);
        let mut released_hour = None;
        let mut remove_bucket = false;
        if let Some(bucket) = state.buckets.get_mut(&key) {
            if bucket.active > 0 {
                bucket.active -= 1;
                released_hour = Some(bucket.hour);
            }
            remove_bucket = bucket.active == 0 && bucket.expires_at <= now;
        }
        if remove_bucket {
            state.buckets.remove(&key);
        }
        if let Some(hour) = released_hour
            && let Some(active) = state.active_hours.get_mut(&hour)
        {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_hours.remove(&hour);
            }
        }
    }

    fn prune_expired(&self, now: Instant) {
        prune_state(&mut lock_unpoisoned(&self.state), now);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DailyVisitorCounts {
    current: u64,
    previous: u64,
}

struct DailyVisitors {
    day: Option<u64>,
    registers: Box<[u8; HYPERLOGLOG_REGISTERS]>,
    previous_estimate: u64,
}

impl DailyVisitors {
    fn new() -> Self {
        Self {
            day: None,
            registers: Box::new([0; HYPERLOGLOG_REGISTERS]),
            previous_estimate: 0,
        }
    }

    fn observe(
        &mut self,
        root_key: &hmac::Key,
        source: IpAddr,
        now: SystemTime,
    ) -> DailyVisitorCounts {
        let observed_day = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
            / SECONDS_PER_UTC_DAY;
        let day = match self.day {
            Some(current) if observed_day <= current => current,
            Some(_) => {
                self.previous_estimate = hyperloglog_estimate(&self.registers);
                self.registers.fill(0);
                self.day = Some(observed_day);
                observed_day
            }
            None => {
                self.day = Some(observed_day);
                observed_day
            }
        };

        let mut key_material = [0_u8; 21];
        key_material[..13].copy_from_slice(b"daily-visitor");
        key_material[13..].copy_from_slice(&day.to_be_bytes());
        let day_key = hmac::sign(root_key, &key_material);
        let day_key = hmac::Key::new(hmac::HMAC_SHA256, day_key.as_ref());
        key_material.fill(0);
        let (mut normalized, length) = normalized_source(source);
        let tag = hmac::sign(&day_key, &normalized[..length]);
        normalized.fill(0);

        let hash = u64::from_be_bytes(tag.as_ref()[..8].try_into().expect("HMAC prefix"));
        let register = (hash >> (64 - HYPERLOGLOG_PRECISION)) as usize;
        let remaining = hash << HYPERLOGLOG_PRECISION;
        let maximum_rank = 64 - HYPERLOGLOG_PRECISION + 1;
        let rank = remaining
            .leading_zeros()
            .saturating_add(1)
            .min(maximum_rank) as u8;
        self.registers[register] = self.registers[register].max(rank);

        DailyVisitorCounts {
            current: hyperloglog_estimate(&self.registers),
            previous: self.previous_estimate,
        }
    }
}

fn hyperloglog_estimate(registers: &[u8; HYPERLOGLOG_REGISTERS]) -> u64 {
    let register_count = HYPERLOGLOG_REGISTERS as f64;
    let alpha = 0.7213 / (1.0 + 1.079 / register_count);
    let harmonic_sum = registers
        .iter()
        .map(|rank| 2_f64.powi(-i32::from(*rank)))
        .sum::<f64>();
    let raw = alpha * register_count * register_count / harmonic_sum;
    let empty = registers.iter().filter(|rank| **rank == 0).count();
    let corrected = if raw <= 2.5 * register_count && empty > 0 {
        register_count * (register_count / empty as f64).ln()
    } else {
        raw
    };
    corrected.round().max(0.0) as u64
}

fn normalized_source(source: IpAddr) -> ([u8; 17], usize) {
    let mut normalized = [0_u8; 17];
    let length = match source {
        IpAddr::V4(address) => {
            normalized[0] = 4;
            normalized[1..5].copy_from_slice(&address.octets());
            5
        }
        IpAddr::V6(address) => {
            if let Some(address) = address.to_ipv4_mapped() {
                normalized[0] = 4;
                normalized[1..5].copy_from_slice(&address.octets());
                5
            } else {
                normalized[0] = 6;
                normalized[1..].copy_from_slice(&address.octets());
                normalized.len()
            }
        }
    };
    (normalized, length)
}

fn prune_state(state: &mut SourceState, now: Instant) {
    state
        .buckets
        .retain(|_, bucket| bucket.active > 0 || bucket.expires_at > now);
}

struct SourcePermit {
    admission: Arc<SourceAdmission>,
    key: Option<SourceKey>,
}

impl Drop for SourcePermit {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.admission.release(key);
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn limits() -> AdmissionLimits {
        AdmissionLimits {
            maximum_handshakes: 20,
            maximum_browsing_sessions: 20,
            maximum_connections_per_source: 2,
            maximum_source_buckets: 20,
            source_attempts_per_window: 2,
            source_attempt_burst: 2,
            source_attempt_window: Duration::from_millis(100),
        }
    }

    #[test]
    fn defaults_match_the_public_capacity_policy() {
        let limits = AdmissionLimits::default();
        assert_eq!(limits.maximum_handshakes, 32);
        assert_eq!(limits.maximum_browsing_sessions, 200);
        assert_eq!(limits.maximum_connections_per_source, 8);
        assert_eq!(limits.maximum_source_buckets, 4_096);
        assert_eq!(limits.source_attempts_per_window, 30);
        assert_eq!(limits.source_attempt_burst, 10);
        assert_eq!(limits.source_attempt_window, Duration::from_secs(60));
    }

    #[test]
    fn source_values_rotate_hourly_and_normalize_mapped_ipv4() {
        let source = SourceAdmission::new(&limits()).expect("admission");
        let address = Ipv4Addr::new(192, 0, 2, 9);
        let ipv4 = IpAddr::V4(address);
        let mapped = IpAddr::V6(address.to_ipv6_mapped());

        assert!(source.transform(ipv4, 10) == source.transform(mapped, 10));
        assert!(source.transform(ipv4, 10) != source.transform(ipv4, 11));
    }

    #[test]
    fn daily_visitor_sketch_deduplicates_and_discards_completed_day_registers() {
        let source = SourceAdmission::new(&limits()).expect("admission");
        let telemetry = Telemetry::new(&limits());
        let first_day = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * SECONDS_PER_UTC_DAY);
        let address = Ipv4Addr::new(192, 0, 2, 9);
        let first = source.observe_visitor(IpAddr::V4(address), first_day, &telemetry);
        let duplicate = source.observe_visitor(
            IpAddr::V6(address.to_ipv6_mapped()),
            first_day + Duration::from_secs(60),
            &telemetry,
        );
        assert_eq!(first.current, 1);
        assert_eq!(duplicate.current, 1);
        assert_eq!(duplicate.previous, 0);

        let next_day = source.observe_visitor(
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)),
            first_day + Duration::from_secs(SECONDS_PER_UTC_DAY),
            &telemetry,
        );
        assert_eq!(next_day.current, 1);
        assert_eq!(next_day.previous, 1);
        assert_eq!(
            lock_unpoisoned(&source.daily_visitors)
                .registers
                .iter()
                .filter(|register| **register != 0)
                .count(),
            1,
            "only the new day's sketch remains after rotation"
        );
    }

    #[test]
    fn monotonic_hourly_rotation_does_not_reset_live_source_limits() {
        let mut policy = limits();
        policy.maximum_connections_per_source = 10;
        policy.source_attempt_window = Duration::from_secs(60);
        let start = Instant::now();
        let source = Arc::new(SourceAdmission::new_at(&policy, start).expect("admission"));
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let before_rotation = start + SOURCE_KEY_ROTATION - Duration::from_millis(10);
        let after_rotation = start + SOURCE_KEY_ROTATION + Duration::from_millis(10);
        let first = source
            .admit(address, before_rotation)
            .expect("first attempt");
        let second = source
            .admit(address, before_rotation)
            .expect("second attempt");
        assert!(
            source.admit(address, after_rotation).is_none(),
            "hour rotation must not restore a depleted burst"
        );
        drop((first, second));
    }

    #[test]
    fn rotation_generation_is_immune_to_wall_clock_jumps() {
        let start = Instant::now();
        let source = SourceAdmission::new_at(&limits(), start).expect("admission");
        assert_eq!(source.generation_at(start), 0);
        assert_eq!(
            source.generation_at(start + SOURCE_KEY_ROTATION - Duration::from_nanos(1)),
            0
        );
        assert_eq!(source.generation_at(start + SOURCE_KEY_ROTATION), 1);
        assert_eq!(source.generation_at(start + SOURCE_KEY_ROTATION * 3), 3);
        assert_eq!(
            source.generation_at(
                start
                    .checked_sub(Duration::from_secs(1))
                    .expect("clock range")
            ),
            0,
            "a backward monotonic sample saturates rather than rotating keys"
        );

        let mut policy = limits();
        policy.source_attempt_window = SOURCE_KEY_ROTATION;
        let source = Arc::new(SourceAdmission::new_at(&policy, start).expect("admission"));
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let first = source.admit(address, start).expect("first attempt");
        let second = source.admit(address, start).expect("second attempt");
        drop((first, second));
        assert!(
            source
                .admit(address, start + SOURCE_KEY_ROTATION * 2)
                .is_some(),
            "a forward monotonic skip grants a new burst only after the full refill horizon"
        );
        // Wall time is intentionally absent from SourceAdmission; forward and
        // backward UTC adjustments cannot affect generation or rate state.
    }

    #[test]
    fn globally_rejected_attempts_still_spend_source_tokens() {
        let mut policy = limits();
        policy.maximum_handshakes = 1;
        policy.maximum_browsing_sessions = 1;
        policy.maximum_connections_per_source = 10;
        policy.source_attempts_per_window = 2;
        policy.source_attempt_burst = 2;
        let admission = Admission::new(policy).expect("admission");
        let peer = SocketAddr::from((Ipv4Addr::LOCALHOST, 1234));

        let held = admission.admit(peer).expect("first attempt spends a token");
        assert!(
            admission.admit(peer).is_none(),
            "the second attempt is globally rejected"
        );
        drop(held);
        assert!(
            admission.admit(peer).is_none(),
            "the globally rejected attempt must have spent the source's last token"
        );
    }

    #[test]
    fn global_permits_recover_when_an_owner_unwinds() {
        let mut policy = limits();
        policy.maximum_handshakes = 1;
        policy.maximum_browsing_sessions = 1;
        policy.maximum_connections_per_source = 10;
        policy.source_attempts_per_window = 10;
        policy.source_attempt_burst = 10;
        let admission = Admission::new(policy).expect("admission");
        let peer = SocketAddr::from((Ipv4Addr::LOCALHOST, 1234));

        let handshake = admission.admit(peer).expect("handshake permit");
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owned = handshake;
            panic!("simulated connection panic");
        }));
        assert!(panicked.is_err());

        let connection = admission.admit(peer).expect("recovered handshake permit");
        let session_admission = connection.session_admission();
        let session = session_admission.try_reserve().expect("session permit");
        assert!(session_admission.establish(session));
        let waiting = admission.admit(peer).expect("second handshake");
        assert!(waiting.session_admission().try_reserve().is_err());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owned = connection;
            panic!("simulated connection-owner panic");
        }));
        assert!(panicked.is_err());
        let recovered = waiting
            .session_admission()
            .try_reserve()
            .expect("recovered session permit");
        assert!(waiting.session_admission().establish(recovered));
    }

    #[test]
    fn validation_rejects_panicking_or_rate_weakening_limits() {
        for invalid in [
            AdmissionLimits {
                maximum_handshakes: Semaphore::MAX_PERMITS + 1,
                ..AdmissionLimits::default()
            },
            AdmissionLimits {
                maximum_browsing_sessions: Semaphore::MAX_PERMITS + 1,
                ..AdmissionLimits::default()
            },
            AdmissionLimits {
                source_attempts_per_window: 1,
                source_attempt_burst: 2,
                ..AdmissionLimits::default()
            },
            AdmissionLimits {
                source_attempt_window: Duration::from_secs(60 * 60 + 1),
                ..AdmissionLimits::default()
            },
        ] {
            assert!(matches!(
                Admission::new(invalid),
                Err(AdmissionError::InvalidLimit(_))
            ));
        }
    }

    #[test]
    fn source_table_rejects_new_buckets_at_its_hard_bound() {
        let mut policy = limits();
        policy.maximum_source_buckets = 2;
        let source = Arc::new(SourceAdmission::new(&policy).expect("admission"));
        let start = Instant::now();
        let first = source
            .admit(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), start)
            .expect("first source");
        let second = source
            .admit(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), start)
            .expect("second source");
        assert!(
            source
                .admit(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3)), start)
                .is_none()
        );
        assert_eq!(lock_unpoisoned(&source.state).buckets.len(), 2);
        drop((first, second));
    }

    #[test]
    fn many_source_flood_cannot_grow_the_bounded_table() {
        let mut policy = limits();
        policy.maximum_source_buckets = 64;
        policy.source_attempts_per_window = 100;
        policy.source_attempt_burst = 100;
        let source = Arc::new(SourceAdmission::new(&policy).expect("admission"));
        let start = Instant::now();
        for value in 0..10_000_u128 {
            let permit = source.admit(IpAddr::V6(value.into()), start);
            drop(permit);
        }
        assert_eq!(lock_unpoisoned(&source.state).buckets.len(), 64);
    }

    #[test]
    fn source_buckets_enforce_concurrency_refill_and_expiry() {
        let source = Arc::new(SourceAdmission::new(&limits()).expect("admission"));
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let start = Instant::now();
        let first = source.admit(address, start).expect("first connection");
        let second = source.admit(address, start).expect("second connection");
        assert!(source.admit(address, start).is_none());

        drop(first);
        assert!(
            source
                .admit(address, start + Duration::from_millis(50))
                .is_some(),
            "one token refills halfway through a two-attempt window"
        );
        drop(second);
        source.prune_expired(start + Duration::from_millis(151));
        assert!(lock_unpoisoned(&source.state).buckets.is_empty());
    }
}
