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
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout, timeout_at};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::browser::{Browser, CommandOutcome, ContentUpdate, FeedEntryLoad};
use crate::content::{
    Comment, ContentError, ContentSnapshot, ContentSource, FeedEntry, FeedEntryId,
};
use crate::hacker_news::HackerNewsContent;
use crate::input::{InputDecoder, Key};
use crate::ssh_transport::SshTransport;
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

const PTY_REQUIRED_MESSAGE: &[u8] =
    b"HNSSH requires an interactive terminal.\r\nReconnect with: ssh -t <hostname>\r\n";
const INPUT_RATE_LIMIT_MESSAGE: &[u8] =
    b"HNSSH closed this browsing session because its input rate stayed above 8 KiB/s.\r\n";

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub listen_address: SocketAddr,
    pub host_key_path: PathBuf,
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
    #[error("could not accept an SSH connection")]
    Accept(#[source] io::Error),
}

pub struct Server {
    listener: TcpListener,
    ssh_config: Arc<server::Config>,
    content: Arc<dyn ContentSource>,
}

impl Server {
    pub async fn bind(config: ServerConfig) -> Result<Self, ServerError> {
        Self::bind_with_content(config, Arc::new(HackerNewsContent::production())).await
    }

    pub async fn bind_with_content(
        config: ServerConfig,
        content: Arc<dyn ContentSource>,
    ) -> Result<Self, ServerError> {
        let host_key = load_secret_key(&config.host_key_path, None).map_err(|source| {
            ServerError::LoadHostKey {
                path: config.host_key_path.clone(),
                source,
            }
        })?;
        let listener = TcpListener::bind(config.listen_address)
            .await
            .map_err(|source| ServerError::Bind {
                address: config.listen_address,
                source,
            })?;

        Ok(Self {
            listener,
            ssh_config: ssh_config(host_key),
            content,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn run(self, shutdown: CancellationToken) -> Result<(), ServerError> {
        let mut sessions = JoinSet::new();

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                Some(joined) = sessions.join_next(), if !sessions.is_empty() => {
                    report_session_result(joined);
                }
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.map_err(ServerError::Accept)?;
                    let ssh_config = Arc::clone(&self.ssh_config);
                    let content = Arc::clone(&self.content);
                    let session_shutdown = shutdown.child_token();
                    sessions.spawn(async move {
                        run_connection(ssh_config, stream, content, session_shutdown).await
                    });
                }
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
            while sessions.join_next().await.is_some() {}
        }

        Ok(())
    }
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
) -> Result<(), SessionError> {
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    // run_stream only exchanges identification before spawning russh's task; dropping
    // RunningSession detaches that task. Keep a shutdown guard for cancellation and
    // stalled writes, including when Server::run aborts its connection tasks.
    let socket = stream.into_std()?;
    let transport = CloseTransport(socket.try_clone()?);
    let stream = SshTransport::new(TcpStream::from_std(socket)?);
    let scope = SessionScope::new();
    let (progress, mut changes) = watch::channel(SetupProgress::Authenticating);
    let running = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Ok(()),
        result = timeout_at(handshake_deadline, server::run_stream(
            config, stream, BrowsingSession::new(content, progress, &scope),
        )) => match result {
            Ok(result) => result?,
            // Before identification/key exchange there is no usable encrypted channel.
            Err(_) => return Ok(()),
        },
    };
    let handle = running.handle();
    tokio::pin!(running);

