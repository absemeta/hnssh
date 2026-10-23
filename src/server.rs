use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::layout::Rect;
use russh::keys::{PrivateKey, load_secret_key};
use russh::server::{self, Auth, Handler, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, ChannelWriteHalf, Disconnect, MethodSet, Pty};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until, timeout, timeout_at};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::admission::{
    Admission, AdmissionError, AdmissionLimits, ConnectionAdmission, SessionAdmission,
};
use crate::browser::{Browser, CommandOutcome, ContentUpdate, FeedEntryLoad};
use crate::content::{
    Comment, ContentError, ContentSnapshot, ContentSource, FeedEntry, FeedEntryId,
};
use crate::hacker_news::HackerNewsContent;
use crate::input::{InputDecoder, Key};
use crate::operations::Operations;
use crate::panic_reporting::report_join_error;
use crate::ssh_transport::SshTransport;
use crate::telemetry::{SessionClosureKind, SetupTimeoutKind, Telemetry};
use crate::terminal_output::TerminalOutput;
use crate::thread::{CommentLoad, CommentUpdate};
use crate::ui;

const DEFAULT_SESSION_SIZE: TerminalSize = TerminalSize {
    columns: 80,
    rows: 24,
};
const MAX_COLUMNS: u16 = 300;
const MAX_ROWS: u16 = 120;
const SESSION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const INPUT_ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);
const INPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const INPUT_BYTES_PER_SECOND: usize = 8 * 1024;
const INPUT_BURST_BYTES: usize = 16 * 1024;
const INPUT_QUEUE_BYTES: usize = INPUT_BURST_BYTES;
const INPUT_QUEUE_PACKETS: usize = 16;
const INPUT_QUEUE_KEYS: usize = 128;
const HANDSHAKE_TIMEOUT_REASON: &str =
    "HNSSH handshake timed out. Reconnect with an SSH client and authenticate within 10 seconds.";
const SETUP_TIMEOUT_REASON: &str =
    "HNSSH requires a PTY and shell within 10 seconds. Reconnect with: ssh -t <hostname>";
const TOP_FEED_PAGE_SIZE: usize = 30;
const PAGE_ITEM_CONCURRENCY: usize = 8;
const REJECTION_CONCURRENCY: usize = 32;
const MAX_CONFIGURED_SESSION_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const PTY_REQUIRED_MESSAGE: &[u8] =
    b"HNSSH requires an interactive terminal.\r\nReconnect with: ssh -t <hostname>\r\n";
const BUSY_MESSAGE: &[u8] = b"HNSSH is busy. Please try again shortly.\r\n";
const INPUT_RATE_LIMIT_MESSAGE: &[u8] =
    b"HNSSH closed this browsing session because its input rate stayed above 8 KiB/s.\r\n";
const RESTART_MESSAGE: &[u8] = b"HNSSH is restarting. Reconnect in a moment.\r\n";

#[derive(Clone, Debug)]
pub struct SessionTimeouts {
    pub input_idle: Duration,
    pub maximum_lifetime: Duration,
    pub warning_lead: Duration,
}

impl Default for SessionTimeouts {
    fn default() -> Self {
        Self {
            input_idle: Duration::from_secs(15 * 60),
            maximum_lifetime: Duration::from_secs(2 * 60 * 60),
            warning_lead: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub listen_address: SocketAddr,
    pub operations_address: SocketAddr,
    pub host_key_path: PathBuf,
    pub admission_limits: AdmissionLimits,
    pub session_timeouts: SessionTimeouts,
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("could not load SSH host key from {path}")]
    LoadHostKey {
        path: PathBuf,
        #[source]
        source: russh::keys::Error,
    },
    #[error("could not bind SSH listener at {address}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("could not configure inherited SSH listener")]
    ConfigureInheritedListener(#[source] io::Error),
    #[error("operations listener address must be loopback, got {0}")]
    PublicOperationsAddress(SocketAddr),
    #[error("could not bind operations listener at {address}")]
    BindOperations {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("could not accept an SSH connection")]
    Accept(#[source] io::Error),
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("invalid session timeout configuration: {0}")]
    InvalidSessionTimeout(&'static str),
}

pub struct Server {
    listener: TcpListener,
    operations: Operations,
    ssh_config: Arc<server::Config>,
    content: Arc<dyn ContentSource>,
    admission: Admission,
    session_timeouts: SessionTimeouts,
    telemetry: Telemetry,
}

impl Server {
    pub async fn bind(config: ServerConfig) -> Result<Self, ServerError> {
        let telemetry = Telemetry::new(&config.admission_limits);
        let content = Arc::new(HackerNewsContent::production_with_telemetry(
            telemetry.clone(),
        ));
        Self::bind_with_parts(config, content, telemetry, None).await
    }

    /// Starts HNSSH with a pre-bound TCP listener, such as one inherited from
    /// systemd socket activation. The listener is switched to nonblocking mode
    /// and otherwise follows the same admission and session path as a directly
    /// bound listener.
    pub async fn from_listener(
        config: ServerConfig,
        listener: std::net::TcpListener,
    ) -> Result<Self, ServerError> {
        let telemetry = Telemetry::new(&config.admission_limits);
        let content = Arc::new(HackerNewsContent::production_with_telemetry(
            telemetry.clone(),
        ));
        Self::bind_with_parts(config, content, telemetry, Some(listener)).await
    }

    pub async fn bind_with_content(
        config: ServerConfig,
        content: Arc<dyn ContentSource>,
    ) -> Result<Self, ServerError> {
        let telemetry = Telemetry::new(&config.admission_limits);
        Self::bind_with_parts(config, content, telemetry, None).await
    }

    /// Test/content-source counterpart to [`Server::from_listener`].
    pub async fn from_listener_with_content(
        config: ServerConfig,
        listener: std::net::TcpListener,
        content: Arc<dyn ContentSource>,
    ) -> Result<Self, ServerError> {
        let telemetry = Telemetry::new(&config.admission_limits);
        Self::bind_with_parts(config, content, telemetry, Some(listener)).await
    }

    async fn bind_with_parts(
        config: ServerConfig,
        content: Arc<dyn ContentSource>,
        telemetry: Telemetry,
        inherited_listener: Option<std::net::TcpListener>,
    ) -> Result<Self, ServerError> {
        if !config.operations_address.ip().is_loopback() {
            return Err(ServerError::PublicOperationsAddress(
                config.operations_address,
            ));
        }
        let host_key = load_secret_key(&config.host_key_path, None).map_err(|source| {
            ServerError::LoadHostKey {
                path: config.host_key_path.clone(),
                source,
            }
        })?;
        validate_session_timeouts(&config.session_timeouts)?;
        let admission = Admission::new_with_telemetry(config.admission_limits, telemetry.clone())?;
        let listener = match inherited_listener {
            Some(listener) => {
                listener
                    .set_nonblocking(true)
                    .map_err(ServerError::ConfigureInheritedListener)?;
                TcpListener::from_std(listener).map_err(ServerError::ConfigureInheritedListener)?
            }
            None => TcpListener::bind(config.listen_address)
                .await
                .map_err(|source| ServerError::Bind {
                    address: config.listen_address,
                    source,
                })?,
        };
        let operations = Operations::bind(config.operations_address, telemetry.clone())
            .await
            .map_err(|source| ServerError::BindOperations {
                address: config.operations_address,
                source,
            })?;

        Ok(Self {
            listener,
            operations,
            ssh_config: ssh_config(host_key),
            content,
            admission,
            session_timeouts: config.session_timeouts,
            telemetry,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn operations_addr(&self) -> SocketAddr {
        self.operations.local_addr()
    }

    pub async fn run(mut self, shutdown: CancellationToken) -> Result<(), ServerError> {
        let mut sessions = JoinSet::new();
        let mut rejections = JoinSet::new();
        let mut admission_cleanup = interval(self.admission.cleanup_interval());
        admission_cleanup.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.operations.set_ready(true);
        info!("HNSSH runtime ready");

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                Some(joined) = sessions.join_next(), if !sessions.is_empty() => {
                    report_session_result(joined);
                }
                Some(joined) = rejections.join_next(), if !rejections.is_empty() => {
                    if let Err(error) = joined {
                        report_join_error(error, "busy_rejection");
                    }
                }
                _ = admission_cleanup.tick() => self.admission.prune_expired(),
                accepted = self.listener.accept() => {
                    let (stream, peer) = accepted.map_err(ServerError::Accept)?;
                    let Some(connection_admission) = self.admission.admit(peer) else {
                        if rejections.len() < REJECTION_CONCURRENCY {
                            rejections.spawn(reject_busy(stream));
                        } else {
                            reject_busy_immediately(stream);
                        }
                        continue;
                    };
                    let ssh_config = Arc::clone(&self.ssh_config);
                    let content = Arc::clone(&self.content);
                    let session_shutdown = shutdown.child_token();
                    let session_timeouts = self.session_timeouts.clone();
                    let telemetry = self.telemetry.clone();
                    sessions.spawn(async move {
                        run_connection(
                            ssh_config,
                            stream,
                            content,
                            session_shutdown,
                            connection_admission,
                            session_timeouts,
                            telemetry,
                        )
                        .await
                    });
                }
            }
        }

        self.operations.set_ready(false);
        info!("HNSSH runtime draining");
        rejections.abort_all();
        while let Some(joined) = rejections.join_next().await {
            if let Err(error) = joined {
                report_join_error(error, "busy_rejection");
            }
        }

        if timeout(SESSION_DRAIN_TIMEOUT, async {
            while let Some(joined) = sessions.join_next().await {
                report_session_result(joined);
            }
        })
        .await
        .is_err()
        {
            sessions.abort_all();
            while let Some(joined) = sessions.join_next().await {
                report_session_result(joined);
            }
        }

        self.operations.shutdown().await;
        info!("HNSSH runtime stopped");
        Ok(())
    }
}

fn validate_session_timeouts(timeouts: &SessionTimeouts) -> Result<(), ServerError> {
    if timeouts.input_idle.is_zero() {
        return Err(ServerError::InvalidSessionTimeout(
            "input_idle must be greater than zero",
        ));
    }
    if timeouts.maximum_lifetime.is_zero() {
        return Err(ServerError::InvalidSessionTimeout(
            "maximum_lifetime must be greater than zero",
        ));
    }
    if timeouts.warning_lead.is_zero() {
        return Err(ServerError::InvalidSessionTimeout(
            "warning_lead must be greater than zero",
        ));
    }
    if timeouts.input_idle > MAX_CONFIGURED_SESSION_TIMEOUT
        || timeouts.maximum_lifetime > MAX_CONFIGURED_SESSION_TIMEOUT
    {
        return Err(ServerError::InvalidSessionTimeout(
            "timeout periods must not exceed seven days",
        ));
    }
    if timeouts.warning_lead >= timeouts.input_idle
        || timeouts.warning_lead >= timeouts.maximum_lifetime
    {
        return Err(ServerError::InvalidSessionTimeout(
            "warning_lead must be shorter than both timeout periods",
        ));
    }
    Ok(())
}

fn reject_busy_immediately(stream: TcpStream) {
    let _ = stream.try_write(BUSY_MESSAGE);
}

async fn reject_busy(mut stream: TcpStream) {
    // The caller admits this work only through a fixed-size JoinSet. Excess
    // peers therefore cannot create an unbounded task/buffer queue, and one
    // unwritable arrival cannot serialize later accepts.
    let _ = timeout(Duration::from_millis(100), async {
        stream.write_all(BUSY_MESSAGE).await?;
        stream.shutdown().await
    })
    .await;
}

fn ssh_config(host_key: PrivateKey) -> Arc<server::Config> {
    Arc::new(server::Config {
        // RFC 4252 forbids advertising "none". It is accepted directly by the handler.
        methods: MethodSet::empty(),
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![host_key],
        nodelay: true,
        ..Default::default()
    })
}

async fn run_connection(
    config: Arc<server::Config>,
    stream: TcpStream,
    content: Arc<dyn ContentSource>,
    shutdown: CancellationToken,
    admission: ConnectionAdmission,
    session_timeouts: SessionTimeouts,
    telemetry: Telemetry,
) -> Result<(), SessionError> {
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    // run_stream only exchanges identification before spawning russh's task; dropping
    // RunningSession detaches that task. Keep a shutdown guard for cancellation and
    // stalled writes, including when Server::run aborts its connection tasks.
    let socket = stream.into_std()?;
    let transport = CloseTransport(socket.try_clone()?);
    let stream = SshTransport::new(TcpStream::from_std(socket)?);
    let scope = SessionScope::new();
    let session_admission = admission.session_admission();
    let (progress, mut changes) = watch::channel(SetupProgress::Authenticating);
    let running = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Ok(()),
        result = timeout_at(handshake_deadline, server::run_stream(
            config,
            stream,
            BrowsingSession::new(
                content,
                progress,
                &scope,
                shutdown.clone(),
                session_admission,
                session_timeouts,
                telemetry.clone(),
            ),
        )) => match result {
            Ok(result) => result?,
            // Before identification/key exchange there is no usable encrypted channel.
            Err(_) => return Ok(()),
        },
    };
    let handle = running.handle();
    tokio::pin!(running);

    let reason = loop {
        let (deadline, reason) = match *changes.borrow_and_update() {
            SetupProgress::Authenticating => (Some(handshake_deadline), HANDSHAKE_TIMEOUT_REASON),
            SetupProgress::Authenticated(at) => (Some(at + SETUP_TIMEOUT), SETUP_TIMEOUT_REASON),
            SetupProgress::ShellStarted => (None, "HNSSH browsing session ended"),
            SetupProgress::Closing(reason) => (Some(Instant::now()), reason),
        };
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break "HNSSH is shutting down",
            result = &mut running => {
                scope.drain().await;
                return result;
            },
            _ = scope.ended.cancelled() => break "HNSSH browsing session ended",
            _ = scope.channel_closed.cancelled() => break "HNSSH browsing session ended",
            Ok(()) = changes.changed() => continue,
            _ = async {
                match deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                match *changes.borrow() {
                    SetupProgress::Authenticating => {
                        telemetry.setup_timed_out(SetupTimeoutKind::Handshake);
                    }
                    SetupProgress::Authenticated(_) => {
                        telemetry.setup_timed_out(SetupTimeoutKind::PtyAndShell);
                    }
                    SetupProgress::ShellStarted | SetupProgress::Closing(_) => {}
                }
                break reason;
            },
        }
    };

    // The SSH disconnect reason is available even without a session channel.
    // Never let an unresponsive peer extend teardown indefinitely.
    let result = timeout(SESSION_DRAIN_TIMEOUT, async {
        scope.drain().await;
        handle
            .disconnect(
                Disconnect::ByApplication,
                reason.to_owned(),
                "en".to_owned(),
            )
            .await?;
        running.as_mut().await
    })
    .await;
    drop(transport);
    match result {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

// The connection owns all session children even when Russh detaches its own
// task. Drop aborts children on forced server-future cancellation; ordinary
// teardown cancels and joins them before disconnecting the transport.
struct SessionScope {
    stop: CancellationToken,
    ended: CancellationToken,
    channel_closed: CancellationToken,
    tasks: Arc<Mutex<JoinSet<()>>>,
}

impl SessionScope {
    fn new() -> Self {
        Self {
            stop: CancellationToken::new(),
            ended: CancellationToken::new(),
            channel_closed: CancellationToken::new(),
            tasks: Arc::new(Mutex::new(JoinSet::new())),
        }
    }

    async fn drain(&self) {
        self.stop.cancel();
        let mut tasks = std::mem::take(&mut *self.tasks.lock().expect("session task scope"));
        while let Some(joined) = tasks.join_next().await {
            if let Err(error) = joined {
                report_join_error(error, "browsing_session");
            }
        }
    }
}

impl Drop for SessionScope {
    fn drop(&mut self) {
        self.stop.cancel();
        self.tasks.lock().expect("session task scope").abort_all();
    }
}

struct CloseTransport(std::net::TcpStream);

impl Drop for CloseTransport {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Clone, Copy)]
enum SetupProgress {
    Authenticating,
    Authenticated(Instant),
    ShellStarted,
    Closing(&'static str),
}

fn report_session_result(joined: Result<Result<(), SessionError>, tokio::task::JoinError>) {
    match joined {
        Ok(Ok(())) => debug!("browsing session ended"),
        Ok(Err(error)) => debug!(%error, "browsing session ended with an error"),
        Err(error) => report_join_error(error, "ssh_connection"),
    }
}

#[derive(Debug, Error)]
enum SessionError {
    #[error("SSH transport error")]
    Transport(#[from] io::Error),
    #[error("SSH protocol error")]
    Ssh(#[from] russh::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalSize {
    columns: u16,
    rows: u16,
}

impl TerminalSize {
    fn from_protocol(columns: u32, rows: u32, previous: Self) -> Self {
        Self {
            columns: clamp_dimension(columns, previous.columns, MAX_COLUMNS),
            rows: clamp_dimension(rows, previous.rows, MAX_ROWS),
        }
    }

    fn area(self) -> Rect {
        Rect::new(0, 0, self.columns, self.rows)
    }
}

fn clamp_dimension(value: u32, previous: u16, maximum: u16) -> u16 {
    if value == 0 {
        previous
    } else {
        value.min(u32::from(maximum)) as u16
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputRateDecision {
    Allow,
    Drop,
    Close,
}

struct InputRateLimiter {
    tokens: f64,
    excess: f64,
    updated_at: Instant,
}

impl InputRateLimiter {
    fn new() -> Self {
        Self::at(Instant::now())
    }

    fn at(now: Instant) -> Self {
        Self {
            tokens: INPUT_BURST_BYTES as f64,
            excess: 0.0,
            updated_at: now,
        }
    }

    fn check(&mut self, bytes: usize, now: Instant) -> InputRateDecision {
        let refill = now.saturating_duration_since(self.updated_at).as_secs_f64()
            * INPUT_BYTES_PER_SECOND as f64;
        self.updated_at = now;
        let debt_payment = refill.min(self.excess);
        self.excess -= debt_payment;
        self.tokens = (self.tokens + refill - debt_payment).min(INPUT_BURST_BYTES as f64);

        let bytes = bytes as f64;
        if bytes <= self.tokens {
            self.tokens -= bytes;
            return InputRateDecision::Allow;
        }

        self.excess += bytes - self.tokens;
        self.tokens = 0.0;
        if self.excess > INPUT_BURST_BYTES as f64 {
            InputRateDecision::Close
        } else {
            InputRateDecision::Drop
        }
    }
}

fn needs_background_revalidation<T>(
    result: &Result<ContentSnapshot<T>, ContentError>,
    update: ContentUpdate,
) -> bool {
    update == ContentUpdate::Initial
        && result
            .as_ref()
            .is_ok_and(|snapshot| snapshot.freshness.is_stale())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActorExit {
    Visitor,
    ServerShutdown,
}

enum ActorEvent {
    TopFeed {
        result: Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>,
        update: ContentUpdate,
    },
    FeedEntry {
        index: usize,
        id: FeedEntryId,
        result: Result<ContentSnapshot<FeedEntry>, ContentError>,
        update: ContentUpdate,
    },
    Comment {
        load: CommentLoad,
        result: Result<ContentSnapshot<Comment>, ContentError>,
    },
}

#[derive(Debug, Error)]
enum ActorError {
    #[error("terminal rendering error")]
    Terminal(#[from] io::Error),
    #[error("SSH output channel closed")]
    OutputClosed,
    #[error("session input rate exceeded")]
    InputExcess,
    #[error("browsing session reached its input-idle timeout")]
    InputIdleTimeout,
    #[error("browsing session reached its maximum lifetime")]
    MaximumLifetime,
    #[error("configured session deadline is outside the runtime clock range")]
    ClockRange,
}

#[derive(Clone)]
struct IdleActivity {
    state: Arc<Mutex<IdleActivityState>>,
    changed: watch::Sender<u64>,
    timeout: Duration,
}

struct IdleActivityState {
    deadline: Instant,
    last_activity: Instant,
    revision: u64,
    expired: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdleSnapshot {
    Active { now: Instant, deadline: Instant },
    Expired,
}

impl IdleActivity {
    fn new(timeout: Duration) -> Option<(Self, watch::Receiver<u64>)> {
        Self::new_at(timeout, Instant::now())
    }

    fn new_at(timeout: Duration, started_at: Instant) -> Option<(Self, watch::Receiver<u64>)> {
        let deadline = started_at.checked_add(timeout)?;
        let (changed, changes) = watch::channel(0);
        Some((
            Self {
                state: Arc::new(Mutex::new(IdleActivityState {
                    deadline,
                    last_activity: started_at,
                    revision: 0,
                    expired: false,
                })),
                changed,
                timeout,
            },
            changes,
        ))
    }

    fn record(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Taking the timestamp while holding the same lock as expiry makes
        // input and timeout linearizable at the deadline.
        let occurred_at = Instant::now();
        self.record_locked(&mut state, occurred_at)
    }

    #[cfg(test)]
    fn record_at(&self, occurred_at: Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.record_locked(&mut state, occurred_at)
    }

    fn record_locked(&self, state: &mut IdleActivityState, occurred_at: Instant) -> bool {
        if state.expired || occurred_at < state.last_activity || occurred_at > state.deadline {
            return false;
        }
        let Some(deadline) = occurred_at.checked_add(self.timeout) else {
            return false;
        };
        state.last_activity = occurred_at;
        state.deadline = deadline;
        state.revision = state.revision.wrapping_add(1);
        let revision = state.revision;
        let _ = self.changed.send_replace(revision);
        true
    }

    fn snapshot(&self) -> IdleSnapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        self.snapshot_locked(&mut state, now)
    }

    #[cfg(test)]
    fn snapshot_at(&self, now: Instant) -> IdleSnapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.snapshot_locked(&mut state, now)
    }

    fn snapshot_locked(&self, state: &mut IdleActivityState, now: Instant) -> IdleSnapshot {
        if state.expired || now >= state.deadline {
            state.expired = true;
            IdleSnapshot::Expired
        } else {
            IdleSnapshot::Active {
                now,
                deadline: state.deadline,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimeoutWarning {
    InputIdle,
    MaximumLifetime,
}

enum ContentJob {
    FeedEntry(FeedEntryLoad, ContentUpdate),
    Comment(CommentLoad),
}

struct SessionActor {
    size: TerminalSize,
    source: Arc<dyn ContentSource>,
    // Top-feed acquisition has one coalesced lane independent of the eight
    // item/comment futures/results. Returning from a slow thread must not
    // leave feed refresh queued behind work for that thread.
    top_feed_task: JoinSet<ActorEvent>,
    pending_top_feed: Option<ContentUpdate>,
    // Pending loads describe the content requested by the owned browsing
    // state, not output frames or semaphore-waiting tasks.
    load_tasks: JoinSet<ActorEvent>,
    pending_loads: VecDeque<ContentJob>,
    browser: Browser,
    output: TerminalOutput,
    timeouts: SessionTimeouts,
    timeout_warning: Option<TimeoutWarning>,
    shutdown: CancellationToken,
    telemetry: Telemetry,
}

impl SessionActor {
    fn new(
        channel: ChannelWriteHalf<server::Msg>,
        size: TerminalSize,
        source: Arc<dyn ContentSource>,
        handle: server::Handle,
        timeouts: SessionTimeouts,
        shutdown: CancellationToken,
        telemetry: Telemetry,
    ) -> Result<Self, ActorError> {
        Ok(Self {
            size,
            source,
            top_feed_task: JoinSet::new(),
            pending_top_feed: None,
            load_tasks: JoinSet::new(),
            pending_loads: VecDeque::new(),
            browser: Browser::new(),
            output: TerminalOutput::new(size.area(), channel, handle, telemetry.clone())?,
            timeouts,
            timeout_warning: None,
            shutdown,
            telemetry,
        })
    }

    async fn run(
        mut self,
        mut keys: Receiver<Key>,
        mut sizes: watch::Receiver<TerminalSize>,
        idle_activity: IdleActivity,
        mut activity_changes: watch::Receiver<u64>,
        stop: CancellationToken,
        input_excess: Arc<AtomicBool>,
    ) -> Result<(), ActorError> {
        let shutdown = self.shutdown.clone();
        let mut result = self
            .browse(
                &mut keys,
                &mut sizes,
                &idle_activity,
                &mut activity_changes,
                &shutdown,
                &stop,
            )
            .await;
        if input_excess.load(Ordering::Relaxed) {
            result = Err(ActorError::InputExcess);
        }
        match &result {
            Err(ActorError::InputIdleTimeout) => {
                self.telemetry.session_closed(SessionClosureKind::InputIdle)
            }
            Err(ActorError::MaximumLifetime) => self
                .telemetry
                .session_closed(SessionClosureKind::MaximumLifetime),
            Err(ActorError::InputExcess) => {
                self.telemetry.session_closed(SessionClosureKind::InputRate)
            }
            Err(ActorError::Terminal(_)) => self.telemetry.render_failed(),
            Ok(ActorExit::Visitor | ActorExit::ServerShutdown)
            | Err(ActorError::OutputClosed | ActorError::ClockRange) => {}
        }
        self.top_feed_task.abort_all();
        self.load_tasks.abort_all();
        while let Some(joined) = self.top_feed_task.join_next().await {
            if let Err(error) = joined {
                report_join_error(error, "session_top_feed");
            }
        }
        while let Some(joined) = self.load_tasks.join_next().await {
            if let Err(error) = joined {
                report_join_error(error, "session_content");
            }
        }
        let failed = result.is_err();
        let notice = match &result {
            Err(ActorError::InputExcess) => INPUT_RATE_LIMIT_MESSAGE.to_vec(),
            Err(ActorError::InputIdleTimeout) => format!(
                "HNSSH closed this browsing session after {} without input. Reconnect to continue.\r\n",
                duration_label(self.timeouts.input_idle),
            )
            .into_bytes(),
            Err(ActorError::MaximumLifetime) => format!(
                "HNSSH closed this browsing session at its {} maximum lifetime. Reconnect to continue.\r\n",
                duration_label(self.timeouts.maximum_lifetime),
            )
            .into_bytes(),
            Ok(ActorExit::ServerShutdown) => RESTART_MESSAGE.to_vec(),
            Ok(ActorExit::Visitor)
            | Err(ActorError::Terminal(_) | ActorError::OutputClosed | ActorError::ClockRange) => {
                Vec::new()
            }
        };
        self.output.finish(u32::from(failed), notice).await;
        result.map(|_| ())
    }

    async fn browse(
        &mut self,
        keys: &mut Receiver<Key>,
        sizes: &mut watch::Receiver<TerminalSize>,
        idle_activity: &IdleActivity,
        activity_changes: &mut watch::Receiver<u64>,
        shutdown: &CancellationToken,
        stop: &CancellationToken,
    ) -> Result<ActorExit, ActorError> {
        self.redraw()?;
        self.pending_top_feed = Some(ContentUpdate::Initial);
        let output_failed = self.output.failed();
        let started = Instant::now();
        let lifetime_deadline = started
            .checked_add(self.timeouts.maximum_lifetime)
            .ok_or(ActorError::ClockRange)?;
        loop {
            self.start_loads();
            let (now, idle_deadline) = match idle_activity.snapshot() {
                IdleSnapshot::Active { now, deadline } => (now, deadline),
                IdleSnapshot::Expired => return Err(ActorError::InputIdleTimeout),
            };
            if now >= lifetime_deadline {
                return Err(ActorError::MaximumLifetime);
            }
            let warning = active_timeout_warning(
                now,
                idle_deadline,
                lifetime_deadline,
                self.timeouts.warning_lead,
            );
            if warning != self.timeout_warning {
                self.timeout_warning = warning;
                self.redraw()?;
            }
            let timeout_transition = next_timeout_transition(
                now,
                idle_deadline,
                lifetime_deadline,
                self.timeouts.warning_lead,
            );
            let event = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return Ok(ActorExit::ServerShutdown),
                _ = stop.cancelled() => return Ok(ActorExit::Visitor),
                _ = output_failed.cancelled() => return Err(ActorError::OutputClosed),
                changed = activity_changes.changed() => {
                    if changed.is_err() { return Ok(ActorExit::Visitor); }
                    continue;
                }
                changed = sizes.changed() => {
                    if changed.is_err() { return Ok(ActorExit::Visitor); }
                    self.size = *sizes.borrow_and_update();
                    self.browser.request_entry_reveal();
                    self.output.resize(self.size.area())?;
                    self.redraw()?;
                    continue;
                }
                key = keys.recv() => {
                    let Some(key) = key else { return Ok(ActorExit::Visitor); };
                    let page_len = ui::navigation_page_len(self.browser.view_kind(), self.size.rows);
                    match self.browser.handle_key(key, page_len) {
                        CommandOutcome::Redraw | CommandOutcome::LoadComments => self.redraw()?,
                        CommandOutcome::RefreshTopFeed => {
                            self.redraw()?;
                            self.pending_top_feed = Some(ContentUpdate::Revalidation);
                        }
                        CommandOutcome::LoadNextPage => {
                            let entries = self.browser.load_next_page(TOP_FEED_PAGE_SIZE);
                            self.redraw()?;
                            self.load_entries(entries, ContentUpdate::Initial);
                        }
                        CommandOutcome::Unchanged => {}
                        CommandOutcome::Quit => return Ok(ActorExit::Visitor),
                    }
                    continue;
                }
                joined = self.top_feed_task.join_next(), if !self.top_feed_task.is_empty() => {
                    match joined {
                        Some(Ok(event)) => event,
                        Some(Err(error)) => {
                            report_join_error(error, "session_top_feed");
                            continue;
                        }
                        None => continue,
                    }
                }
                joined = self.load_tasks.join_next(), if !self.load_tasks.is_empty() => {
                    match joined {
                        Some(Ok(event)) => event,
                        Some(Err(error)) => {
                            report_join_error(error, "session_content");
                            continue;
                        }
                        None => continue,
                    }
                }
                _ = sleep_until(timeout_transition) => continue,
            };
            match event {
                ActorEvent::TopFeed { result, update } => {
                    let revalidate = needs_background_revalidation(&result, update);
                    let loaded = result.is_ok();
                    self.browser
                        .set_top_feed(result, TOP_FEED_PAGE_SIZE, update);
                    self.redraw()?;
                    if loaded {
                        self.load_entries(self.browser.loaded_entries(), update);
                    }
                    if revalidate {
                        self.browser.begin_top_feed_revalidation();
                        self.redraw()?;
                        self.pending_top_feed = Some(ContentUpdate::Revalidation);
                    }
                }
                ActorEvent::FeedEntry {
                    index,
                    id,
                    result,
                    update,
                } => {
                    let revalidate = needs_background_revalidation(&result, update);
                    self.browser.set_feed_entry(index, id, result, update);
                    if revalidate {
                        self.browser.begin_entry_revalidation();
                    }
                    self.redraw()?;
                    if revalidate {
                        self.pending_loads.push_back(ContentJob::FeedEntry(
                            FeedEntryLoad { index, id },
                            ContentUpdate::Revalidation,
                        ));
                    }
                }
                ActorEvent::Comment { load, result } => {
                    let revalidate = needs_background_revalidation(
                        &result,
                        match load.update {
                            CommentUpdate::Initial => ContentUpdate::Initial,
                            CommentUpdate::Revalidation => ContentUpdate::Revalidation,
                        },
                    );
                    self.browser.set_comment(load, result);
                    self.redraw()?;
                    if revalidate {
                        self.pending_loads
                            .push_back(ContentJob::Comment(CommentLoad {
                                update: CommentUpdate::Revalidation,
                                ..load
                            }));
                    }
                }
            }
        }
    }

    fn load_entries(&mut self, entries: Vec<FeedEntryLoad>, update: ContentUpdate) {
        self.pending_loads.extend(
            entries
                .into_iter()
                .map(|entry| ContentJob::FeedEntry(entry, update)),
        );
    }

    fn start_loads(&mut self) {
        if self.top_feed_task.is_empty()
            && let Some(update) = self.pending_top_feed.take()
        {
            let source = Arc::clone(&self.source);
            self.top_feed_task.spawn(async move {
                let result = match update {
                    ContentUpdate::Initial => source.top_feed().await,
                    ContentUpdate::Revalidation => source.refresh_top_feed().await,
                };
                ActorEvent::TopFeed { result, update }
            });
        }
        while self.load_tasks.len() < PAGE_ITEM_CONCURRENCY {
            let Some(job) = self.pending_loads.pop_front() else {
                break;
            };
            let source = Arc::clone(&self.source);
            self.load_tasks.spawn(async move {
                match job {
                    ContentJob::FeedEntry(FeedEntryLoad { index, id }, update) => {
                        let result = match update {
                            ContentUpdate::Initial => source.feed_entry(id).await,
                            ContentUpdate::Revalidation => source.refresh_feed_entry(id).await,
                        };
                        ActorEvent::FeedEntry {
                            index,
                            id,
                            result,
                            update,
                        }
                    }
                    ContentJob::Comment(load) => {
                        let result = match load.update {
                            CommentUpdate::Initial => source.comment(load.id).await,
                            CommentUpdate::Revalidation => source.refresh_comment(load.id).await,
                        };
                        ActorEvent::Comment { load, result }
                    }
                }
            });
        }
    }

    fn redraw(&mut self) -> Result<(), ActorError> {
        let candidates = self.render()?;
        let mut loads = self.browser.take_comment_loads();
        let (materialized, nearby_loads) = self.browser.materialize_current_comments(&candidates);
        loads.extend(nearby_loads);
        if materialized {
            self.render()?;
        }
        self.pending_loads
            .extend(loads.into_iter().map(ContentJob::Comment));
        Ok(())
    }

    fn render(&mut self) -> Result<Vec<usize>, ActorError> {
        let browser = &mut self.browser;
        let size = self.size.area();
        let warning = self.timeout_warning.map(|warning| match warning {
            TimeoutWarning::InputIdle => format!(
                "WARNING: input-idle timeout in {}; press any key to stay connected",
                duration_label(self.timeouts.warning_lead),
            ),
            TimeoutWarning::MaximumLifetime => format!(
                "WARNING: maximum session lifetime in {}; reconnect to continue",
                duration_label(self.timeouts.warning_lead),
            ),
        });
        Ok(self
            .output
            .draw(|frame| ui::render(frame, browser, size, warning.as_deref()))?)
    }
}

fn active_timeout_warning(
    now: Instant,
    idle_deadline: Instant,
    lifetime_deadline: Instant,
    warning_lead: Duration,
) -> Option<TimeoutWarning> {
    let mut warnings = Vec::new();
    if idle_deadline
        .checked_sub(warning_lead)
        .is_some_and(|warning_at| now >= warning_at)
    {
        warnings.push((idle_deadline, TimeoutWarning::InputIdle));
    }
    if lifetime_deadline
        .checked_sub(warning_lead)
        .is_some_and(|warning_at| now >= warning_at)
    {
        warnings.push((lifetime_deadline, TimeoutWarning::MaximumLifetime));
    }
    warnings
        .into_iter()
        .min_by_key(|(deadline, _)| *deadline)
        .map(|(_, warning)| warning)
}

fn next_timeout_transition(
    now: Instant,
    idle_deadline: Instant,
    lifetime_deadline: Instant,
    warning_lead: Duration,
) -> Instant {
    [
        idle_deadline.checked_sub(warning_lead),
        Some(idle_deadline),
        lifetime_deadline.checked_sub(warning_lead),
        Some(lifetime_deadline),
    ]
    .into_iter()
    .flatten()
    .filter(|transition| *transition > now)
    .min()
    .unwrap_or(now)
}

fn duration_label(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 && seconds.is_multiple_of(60) {
        let minutes = seconds / 60;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    } else if seconds > 0 {
        format!("{seconds} second{}", if seconds == 1 { "" } else { "s" })
    } else {
        format!("{} ms", duration.as_millis())
    }
}

#[derive(Clone)]
struct InputSender {
    packets: Sender<QueuedInput>,
    capacity: Arc<Semaphore>,
}

impl InputSender {
    async fn send(&self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() {
            return true;
        }
        let Ok(permits) = u32::try_from(bytes.len()) else {
            return false;
        };
        if bytes.len() > INPUT_QUEUE_BYTES {
            return false;
        }

        let capacity = Arc::clone(&self.capacity);
        let permit = tokio::select! {
            biased;
            _ = self.packets.closed() => return false,
            permit = capacity.acquire_many_owned(permits) => match permit {
                Ok(permit) => permit,
                Err(_) => return false,
            },
        };
        self.packets
            .send(QueuedInput {
                bytes,
                _capacity: permit,
            })
            .await
            .is_ok()
    }
}

struct QueuedInput {
    bytes: Vec<u8>,
    _capacity: OwnedSemaphorePermit,
}

fn input_decoder(
    events: Sender<Key>,
    stop: CancellationToken,
) -> (InputSender, impl std::future::Future<Output = ()> + Send) {
    let (packets, mut queued): (Sender<QueuedInput>, Receiver<QueuedInput>) =
        channel(INPUT_QUEUE_PACKETS);
    let input = InputSender {
        packets,
        capacity: Arc::new(Semaphore::new(INPUT_QUEUE_BYTES)),
    };
    let task = async move {
        let mut decoder = InputDecoder::default();
        loop {
            let packet = if decoder.is_waiting_for_escape_key() {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = events.closed() => break,
                    packet = queued.recv() => packet,
                    _ = tokio::time::sleep(INPUT_ESCAPE_TIMEOUT) => {
                        if let Some(key) = decoder.expire_escape_key() {
                            tokio::select! {
                                biased;
                                _ = stop.cancelled() => break,
                                sent = events.send(key) => if sent.is_err() { break; },
                            }
                        }
                        continue;
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = events.closed() => break,
                    packet = queued.recv() => packet,
                }
            };
            let Some(QueuedInput { bytes, .. }) = packet else {
                break;
            };
            for key in decoder.push(&bytes) {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => return,
                    sent = events.send(key) => if sent.is_err() { return; },
                }
            }
        }
    };
    (input, task)
}

struct BrowsingSession {
    progress: watch::Sender<SetupProgress>,
    channel: Option<ChannelId>,
    pty_size: Option<TerminalSize>,
    shell_started: bool,
    closed: bool,
    content: Arc<dyn ContentSource>,
    session_admission: SessionAdmission,
    session_timeouts: SessionTimeouts,
    telemetry: Telemetry,
    input_rate: InputRateLimiter,
    input: Option<InputSender>,
    activity: Option<IdleActivity>,
    input_done: Option<oneshot::Receiver<()>>,
    input_stop: CancellationToken,
    sizes: Option<watch::Sender<TerminalSize>>,
    output_channel: Option<ChannelWriteHalf<server::Msg>>,
    stop: CancellationToken,
    shutdown: CancellationToken,
    ended: CancellationToken,
    channel_closed: CancellationToken,
    input_excess: Arc<AtomicBool>,
    tasks: Arc<Mutex<JoinSet<()>>>,
}

impl BrowsingSession {
    fn new(
        content: Arc<dyn ContentSource>,
        progress: watch::Sender<SetupProgress>,
        scope: &SessionScope,
        shutdown: CancellationToken,
        session_admission: SessionAdmission,
        session_timeouts: SessionTimeouts,
        telemetry: Telemetry,
    ) -> Self {
        Self {
            progress,
            channel: None,
            pty_size: None,
            shell_started: false,
            closed: false,
            content,
            session_admission,
            session_timeouts,
            telemetry,
            input_rate: InputRateLimiter::new(),
            input: None,
            activity: None,
            input_done: None,
            input_stop: scope.stop.child_token(),
            sizes: None,
            output_channel: None,
            stop: scope.stop.clone(),
            shutdown,
            ended: scope.ended.clone(),
            channel_closed: scope.channel_closed.clone(),
            input_excess: Arc::new(AtomicBool::new(false)),
            tasks: Arc::clone(&scope.tasks),
        }
    }

    fn owns_channel(&self, channel: ChannelId) -> bool {
        self.channel == Some(channel)
    }

    fn start_shell(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
        session_permit: OwnedSemaphorePermit,
    ) -> Result<(), SessionError> {
        let size = self.pty_size.unwrap_or(DEFAULT_SESSION_SIZE);
        self.shell_started = true;
        let Some((activity, activity_changes)) =
            IdleActivity::new(self.session_timeouts.input_idle)
        else {
            return self.finish_without_actor(channel, 1, session);
        };
        let output_channel = self
            .output_channel
            .take()
            .expect("accepted session channel");
        let actor = match SessionActor::new(
            output_channel,
            size,
            Arc::clone(&self.content),
            session.handle(),
            self.session_timeouts.clone(),
            self.shutdown.clone(),
            self.telemetry.clone(),
        ) {
            Ok(actor) => actor,
            Err(error) => {
                self.telemetry.render_failed();
                debug!(%error, "could not initialize terminal");
                session.data(channel, crate::terminal_output::LEAVE_SCREEN.to_vec())?;
                return self.finish_without_actor(channel, 1, session);
            }
        };
        let (keys, receiver) = tokio::sync::mpsc::channel(INPUT_QUEUE_KEYS);
        let (sizes, size_changes) = watch::channel(size);
        let (input, decoder) = input_decoder(keys, self.input_stop.clone());
        let (done, input_done) = oneshot::channel();
        let stop = self.stop.clone();
        let ended = self.ended.clone();
        let input_excess = Arc::clone(&self.input_excess);
        let actor_activity = activity.clone();
        {
            let mut tasks = self.tasks.lock().expect("session task scope");
            if self.stop.is_cancelled() {
                // Teardown may race a late shell callback in Russh's task.
                // Checking under the scope lock prevents spawning past drain.
                drop(tasks);
                return self.finish_without_actor(channel, 1, session);
            }
            tasks.spawn(async move {
                decoder.await;
                let _ = done.send(());
            });
            tasks.spawn(async move {
                // Cancellation on drop covers ordinary completion and unwind;
                // the connection owner can then close even if the actor panics.
                let _ended = ended.drop_guard();
                if let Err(error) = actor
                    .run(
                        receiver,
                        size_changes,
                        actor_activity,
                        activity_changes,
                        stop,
                        input_excess,
                    )
                    .await
                {
                    debug!(%error, "browsing session actor ended with an error");
                }
            });
        }
        self.input = Some(input);
        self.activity = Some(activity);
        self.input_done = Some(input_done);
        self.sizes = Some(sizes);
        if !self.session_admission.establish(session_permit) {
            self.stop_actor();
            self.progress
                .send_replace(SetupProgress::Closing("HNSSH browsing session ended"));
            return Ok(());
        }
        self.progress.send_replace(SetupProgress::ShellStarted);
        Ok(())
    }

    fn resize(&mut self, size: TerminalSize) {
        self.pty_size = Some(size);
        if let Some(sizes) = &self.sizes {
            sizes.send_replace(size);
        }
    }

    fn finish_without_actor(
        &mut self,
        channel: ChannelId,
        status: u32,
        session: &mut Session,
    ) -> Result<(), SessionError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.progress
            .send_replace(SetupProgress::Closing("HNSSH browsing session ended"));
        session.exit_status_request(channel, status)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    fn finish_for_input_excess(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), SessionError> {
        if self.closed {
            return Ok(());
        }
        self.input_excess.store(true, Ordering::Relaxed);
        self.abort_input();
        if self.shell_started {
            self.closed = true;
            self.stop_actor();
            Ok(())
        } else {
            session.data(channel, INPUT_RATE_LIMIT_MESSAGE.to_vec())?;
            self.finish_without_actor(channel, 1, session)
        }
    }

    fn charge_inbound_bytes(
        &mut self,
        channel: ChannelId,
        bytes: usize,
        session: &mut Session,
    ) -> Result<bool, SessionError> {
        if !self.owns_channel(channel) || self.closed {
            return Ok(false);
        }

        match self.input_rate.check(bytes, Instant::now()) {
            InputRateDecision::Allow => Ok(true),
            InputRateDecision::Drop => Ok(false),
            InputRateDecision::Close => {
                self.finish_for_input_excess(channel, session)?;
                Ok(false)
            }
        }
    }

    async fn drain_input(&mut self) {
        self.input.take();
        if let Some(done) = self.input_done.take()
            && timeout(INPUT_DRAIN_TIMEOUT, done).await.is_err()
        {
            self.input_stop.cancel();
        }
    }

    fn abort_input(&mut self) {
        self.input.take();
        self.input_stop.cancel();
    }

    fn stop_actor(&mut self) {
        self.stop.cancel();
    }
}

impl Drop for BrowsingSession {
    fn drop(&mut self) {
        self.abort_input();
        self.stop_actor();
    }
}

impl Handler for BrowsingSession {
    type Error = SessionError;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_succeeded(&mut self, _session: &mut Session) -> Result<(), Self::Error> {
        self.progress
            .send_replace(SetupProgress::Authenticated(Instant::now()));
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.channel.is_some() {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
        } else {
            self.channel = Some(channel.id());
            let (_, output) = channel.split();
            self.output_channel = Some(output);
            reply.accept().await;
        }
        Ok(())
    }

    // No forwarding channel is part of a browsing session. Reject explicitly
    // rather than relying on the default Handler implementations.
    async fn channel_open_x11(
        &mut self,
        _channel: Channel<server::Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        _channel: Channel<server::Msg>,
        _host: &str,
        _port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn channel_open_forwarded_tcpip(
        &mut self,
        _channel: Channel<server::Msg>,
        _host: &str,
        _port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn channel_open_direct_streamlocal(
        &mut self,
        _channel: Channel<server::Msg>,
        _socket_path: &str,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        columns: u32,
        rows: u32,
        _pixel_width: u32,
        _pixel_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.owns_channel(channel)
            || self.pty_size.is_some()
            || self.shell_started
            || self.closed
        {
            session.channel_failure(channel)?;
            return Ok(());
        }

        let size = TerminalSize::from_protocol(columns, rows, DEFAULT_SESSION_SIZE);
        self.resize(size);
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.owns_channel(channel) || self.shell_started || self.closed {
            session.channel_failure(channel)?;
            return Ok(());
        }

        session.channel_success(channel)?;
        if self.pty_size.is_none() {
            session.data(channel, PTY_REQUIRED_MESSAGE.to_vec())?;
            return self.finish_without_actor(channel, 1, session);
        }

        let session_permit = match self.session_admission.try_reserve() {
            Ok(permit) => permit,
            Err(_) => {
                session.data(channel, BUSY_MESSAGE.to_vec())?;
                return self.finish_without_actor(channel, 1, session);
            }
        };
        self.start_shell(channel, session, session_permit)
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        columns: u32,
        rows: u32,
        _pixel_width: u32,
        _pixel_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.owns_channel(channel) || self.pty_size.is_none() || self.closed {
            session.channel_failure(channel)?;
            return Ok(());
        }

        let previous = self.pty_size.unwrap_or(DEFAULT_SESSION_SIZE);
        self.resize(TerminalSize::from_protocol(columns, rows, previous));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.charge_inbound_bytes(channel, data.len(), session)? || !self.shell_started {
            return Ok(());
        }
        if self
            .activity
            .as_ref()
            .is_some_and(|activity| !activity.record())
        {
            return Ok(());
        }
        if let Some(input) = self.input.clone() {
            let _ = input.send(data.to_vec()).await;
        }
        Ok(())
    }

    async fn extended_data(
        &mut self,
        channel: ChannelId,
        _code: u32,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = self.charge_inbound_bytes(channel, data.len(), session)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        _variable_name: &str,
        _variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        _auth_protocol: &str,
        _auth_cookie: &str,
        _screen_number: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn agent_request(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        // The pinned russh patch maps this to a channel reply, honoring want_reply.
        Ok(false)
    }

    async fn tcpip_forward(
        &mut self,
        _address: &str,
        _port: &mut u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        _address: &str,
        _port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    async fn streamlocal_forward(
        &mut self,
        _socket_path: &str,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    async fn cancel_streamlocal_forward(
        &mut self,
        _socket_path: &str,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.owns_channel(channel) && !self.closed {
            self.closed = true;
            // Data and EOF callbacks are ordered. Keep the actor alive while
            // closing and draining the bounded decoder queue; only then expose
            // closing to the connection owner, whose scope drain cancels tasks.
            self.drain_input().await;
            self.stop_actor();
            self.progress
                .send_replace(SetupProgress::Closing("HNSSH channel reached EOF"));
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.owns_channel(channel) {
            self.channel_closed.cancel();
            self.closed = true;
            self.progress
                .send_replace(SetupProgress::Closing("HNSSH channel closed"));
            self.abort_input();
            self.stop_actor();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tracing::instrument::WithSubscriber;

    use super::*;

    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn scoped_task_panic_emits_a_category_only_structured_warning() {
        const PRIVATE_PANIC: &str = "private-scoped-panic-payload";
        crate::panic_reporting::install_privacy_safe_panic_hook();
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&logs);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || CapturedWriter(Arc::clone(&captured)))
            .finish();
        let scope = SessionScope::new();
        scope
            .tasks
            .lock()
            .expect("session task scope")
            .spawn(async { panic!("{PRIVATE_PANIC}") });

        scope.drain().with_subscriber(subscriber).await;

        let output = String::from_utf8(
            logs.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        )
        .expect("UTF-8 logs");
        assert!(output.contains("asynchronous task failed"));
        assert!(output.contains("task.scope=\"browsing_session\""));
        assert!(output.contains("task.failure=\"panic\""));
        assert!(!output.contains(PRIVATE_PANIC));
    }

    #[test]
    fn default_session_timeouts_match_the_public_policy() {
        let timeouts = SessionTimeouts::default();
        assert_eq!(timeouts.input_idle, Duration::from_secs(15 * 60));
        assert_eq!(timeouts.maximum_lifetime, Duration::from_secs(2 * 60 * 60));
        assert_eq!(timeouts.warning_lead, Duration::from_secs(60));
    }

    #[test]
    fn session_timeout_validation_rejects_clock_range_and_warning_errors() {
        for invalid in [
            SessionTimeouts {
                input_idle: MAX_CONFIGURED_SESSION_TIMEOUT + Duration::from_secs(1),
                ..SessionTimeouts::default()
            },
            SessionTimeouts {
                maximum_lifetime: MAX_CONFIGURED_SESSION_TIMEOUT + Duration::from_secs(1),
                ..SessionTimeouts::default()
            },
            SessionTimeouts {
                warning_lead: Duration::from_secs(15 * 60),
                ..SessionTimeouts::default()
            },
        ] {
            assert!(matches!(
                validate_session_timeouts(&invalid),
                Err(ServerError::InvalidSessionTimeout(_))
            ));
        }
    }

    #[test]
    fn idle_activity_is_linearizable_and_preserves_coalesced_pre_deadline_input() {
        let start = Instant::now();
        let timeout = Duration::from_secs(10);
        let (activity, _changes) = IdleActivity::new_at(timeout, start).expect("clock range");

        assert!(activity.record_at(start + Duration::from_secs(9)));
        // This is after the original deadline but before the deadline extended
        // by the first event. A watch mailbox could overwrite the first event;
        // shared deadline state preserves and then extends it again.
        assert!(activity.record_at(start + Duration::from_secs(11)));
        assert_eq!(
            activity.snapshot_at(start + Duration::from_secs(12)),
            IdleSnapshot::Active {
                now: start + Duration::from_secs(12),
                deadline: start + Duration::from_secs(21),
            }
        );

        let (input_wins, _) = IdleActivity::new_at(timeout, start).expect("clock range");
        assert!(input_wins.record_at(start + timeout));
        assert!(matches!(
            input_wins.snapshot_at(start + timeout),
            IdleSnapshot::Active { .. }
        ));

        let (timeout_wins, _) = IdleActivity::new_at(timeout, start).expect("clock range");
        assert_eq!(
            timeout_wins.snapshot_at(start + timeout),
            IdleSnapshot::Expired
        );
        assert!(!timeout_wins.record_at(start + timeout));
    }

    #[test]
    fn protocol_dimensions_are_clamped_without_losing_the_last_usable_size() {
        let previous = TerminalSize {
            columns: 90,
            rows: 40,
        };

        assert_eq!(TerminalSize::from_protocol(0, 0, previous), previous);
        assert_eq!(
            TerminalSize::from_protocol(10_000, 10_000, previous),
            TerminalSize {
                columns: MAX_COLUMNS,
                rows: MAX_ROWS,
            }
        );
    }

    #[test]
    fn input_rate_limiter_tolerates_a_burst_and_recovers_at_eight_kibibytes_per_second() {
        let start = Instant::now();
        let mut limiter = InputRateLimiter::at(start);

        assert_eq!(
            limiter.check(INPUT_BURST_BYTES, start),
            InputRateDecision::Allow
        );
        assert_eq!(limiter.check(1, start), InputRateDecision::Drop);
        assert_eq!(
            limiter.check(INPUT_BYTES_PER_SECOND - 1, start + Duration::from_secs(1)),
            InputRateDecision::Allow
        );
        assert_eq!(
            limiter.check(INPUT_BYTES_PER_SECOND, start + Duration::from_secs(2)),
            InputRateDecision::Allow
        );
    }

    #[test]
    fn sustained_excess_cannot_reuse_refill_for_both_debt_and_tokens() {
        let start = Instant::now();
        let mut limiter = InputRateLimiter::at(start);

        assert_eq!(
            limiter.check(INPUT_BURST_BYTES, start),
            InputRateDecision::Allow
        );
        for second in 1..=4 {
            assert_eq!(
                limiter.check(12 * 1024, start + Duration::from_secs(second)),
                InputRateDecision::Drop
            );
        }
        assert_eq!(
            limiter.check(12 * 1024, start + Duration::from_secs(5)),
            InputRateDecision::Close
        );
    }

    #[test]
    fn input_rate_limiter_closes_repeated_excess_but_not_the_first_excess() {
        let start = Instant::now();
        let mut limiter = InputRateLimiter::at(start);

        assert_eq!(
            limiter.check(INPUT_BURST_BYTES, start),
            InputRateDecision::Allow
        );
        assert_eq!(
            limiter.check(INPUT_BURST_BYTES, start),
            InputRateDecision::Drop
        );
        assert_eq!(limiter.check(1, start), InputRateDecision::Close);
    }

    #[tokio::test]
    async fn input_decoder_stops_when_the_actor_event_receiver_closes() {
        let (events, actor_events) = channel(INPUT_QUEUE_KEYS);
        let (input, decoder) = input_decoder(events, CancellationToken::new());
        let task = tokio::spawn(decoder);
        drop(actor_events);

        timeout(Duration::from_secs(1), task)
            .await
            .expect("input decoder did not observe actor termination")
            .expect("input decoder task failed");
        assert!(!input.send(b"j".to_vec()).await);
    }
}