    let mut actor_finished = false;
    let mut close_deadline = None;
    let reason = loop {
        let (deadline, reason) = match *changes.borrow_and_update() {
            SetupProgress::Authenticating => (Some(handshake_deadline), HANDSHAKE_TIMEOUT_REASON),
            SetupProgress::Authenticated(at) => (Some(at + SETUP_TIMEOUT), SETUP_TIMEOUT_REASON),
            SetupProgress::ShellStarted => (close_deadline, "HNSSH browsing session ended"),
        };
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break "HNSSH is shutting down",
            result = &mut running => {
                scope.drain().await;
                return result;
            },
            _ = scope.ended.cancelled(), if !actor_finished => {
                actor_finished = true;
                if !scope.channel_closed.is_cancelled() {
                    close_deadline = Some(Instant::now() + SESSION_DRAIN_TIMEOUT);
                }
            },
            _ = scope.channel_closed.cancelled(), if close_deadline.is_some() => {
                close_deadline = None;
            },
            Ok(()) = changes.changed() => continue,
            _ = async {
                match deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => break reason,
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
        while tasks.join_next().await.is_some() {}
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
}

fn report_session_result(joined: Result<Result<(), SessionError>, tokio::task::JoinError>) {
    match joined {
        Ok(Ok(())) => debug!("browsing session ended"),
        Ok(Err(error)) => debug!(%error, "browsing session ended with an error"),
        Err(error) if error.is_cancelled() => {}
        Err(error) => warn!(%error, "browsing session task failed"),
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
}

impl SessionActor {
    fn new(
        channel: ChannelWriteHalf<server::Msg>,
        size: TerminalSize,
        source: Arc<dyn ContentSource>,
        handle: server::Handle,
    ) -> Result<Self, ActorError> {
        Ok(Self {
            size,
            source,
            top_feed_task: JoinSet::new(),
            pending_top_feed: None,
            load_tasks: JoinSet::new(),
            pending_loads: VecDeque::new(),
            browser: Browser::new(),
            output: TerminalOutput::new(size.area(), channel, handle)?,
        })
    }

    async fn run(
        mut self,
        mut keys: Receiver<Key>,
        mut sizes: watch::Receiver<TerminalSize>,
        stop: CancellationToken,
        input_excess: Arc<AtomicBool>,
    ) -> Result<(), ActorError> {
        let mut result = self.browse(&mut keys, &mut sizes, &stop).await;
        if input_excess.load(Ordering::Relaxed) {
            result = Err(ActorError::InputExcess);
        }
        self.top_feed_task.abort_all();
        self.load_tasks.abort_all();
        while self.top_feed_task.join_next().await.is_some() {}
        while self.load_tasks.join_next().await.is_some() {}
        let notice = if matches!(result, Err(ActorError::InputExcess)) {
            INPUT_RATE_LIMIT_MESSAGE
        } else {
            b""
        };
        self.output.finish(u32::from(result.is_err()), notice).await;
        result
    }

    async fn browse(
        &mut self,
        keys: &mut Receiver<Key>,
        sizes: &mut watch::Receiver<TerminalSize>,
        stop: &CancellationToken,
    ) -> Result<(), ActorError> {
        self.redraw()?;
        self.pending_top_feed = Some(ContentUpdate::Initial);
        let output_failed = self.output.failed();
        loop {
            self.start_loads();
            let event = tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                _ = output_failed.cancelled() => return Err(ActorError::OutputClosed),
                changed = sizes.changed() => {
                    if changed.is_err() { return Ok(()); }
                    self.size = *sizes.borrow_and_update();
                    self.browser.request_entry_reveal();
                    self.output.resize(self.size.area())?;
                    self.redraw()?;
                    continue;
                }
                key = keys.recv() => {
                    let Some(key) = key else { return Ok(()); };
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
                        CommandOutcome::Quit => return Ok(()),
                    }
                    continue;
                }
                joined = self.top_feed_task.join_next(), if !self.top_feed_task.is_empty() => {
                    match joined {
                        Some(Ok(event)) => event,
                        Some(Err(error)) => {
                            debug!(%error, "session top-feed task failed");
                            continue;
                        }
                        None => continue,
                    }
                }
                joined = self.load_tasks.join_next(), if !self.load_tasks.is_empty() => {
                    match joined {
                        Some(Ok(event)) => event,
                        Some(Err(error)) => {
                            debug!(%error, "session content task failed");
                            continue;
                        }
                        None => continue,
                    }
                }
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
        Ok(self.output.draw(|frame| ui::render(frame, browser, size))?)
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
    input_rate: InputRateLimiter,
    input: Option<InputSender>,
    input_done: Option<oneshot::Receiver<()>>,
    input_stop: CancellationToken,
    sizes: Option<watch::Sender<TerminalSize>>,
    output_channel: Option<ChannelWriteHalf<server::Msg>>,
    stop: CancellationToken,
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
    ) -> Self {
        Self {
            progress,
            channel: None,
            pty_size: None,
            shell_started: false,
            closed: false,
            content,
            input_rate: InputRateLimiter::new(),
            input: None,
            input_done: None,
            input_stop: scope.stop.child_token(),
            sizes: None,
            output_channel: None,
            stop: scope.stop.clone(),
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
    ) -> Result<(), SessionError> {
        let size = self.pty_size.unwrap_or(DEFAULT_SESSION_SIZE);
        self.shell_started = true;
        let output_channel = self
            .output_channel
            .take()
            .expect("accepted session channel");
        let actor = match SessionActor::new(
            output_channel,
            size,
            Arc::clone(&self.content),
            session.handle(),
        ) {
            Ok(actor) => actor,
            Err(error) => {
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
                if let Err(error) = actor.run(receiver, size_changes, stop, input_excess).await {
                    debug!(%error, "browsing session actor ended with an error");
                }
                ended.cancel();
            });
        }
        self.input = Some(input);
        self.input_done = Some(input_done);
        self.sizes = Some(sizes);
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

        self.start_shell(channel, session)
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
            // Data and EOF callbacks are ordered. Closing the bounded input
            // queue lets its decoder forward every admitted key before Stop.
            self.drain_input().await;
            self.stop_actor();
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
            self.abort_input();
            self.stop_actor();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
