use std::collections::{HashMap, VecDeque};
use std::fs;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hnssh::{
    AdmissionLimits, Comment, CommentDetails, ContentError, ContentSnapshot, ContentSource,
    EntryDetails, FeedEntry, FeedEntryId, FixtureContent, HackerNewsContent, HackerNewsOptions,
    Server, ServerConfig, ServerError, SessionTimeouts,
};
use russh::client;
use russh::keys::{Algorithm, PrivateKey, ssh_key};
use russh::{Channel, ChannelMsg, client::AuthResult};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Barrier, Semaphore, oneshot};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

#[path = "support/ssh_output.rs"]
mod output_acceptance;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct TrustExpectedHostKey {
    expected: ssh_key::PublicKey,
    disconnected: Option<oneshot::Sender<String>>,
}

impl client::Handler for TrustExpectedHostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(server_public_key == &self.expected)
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let message = match reason {
            client::DisconnectReason::ReceivedDisconnect(info) => info.message,
            client::DisconnectReason::Error(error) => format!("transport error: {error}"),
        };
        if let Some(sender) = self.disconnected.take() {
            let _ = sender.send(message);
        }
        Ok(())
    }
}

struct TestServer {
    address: SocketAddr,
    operations_address: SocketAddr,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), hnssh::ServerError>>,
    _key_dir: TempDir,
    public_key: ssh_key::PublicKey,
}

impl TestServer {
    async fn start() -> TestResult<Self> {
        Self::start_with_content(Arc::new(FixtureContent::default())).await
    }

    async fn start_with_content(content: Arc<dyn ContentSource>) -> TestResult<Self> {
        Self::start_with_policy(
            content,
            AdmissionLimits::default(),
            SessionTimeouts::default(),
        )
        .await
    }

    async fn start_with_policy(
        content: Arc<dyn ContentSource>,
        admission_limits: AdmissionLimits,
        session_timeouts: SessionTimeouts,
    ) -> TestResult<Self> {
        let (key_dir, host_key_path, public_key) = make_host_key()?;
        let server = Server::bind_with_content(
            ServerConfig {
                listen_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                operations_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                host_key_path,
                admission_limits,
                session_timeouts,
            },
            content,
        )
        .await?;
        let address = server.local_addr()?;
        let operations_address = server.operations_addr();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.run(shutdown.clone()));

        Ok(Self {
            address,
            operations_address,
            shutdown,
            task,
            _key_dir: key_dir,
            public_key,
        })
    }

    async fn connect_unauthenticated(
        &self,
    ) -> TestResult<(
        client::Handle<TrustExpectedHostKey>,
        oneshot::Receiver<String>,
    )> {
        self.connect_unauthenticated_from(None).await
    }

    async fn connect_unauthenticated_from(
        &self,
        source: Option<Ipv4Addr>,
    ) -> TestResult<(
        client::Handle<TrustExpectedHostKey>,
        oneshot::Receiver<String>,
    )> {
        let config = Arc::new(client::Config::default());
        let (sender, disconnected) = oneshot::channel();
        let handler = TrustExpectedHostKey {
            expected: self.public_key.clone(),
            disconnected: Some(sender),
        };
        let connection = if let Some(source) = source {
            let socket = tokio::net::TcpSocket::new_v4()?;
            socket.bind(SocketAddr::new(IpAddr::V4(source), 0))?;
            let stream = socket.connect(self.address).await?;
            timeout(
                TEST_TIMEOUT,
                client::connect_stream(config, stream, handler),
            )
            .await??
        } else {
            timeout(TEST_TIMEOUT, client::connect(config, self.address, handler)).await??
        };
        Ok((connection, disconnected))
    }

    async fn connect(&self) -> TestResult<client::Handle<TrustExpectedHostKey>> {
        let (mut connection, _) = self.connect_unauthenticated().await?;
        let result: AuthResult = connection.authenticate_none("ignored-visitor-name").await?;
        assert!(result.success(), "none authentication should succeed");
        Ok(connection)
    }

    async fn connect_unauthenticated_with_socket(
        &self,
    ) -> TestResult<(
        client::Handle<TrustExpectedHostKey>,
        oneshot::Receiver<String>,
        std::net::TcpStream,
    )> {
        let stream = TcpStream::connect(self.address).await?.into_std()?;
        let socket = stream.try_clone()?;
        let stream = TcpStream::from_std(stream)?;
        let (sender, disconnected) = oneshot::channel();
        let handler = TrustExpectedHostKey {
            expected: self.public_key.clone(),
            disconnected: Some(sender),
        };
        let connection = timeout(
            TEST_TIMEOUT,
            client::connect_stream(Arc::new(client::Config::default()), stream, handler),
        )
        .await??;
        Ok((connection, disconnected, socket))
    }

    async fn stop(self) -> TestResult {
        self.shutdown.cancel();
        timeout(TEST_TIMEOUT, self.task).await???;
        Ok(())
    }
}

struct GatedFixtureContent {
    fixture: FixtureContent,
    entry_gate: Arc<Semaphore>,
}

impl ContentSource for GatedFixtureContent {
    fn top_feed(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>>
                + Send
                + '_,
        >,
    > {
        self.fixture.top_feed()
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<FeedEntry>, ContentError>> + Send + '_>>
    {
        Box::pin(async move {
            let permit = self
                .entry_gate
                .acquire()
                .await
                .map_err(|_| ContentError::Unavailable)?;
            permit.forget();
            self.fixture.feed_entry(id).await
        })
    }
}

struct PendingCommentContent {
    started: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
}

struct CancellationProbe(Arc<AtomicUsize>);

impl Drop for CancellationProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl ContentSource for PendingCommentContent {
    fn top_feed(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async { Ok(ContentSnapshot::fresh(vec![1])) })
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<FeedEntry>, ContentError>> + Send + '_>>
    {
        Box::pin(async move {
            if id != 1 {
                return Err(ContentError::Unavailable);
            }
            Ok(ContentSnapshot::fresh(thread_story(
                id,
                "Cancellation story",
                vec![10],
            )))
        })
    }

    fn comment(
        &self,
        _id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<Comment>, ContentError>> + Send + '_>>
    {
        let started = Arc::clone(&self.started);
        let cancelled = Arc::clone(&self.cancelled);
        Box::pin(async move {
            started.fetch_add(1, Ordering::SeqCst);
            let _probe = CancellationProbe(cancelled);
            std::future::pending::<()>().await;
            unreachable!("pending comment load only ends by cancellation")
        })
    }
}

struct LimitFixtureContent {
    requested_comments: Arc<Mutex<Vec<FeedEntryId>>>,
}

impl LimitFixtureContent {
    fn request_count(&self, id: FeedEntryId) -> usize {
        self.requested_comments
            .lock()
            .expect("limit fixture request lock")
            .iter()
            .filter(|&&requested| requested == id)
            .count()
    }
}

impl ContentSource for LimitFixtureContent {
    fn top_feed(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async { Ok(ContentSnapshot::fresh(vec![1, 2])) })
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<FeedEntry>, ContentError>> + Send + '_>>
    {
        Box::pin(async move {
            let entry = match id {
                1 => thread_story(1, "Depth limit story", vec![10_000]),
                2 => thread_story(2, "Node limit story", (20_000..=21_000).collect()),
                _ => return Err(ContentError::Unavailable),
            };
            Ok(ContentSnapshot::fresh(entry))
        })
    }

    fn comment(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<Comment>, ContentError>> + Send + '_>>
    {
        let requested_comments = Arc::clone(&self.requested_comments);
        Box::pin(async move {
            requested_comments
                .lock()
                .expect("limit fixture request lock")
                .push(id);
            let (parent, author, child_ids) = if (10_000..=10_064).contains(&id) {
                (
                    if id == 10_000 { 1 } else { id - 1 },
                    format!("deep-author-{id}"),
                    if id < 10_064 {
                        vec![id + 1]
                    } else {
                        Vec::new()
                    },
                )
            } else if (20_000..=21_000).contains(&id) {
                (2, format!("wide-author-{id}"), Vec::new())
            } else {
                return Err(ContentError::Unavailable);
            };
            Ok(ContentSnapshot::fresh(Comment::Available(CommentDetails {
                id,
                parent: Some(parent),
                author: Some(author),
                published_at: None,
                text: None,
                child_ids,
                links: Vec::new(),
                omitted_link_count: 0,
            })))
        })
    }
}

fn thread_story(id: FeedEntryId, title: &str, comment_ids: Vec<FeedEntryId>) -> FeedEntry {
    FeedEntry::Story(EntryDetails {
        id,
        title: Some(title.to_owned()),
        source: None,
        score: None,
        author: None,
        published_at: None,
        comment_count: u32::try_from(comment_ids.len()).ok(),
        comment_ids,
        text: None,
        primary_link: None,
        links: Vec::new(),
        omitted_link_count: 0,
    })
}

#[derive(Clone)]
struct FakeReply {
    status: StatusCode,
    body: String,
    delay: Duration,
    retry_after: Option<String>,
}

impl FakeReply {
    fn json(body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            body: body.into(),
            delay: Duration::ZERO,
            retry_after: None,
        }
    }

    fn status(status: StatusCode) -> Self {
        Self {
            status,
            body: "null".to_owned(),
            delay: Duration::ZERO,
            retry_after: None,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn retry_after(mut self, value: impl Into<String>) -> Self {
        self.retry_after = Some(value.into());
        self
    }
}

struct FakeHackerNewsState {
    replies: Mutex<HashMap<String, VecDeque<FakeReply>>>,
    requests: Mutex<HashMap<String, usize>>,
    request_times: Mutex<HashMap<String, Vec<Instant>>>,
    active_items: AtomicUsize,
    maximum_active_items: AtomicUsize,
}

struct FakeHackerNews {
    address: SocketAddr,
    state: Arc<FakeHackerNewsState>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl FakeHackerNews {
    async fn start(routes: HashMap<String, Vec<FakeReply>>) -> TestResult<Self> {
        let state = Arc::new(FakeHackerNewsState {
            replies: Mutex::new(
                routes
                    .into_iter()
                    .map(|(path, replies)| (path, replies.into()))
                    .collect(),
            ),
            requests: Mutex::new(HashMap::new()),
            request_times: Mutex::new(HashMap::new()),
            active_items: AtomicUsize::new(0),
            maximum_active_items: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/v0/topstories.json", get(fake_top_feed))
            .route("/v0/item/{filename}", get(fake_item))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        });

        Ok(Self {
            address,
            state,
            shutdown,
            task,
        })
    }

    fn endpoint(&self) -> String {
        format!("http://{}/v0", self.address)
    }

    fn request_count(&self, path: &str) -> usize {
        self.state
            .requests
            .lock()
            .expect("fake request lock")
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    fn request_times(&self, path: &str) -> Vec<Instant> {
        self.state
            .request_times
            .lock()
            .expect("fake request-time lock")
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    fn maximum_active_items(&self) -> usize {
        self.state.maximum_active_items.load(Ordering::SeqCst)
    }

    async fn stop(self) -> TestResult {
        self.shutdown.cancel();
        timeout(TEST_TIMEOUT, self.task).await???;
        Ok(())
    }
}

async fn fake_top_feed(State(state): State<Arc<FakeHackerNewsState>>) -> Response {
    fake_response(state, "topstories".to_owned(), false).await
}

async fn fake_item(
    Path(filename): Path<String>,
    State(state): State<Arc<FakeHackerNewsState>>,
) -> Response {
    let Some(id) = filename.strip_suffix(".json") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    fake_response(state, format!("item/{id}"), true).await
}

async fn fake_response(
    state: Arc<FakeHackerNewsState>,
    path: String,
    count_as_item: bool,
) -> Response {
    *state
        .requests
        .lock()
        .expect("fake request lock")
        .entry(path.clone())
        .or_default() += 1;
    state
        .request_times
        .lock()
        .expect("fake request-time lock")
        .entry(path.clone())
        .or_default()
        .push(Instant::now());
    let reply = {
        let mut replies = state.replies.lock().expect("fake reply lock");
        replies.get_mut(&path).and_then(|replies| {
            if replies.len() > 1 {
                replies.pop_front()
            } else {
                replies.front().cloned()
            }
        })
    }
    .unwrap_or_else(|| FakeReply::status(StatusCode::NOT_FOUND));

    if count_as_item {
        let active = state.active_items.fetch_add(1, Ordering::SeqCst) + 1;
        state
            .maximum_active_items
            .fetch_max(active, Ordering::SeqCst);
        sleep(reply.delay).await;
        state.active_items.fetch_sub(1, Ordering::SeqCst);
    } else {
        sleep(reply.delay).await;
    }

    let mut response = (reply.status, reply.body).into_response();
    response.headers_mut().insert(
        "content-type",
        "application/json".parse().expect("static content type"),
    );
    if let Some(retry_after) = reply.retry_after {
        response.headers_mut().insert(
            "retry-after",
            retry_after.parse().expect("test Retry-After value"),
        );
    }
    response
}

fn make_host_key() -> TestResult<(TempDir, PathBuf, ssh_key::PublicKey)> {
    let key_dir = tempfile::tempdir()?;
    let path = key_dir.path().join("host_key");
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let public_key = key.public_key().clone();
    fs::write(&path, key.to_openssh(ssh_key::LineEnding::LF)?.as_bytes())?;
    Ok((key_dir, path, public_key))
}

async fn raw_connection_from(server: &TestServer, source: Ipv4Addr) -> TestResult<TcpStream> {
    raw_connection_to_from(server.address, source).await
}

async fn raw_connection_to_from(address: SocketAddr, source: Ipv4Addr) -> TestResult<TcpStream> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind(SocketAddr::new(IpAddr::V4(source), 0))?;
    Ok(socket.connect(address).await?)
}

async fn read_connection_greeting(stream: &mut TcpStream) -> TestResult<String> {
    let mut greeting = Vec::new();
    timeout(TEST_TIMEOUT, async {
        while !greeting.ends_with(b"\n") {
            let byte = stream.read_u8().await?;
            greeting.push(byte);
            if greeting.len() > 256 {
                return Err("connection greeting exceeded its bound".into());
            }
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(String::from_utf8(greeting)?)
}

async fn operations_get(address: SocketAddr, path: &str) -> TestResult<(StatusCode, String)> {
    let response = reqwest::get(format!("http://{address}{path}")).await?;
    let status = response.status();
    let body = response.text().await?;
    Ok((status, body))
}

async fn expect_success(channel: &mut Channel<client::Msg>) -> TestResult {
    loop {
        let message = timeout(TEST_TIMEOUT, channel.wait())
            .await?
            .ok_or("SSH channel closed before request success")?;
        match message {
            ChannelMsg::Success => return Ok(()),
            ChannelMsg::Failure => return Err("SSH channel request failed".into()),
            _ => {}
        }
    }
}

async fn read_until_visible(
    channel: &mut Channel<client::Msg>,
    terminal: &mut vt100::Parser,
    raw_output: &mut Vec<u8>,
    expected: &str,
) -> TestResult {
    read_until_screen_matches(channel, terminal, raw_output, expected, |screen| {
        screen.contains(expected)
    })
    .await
}

async fn read_until_screen_matches(
    channel: &mut Channel<client::Msg>,
    terminal: &mut vt100::Parser,
    raw_output: &mut Vec<u8>,
    description: &str,
    matches: impl Fn(&str) -> bool,
) -> TestResult {
    let waited = timeout(TEST_TIMEOUT, async {
        // The condition may already hold from earlier output; without this
        // check the loop would wait for a frame the server has no reason to
        // send, timing out on an already-correct screen.
        if matches(&terminal.screen().contents()) {
            return Ok::<_, Box<dyn std::error::Error>>(());
        }
        loop {
            let message = channel
                .wait()
                .await
                .ok_or("SSH channel closed before expected screen appeared")?;
            if let ChannelMsg::Data { data } = message {
                raw_output.extend_from_slice(&data);
                terminal.process(&data);
                if matches(&terminal.screen().contents()) {
                    return Ok::<_, Box<dyn std::error::Error>>(());
                }
            }
        }
    })
    .await;
    match waited {
        Ok(result) => result,
        Err(_) => Err(format!(
            "timed out waiting for {description}; current screen:\n{}",
            terminal.screen().contents()
        )
        .into()),
    }
}

fn row_metadata_contains(screen: &str, row: &str, metadata: &str) -> bool {
    let mut lines = screen.lines();
    while let Some(line) = lines.next() {
        if line.contains(row) {
            return lines.next().is_some_and(|line| line.contains(metadata));
        }
    }
    false
}

async fn read_until_row_metadata(
    channel: &mut Channel<client::Msg>,
    terminal: &mut vt100::Parser,
    raw_output: &mut Vec<u8>,
    row: &str,
    metadata: &str,
) -> TestResult {
    timeout(TEST_TIMEOUT, async {
        loop {
            let message = channel
                .wait()
                .await
                .ok_or("SSH channel closed before expected row metadata appeared")?;
            if let ChannelMsg::Data { data } = message {
                raw_output.extend_from_slice(&data);
                terminal.process(&data);
                if row_metadata_contains(&terminal.screen().contents(), row, metadata) {
                    return Ok::<_, Box<dyn std::error::Error>>(());
                }
            }
        }
    })
    .await??;
    Ok(())
}

async fn open_terminal(
    server: &TestServer,
) -> TestResult<(
    client::Handle<TrustExpectedHostKey>,
    Channel<client::Msg>,
    vt100::Parser,
    Vec<u8>,
)> {
    let (connection, channel, terminal, output, _) = open_terminal_observed(server).await?;
    Ok((connection, channel, terminal, output))
}

async fn open_terminal_from(
    server: &TestServer,
    source: Ipv4Addr,
) -> TestResult<(
    client::Handle<TrustExpectedHostKey>,
    Channel<client::Msg>,
    vt100::Parser,
    Vec<u8>,
)> {
    let (mut connection, _) = server.connect_unauthenticated_from(Some(source)).await?;
    assert!(
        connection
            .authenticate_none("ignored-visitor-name")
            .await?
            .success()
    );
    let mut channel = connection.channel_open_session().await?;
    channel
        .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;
    Ok((
        connection,
        channel,
        vt100::Parser::new(24, 80, 0),
        Vec::new(),
    ))
}

async fn open_terminal_observed(
    server: &TestServer,
) -> TestResult<(
    client::Handle<TrustExpectedHostKey>,
    Channel<client::Msg>,
    vt100::Parser,
    Vec<u8>,
    oneshot::Receiver<String>,
)> {
    let (mut connection, disconnected) = server.connect_unauthenticated().await?;
    assert!(
        connection
            .authenticate_none("ignored-visitor-name")
            .await?
            .success()
    );
    let mut channel = connection.channel_open_session().await?;
    channel
        .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;
    Ok((
        connection,
        channel,
        vt100::Parser::new(24, 80, 0),
        Vec::new(),
        disconnected,
    ))
}

async fn read_through_close(
    channel: &mut Channel<client::Msg>,
    raw_output: &mut Vec<u8>,
) -> TestResult<u32> {
    timeout(TEST_TIMEOUT, async {
        let mut exit_status = None;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => raw_output.extend_from_slice(&data),
                ChannelMsg::ExitStatus {
                    exit_status: status,
                } => exit_status = Some(status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        exit_status.ok_or_else(|| "server closed without an exit status".into())
    })
    .await?
}

#[tokio::test]
async fn operations_listener_transitions_to_ready_and_exposes_only_aggregate_health() -> TestResult
{
    let (_key_dir, host_key_path, _) = make_host_key()?;
    let server = Server::bind_with_content(
        ServerConfig {
            listen_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            operations_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            host_key_path,
            admission_limits: AdmissionLimits::default(),
            session_timeouts: SessionTimeouts::default(),
        },
        Arc::new(FixtureContent::default()),
    )
    .await?;
    let operations_address = server.operations_addr();

    let (status, body) = operations_get(operations_address, "/livez").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "live\n");
    let (status, body) = operations_get(operations_address, "/readyz").await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "not ready\n");

    let shutdown = CancellationToken::new();
    let task = tokio::spawn(server.run(shutdown.clone()));
    timeout(TEST_TIMEOUT, async {
        loop {
            let (status, _) = operations_get(operations_address, "/readyz").await?;
            if status == StatusCode::OK {
                return Ok::<_, Box<dyn std::error::Error>>(());
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;

    let private_metadata = "private-user-term-title-url-marker";
    let (status, metrics) = operations_get(
        operations_address,
        &format!("/metrics?ignored={private_metadata}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert!(metrics.contains("hnssh_build_info"));
    assert!(metrics.contains("hnssh_runtime_ready 1"));
    assert!(metrics.contains("hnssh_daily_visitors_estimate"));
    assert!(!metrics.contains(private_metadata));

    shutdown.cancel();
    timeout(TEST_TIMEOUT, task).await???;
    Ok(())
}

#[tokio::test]
async fn inherited_listener_uses_the_real_peer_for_source_admission() -> TestResult {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    let (_key_dir, host_key_path, _) = make_host_key()?;
    let server = Server::from_listener_with_content(
        ServerConfig {
            // Reusing this occupied address proves startup did not try to bind it.
            listen_address: address,
            operations_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            host_key_path,
            admission_limits: AdmissionLimits {
                maximum_connections_per_source: 1,
                source_attempts_per_window: 100,
                source_attempt_burst: 100,
                ..AdmissionLimits::default()
            },
            session_timeouts: SessionTimeouts::default(),
        },
        listener,
        Arc::new(FixtureContent::default()),
    )
    .await?;
    assert_eq!(server.local_addr()?, address);
    let operations_address = server.operations_addr();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(server.run(shutdown.clone()));

    let first_source = Ipv4Addr::LOCALHOST;
    let mut first = raw_connection_to_from(address, first_source).await?;
    assert!(
        read_connection_greeting(&mut first)
            .await?
            .starts_with("SSH-2.0-")
    );
    let mut same_source = raw_connection_to_from(address, first_source).await?;
    assert!(
        read_connection_greeting(&mut same_source)
            .await?
            .contains("busy")
    );

    // Linux configures the whole 127.0.0.0/8 loopback range. When available,
    // prove the inherited accept path distinguishes the actual second source.
    let other_source = Ipv4Addr::new(127, 0, 0, 2);
    let probe = tokio::net::TcpSocket::new_v4()?;
    let other = if probe
        .bind(SocketAddr::new(IpAddr::V4(other_source), 0))
        .is_ok()
    {
        drop(probe);
        let mut stream = raw_connection_to_from(address, other_source).await?;
        assert!(
            read_connection_greeting(&mut stream)
                .await?
                .starts_with("SSH-2.0-")
        );
        Some(stream)
    } else {
        None
    };

    let (_, metrics) = operations_get(operations_address, "/metrics").await?;
    assert!(metrics.contains("hnssh_admission_rejections_total{reason=\"source\"} 1"));

    drop((first, same_source, other));
    shutdown.cancel();
    timeout(TEST_TIMEOUT, task).await???;
    Ok(())
}

#[tokio::test]
async fn operations_metrics_report_capacity_rejection_without_source_values() -> TestResult {
    let limits = AdmissionLimits {
        maximum_handshakes: 1,
        maximum_browsing_sessions: 1,
        maximum_connections_per_source: 10,
        maximum_source_buckets: 20,
        source_attempts_per_window: 100,
        source_attempt_burst: 100,
        source_attempt_window: Duration::from_secs(60),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        limits,
        SessionTimeouts::default(),
    )
    .await?;
    let mut held = TcpStream::connect(server.address).await?;
    assert!(
        read_connection_greeting(&mut held)
            .await?
            .starts_with("SSH-2.0-")
    );
    let mut rejected = TcpStream::connect(server.address).await?;
    assert!(
        read_connection_greeting(&mut rejected)
            .await?
            .contains("busy")
    );

    let (_, metrics) = operations_get(server.operations_address, "/metrics").await?;
    assert!(metrics.contains("hnssh_active_handshakes 1"));
    assert!(metrics.contains("hnssh_admission_rejections_total{reason=\"handshake_capacity\"} 1"));
    assert!(metrics.contains("hnssh_connections_total{outcome=\"attempted\"} 2"));
    assert!(metrics.contains("hnssh_daily_visitors_estimate{period=\"current\"} 1"));
    assert!(!metrics.contains(&server.address.ip().to_string()));

    drop((held, rejected));
    let (connection, mut channel, _terminal, mut output) = timeout(TEST_TIMEOUT, async {
        loop {
            match open_terminal(&server).await {
                Ok(visitor) => return visitor,
                Err(_) => sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await?;
    let (_, metrics) = operations_get(server.operations_address, "/metrics").await?;
    assert!(metrics.contains("hnssh_active_handshakes 0"));
    assert!(metrics.contains("hnssh_active_sessions 1"));
    assert!(metrics.contains("hnssh_connections_total{outcome=\"session_accepted\"} 1"));

    channel.data_bytes(b"q".as_slice()).await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    drop(connection);
    timeout(TEST_TIMEOUT, async {
        loop {
            let (_, metrics) = operations_get(server.operations_address, "/metrics").await?;
            if metrics.contains("hnssh_active_sessions 0") {
                return Ok::<_, Box<dyn std::error::Error>>(());
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    server.stop().await
}

#[tokio::test]
async fn operations_listener_rejects_non_loopback_configuration() -> TestResult {
    let (_key_dir, host_key_path, _) = make_host_key()?;
    let result = Server::bind_with_content(
        ServerConfig {
            listen_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            operations_address: SocketAddr::from(([0, 0, 0, 0], 9090)),
            host_key_path,
            admission_limits: AdmissionLimits::default(),
            session_timeouts: SessionTimeouts::default(),
        },
        Arc::new(FixtureContent::default()),
    )
    .await;
    assert!(matches!(
        result,
        Err(ServerError::PublicOperationsAddress(_))
    ));
    Ok(())
}

#[tokio::test]
async fn invalid_operator_limits_return_typed_startup_errors_instead_of_panicking() -> TestResult {
    let (_key_dir, host_key_path, _) = make_host_key()?;
    let invalid_admission = [
        AdmissionLimits {
            maximum_handshakes: Semaphore::MAX_PERMITS + 1,
            ..AdmissionLimits::default()
        },
        AdmissionLimits {
            maximum_browsing_sessions: Semaphore::MAX_PERMITS + 1,
            ..AdmissionLimits::default()
        },
        AdmissionLimits {
            maximum_connections_per_source: Semaphore::MAX_PERMITS + 1,
            ..AdmissionLimits::default()
        },
        AdmissionLimits {
            maximum_source_buckets: 65_537,
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
    ];
    for admission_limits in invalid_admission {
        let result = Server::bind_with_content(
            ServerConfig {
                listen_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                operations_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                host_key_path: host_key_path.clone(),
                admission_limits,
                session_timeouts: SessionTimeouts::default(),
            },
            Arc::new(FixtureContent::default()),
        )
        .await;
        assert!(matches!(result, Err(ServerError::Admission(_))));
    }

    for session_timeouts in [
        SessionTimeouts {
            input_idle: Duration::from_secs(8 * 24 * 60 * 60),
            ..SessionTimeouts::default()
        },
        SessionTimeouts {
            maximum_lifetime: Duration::from_secs(u64::MAX),
            ..SessionTimeouts::default()
        },
        SessionTimeouts {
            warning_lead: Duration::from_secs(15 * 60),
            ..SessionTimeouts::default()
        },
    ] {
        let result = Server::bind_with_content(
            ServerConfig {
                listen_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                operations_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                host_key_path: host_key_path.clone(),
                admission_limits: AdmissionLimits::default(),
                session_timeouts,
            },
            Arc::new(FixtureContent::default()),
        )
        .await;
        assert!(matches!(result, Err(ServerError::InvalidSessionTimeout(_))));
    }
    Ok(())
}

#[tokio::test]
async fn global_handshake_limit_rejects_exact_excess_and_recovers_after_transport_loss()
-> TestResult {
    let limits = AdmissionLimits {
        maximum_handshakes: 32,
        maximum_browsing_sessions: 200,
        maximum_connections_per_source: 40,
        maximum_source_buckets: 100,
        source_attempts_per_window: 1_000,
        source_attempt_burst: 100,
        source_attempt_window: Duration::from_secs(60),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        limits,
        SessionTimeouts::default(),
    )
    .await?;
    let source = Ipv4Addr::LOCALHOST;
    let mut admitted = Vec::new();
    for _ in 0..32 {
        let mut stream = raw_connection_from(&server, source).await?;
        assert!(
            read_connection_greeting(&mut stream)
                .await?
                .starts_with("SSH-2.0-")
        );
        admitted.push(stream);
    }

    let rejected_at = Instant::now();
    let barrier = Arc::new(Barrier::new(21));
    let mut arrivals = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let barrier = Arc::clone(&barrier);
        let address = server.address;
        arrivals.spawn(async move {
            let socket = tokio::net::TcpSocket::new_v4()?;
            socket.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
            barrier.wait().await;
            socket.connect(address).await
        });
    }
    barrier.wait().await;
    let mut excess = Vec::new();
    while let Some(arrival) = arrivals.join_next().await {
        excess.push(arrival??);
    }
    for stream in &mut excess {
        assert_eq!(
            read_connection_greeting(stream).await?,
            "HNSSH is busy. Please try again shortly.\r\n"
        );
    }
    assert!(rejected_at.elapsed() < TEST_TIMEOUT);

    drop(excess);
    drop(admitted.pop());
    let recovered = timeout(TEST_TIMEOUT, async {
        loop {
            let mut stream = raw_connection_from(&server, source).await?;
            let greeting = read_connection_greeting(&mut stream).await?;
            if greeting.starts_with("SSH-2.0-") {
                return Ok::<_, Box<dyn std::error::Error>>(stream);
            }
            assert!(greeting.contains("busy"));
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    drop(recovered);
    drop(admitted);
    server.stop().await
}

#[tokio::test]
async fn globally_rejected_arrivals_still_consume_their_source_attempt_budget() -> TestResult {
    // The capacity barrier needs a second loopback source. Linux configures
    // the full 127.0.0.0/8; platforms such as macOS only configure
    // 127.0.0.1, so probe explicitly and skip where the environment cannot
    // express the scenario instead of assuming dual-stack behavior.
    let barrier_source = Ipv4Addr::new(127, 0, 0, 2);
    let probe = tokio::net::TcpSocket::new_v4()?;
    if probe
        .bind(SocketAddr::new(IpAddr::V4(barrier_source), 0))
        .is_err()
    {
        eprintln!(
            "the 127.0.0.2 loopback alias is unavailable; skipped the second-source accounting test"
        );
        return Ok(());
    }
    drop(probe);

    const SOURCE_ATTEMPTS: usize = 30;
    let limits = AdmissionLimits {
        maximum_handshakes: 1,
        maximum_browsing_sessions: 4,
        maximum_connections_per_source: SOURCE_ATTEMPTS + 5,
        maximum_source_buckets: 20,
        source_attempts_per_window: SOURCE_ATTEMPTS as u32,
        source_attempt_burst: SOURCE_ATTEMPTS as u32,
        source_attempt_window: Duration::from_secs(60),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        limits,
        SessionTimeouts::default(),
    )
    .await?;
    let target_source = Ipv4Addr::LOCALHOST;

    // The first target attempt holds the only global handshake slot without
    // ever starting a shell. Every remaining attempt is globally rejected
    // while source-first accounting must still spend the entire budget.
    let mut held = raw_connection_from(&server, target_source).await?;
    assert!(
        read_connection_greeting(&mut held)
            .await?
            .starts_with("SSH-2.0-")
    );
    for _ in 1..SOURCE_ATTEMPTS {
        let mut rejected = raw_connection_from(&server, target_source).await?;
        assert!(
            read_connection_greeting(&mut rejected)
                .await?
                .contains("busy")
        );
    }

    // Teardown barrier: half-close and observe the server's FIN. The
    // connection task has returned and released its handshake permit, so a
    // busy response below cannot be global overload.
    held.shutdown().await?;
    let mut eof = [0_u8; 64];
    timeout(TEST_TIMEOUT, async {
        loop {
            match held.read(&mut eof).await {
                Ok(0) | Err(_) => return Ok::<_, std::io::Error>(()),
                Ok(_) => {}
            }
        }
    })
    .await??;

    // Observable capacity barrier: an independent source can only establish a
    // full browsing session if global handshake capacity is free.
    let (barrier_connection, mut barrier, mut terminal, mut output) =
        timeout(TEST_TIMEOUT, async {
            loop {
                if let Ok(visitor) = open_terminal_from(&server, barrier_source).await {
                    return visitor;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
    read_until_visible(
        &mut barrier,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;

    let mut source_rejected = raw_connection_from(&server, target_source).await?;
    assert!(
        read_connection_greeting(&mut source_rejected)
            .await?
            .contains("busy"),
        "globally rejected arrivals did not consume the target source's attempt budget"
    );
    barrier.data_bytes(b"q".as_slice()).await?;
    assert_eq!(read_through_close(&mut barrier, &mut output).await?, 0);
    drop(barrier_connection);
    server.stop().await
}

#[tokio::test]
async fn per_source_concurrency_and_attempt_burst_are_exact_and_recover() -> TestResult {
    let limits = AdmissionLimits {
        maximum_handshakes: 10,
        maximum_browsing_sessions: 10,
        maximum_connections_per_source: 8,
        maximum_source_buckets: 20,
        source_attempts_per_window: 100,
        source_attempt_burst: 20,
        source_attempt_window: Duration::from_millis(300),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        limits,
        SessionTimeouts::default(),
    )
    .await?;
    let first_source = Ipv4Addr::new(127, 0, 0, 1);
    let mut admitted = Vec::new();
    for _ in 0..8 {
        let mut stream = raw_connection_from(&server, first_source).await?;
        assert!(
            read_connection_greeting(&mut stream)
                .await?
                .starts_with("SSH-2.0-")
        );
        admitted.push(stream);
    }
    let mut excess = raw_connection_from(&server, first_source).await?;
    assert!(
        read_connection_greeting(&mut excess)
            .await?
            .contains("busy")
    );

    drop(admitted.pop());
    sleep(Duration::from_millis(30)).await;
    let mut recovered = raw_connection_from(&server, first_source).await?;
    assert!(
        read_connection_greeting(&mut recovered)
            .await?
            .starts_with("SSH-2.0-")
    );

    drop(admitted);
    drop(recovered);
    server.stop().await?;

    let rate_limits = AdmissionLimits {
        maximum_handshakes: 10,
        maximum_browsing_sessions: 10,
        maximum_connections_per_source: 10,
        maximum_source_buckets: 20,
        source_attempts_per_window: 2,
        source_attempt_burst: 2,
        source_attempt_window: Duration::from_millis(200),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        rate_limits,
        SessionTimeouts::default(),
    )
    .await?;
    let mut first = raw_connection_from(&server, first_source).await?;
    let mut second = raw_connection_from(&server, first_source).await?;
    assert!(
        read_connection_greeting(&mut first)
            .await?
            .starts_with("SSH-2.0-")
    );
    assert!(
        read_connection_greeting(&mut second)
            .await?
            .starts_with("SSH-2.0-")
    );
    let mut burst_excess = raw_connection_from(&server, first_source).await?;
    assert!(
        read_connection_greeting(&mut burst_excess)
            .await?
            .contains("busy")
    );
    sleep(Duration::from_millis(120)).await;
    let mut refilled = raw_connection_from(&server, first_source).await?;
    assert!(
        read_connection_greeting(&mut refilled)
            .await?
            .starts_with("SSH-2.0-")
    );
    drop(first);
    drop(second);
    drop(refilled);
    server.stop().await
}

#[tokio::test]
async fn established_session_limit_rejects_excess_without_displacing_browsers_and_recovers()
-> TestResult {
    let limits = AdmissionLimits {
        maximum_handshakes: 10,
        maximum_browsing_sessions: 2,
        maximum_connections_per_source: 10,
        maximum_source_buckets: 20,
        source_attempts_per_window: 1_000,
        source_attempt_burst: 50,
        source_attempt_window: Duration::from_secs(60),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        limits,
        SessionTimeouts::default(),
    )
    .await?;
    let first = open_terminal(&server).await?;
    let second = open_terminal(&server).await?;
    let (first_connection, mut first_channel, mut first_terminal, mut first_output) = first;
    let (second_connection, mut second_channel, mut second_terminal, mut second_output) = second;
    let (first_ready, second_ready) = tokio::join!(
        read_until_visible(
            &mut first_channel,
            &mut first_terminal,
            &mut first_output,
            "Rust reaches the tiny terminal",
        ),
        read_until_visible(
            &mut second_channel,
            &mut second_terminal,
            &mut second_output,
            "Rust reaches the tiny terminal",
        ),
    );
    first_ready?;
    second_ready?;

    let third_connection = server.connect().await?;
    let mut third_channel = third_connection.channel_open_session().await?;
    third_channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut third_channel).await?;
    third_channel.request_shell(true).await?;
    expect_success(&mut third_channel).await?;
    let mut rejected_output = Vec::new();
    assert_eq!(
        read_through_close(&mut third_channel, &mut rejected_output).await?,
        1
    );
    assert!(String::from_utf8_lossy(&rejected_output).contains("HNSSH is busy"));

    first_channel.data_bytes(b"j".as_slice()).await?;
    read_until_visible(
        &mut first_channel,
        &mut first_terminal,
        &mut first_output,
        "> 2. Ask HN:",
    )
    .await?;
    second_channel.data_bytes(b"?".as_slice()).await?;
    read_until_visible(
        &mut second_channel,
        &mut second_terminal,
        &mut second_output,
        "Keyboard help",
    )
    .await?;

    first_channel.data_bytes(b"q".as_slice()).await?;
    assert_eq!(
        read_through_close(&mut first_channel, &mut first_output).await?,
        0
    );
    let (fourth_connection, mut fourth_channel, mut fourth_terminal, mut fourth_output) =
        open_terminal(&server).await?;
    read_until_visible(
        &mut fourth_channel,
        &mut fourth_terminal,
        &mut fourth_output,
        "Rust reaches the tiny terminal",
    )
    .await?;

    second_channel.data_bytes(b"q".as_slice()).await?;
    fourth_channel.data_bytes(b"q".as_slice()).await?;
    assert_eq!(
        read_through_close(&mut second_channel, &mut second_output).await?,
        0
    );
    assert_eq!(
        read_through_close(&mut fourth_channel, &mut fourth_output).await?,
        0
    );
    drop(first_connection);
    drop(second_connection);
    drop(third_connection);
    drop(fourth_connection);
    server.stop().await
}

#[tokio::test]
async fn cap_one_recovers_after_setup_end_actor_cancellation_and_abrupt_transport_loss()
-> TestResult {
    let limits = AdmissionLimits {
        maximum_handshakes: 1,
        maximum_browsing_sessions: 1,
        maximum_connections_per_source: 10,
        maximum_source_buckets: 20,
        source_attempts_per_window: 100,
        source_attempt_burst: 100,
        source_attempt_window: Duration::from_secs(60),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        limits,
        SessionTimeouts::default(),
    )
    .await?;

    for ending in ["eof", "close"] {
        let (mut connection, disconnected) = server.connect_unauthenticated().await?;
        assert!(
            connection
                .authenticate_none("setup-ending")
                .await?
                .success()
        );
        let channel = connection.channel_open_session().await?;
        if ending == "eof" {
            channel.eof().await?;
        } else {
            channel.close().await?;
        }
        timeout(TEST_TIMEOUT, disconnected).await??;
        drop((connection, channel));
        sleep(Duration::from_millis(20)).await;
    }

    let (mut no_pty, no_pty_disconnected) = server.connect_unauthenticated().await?;
    assert!(no_pty.authenticate_none("no-pty").await?.success());
    let mut no_pty_channel = no_pty.channel_open_session().await?;
    no_pty_channel.request_shell(true).await?;
    expect_success(&mut no_pty_channel).await?;
    let mut no_pty_output = Vec::new();
    assert_eq!(
        read_through_close(&mut no_pty_channel, &mut no_pty_output).await?,
        1
    );
    assert!(String::from_utf8_lossy(&no_pty_output).contains("interactive terminal"));
    timeout(TEST_TIMEOUT, no_pty_disconnected).await??;
    drop(no_pty);
    sleep(Duration::from_millis(20)).await;

    let (mut cancelled, cancelled_disconnected) = server.connect_unauthenticated().await?;
    assert!(
        cancelled
            .authenticate_none("cancelled-actor")
            .await?
            .success()
    );
    let mut cancelled_channel = cancelled.channel_open_session().await?;
    cancelled_channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut cancelled_channel).await?;
    cancelled_channel.request_shell(true).await?;
    expect_success(&mut cancelled_channel).await?;
    let mut cancelled_terminal = vt100::Parser::new(24, 80, 0);
    let mut cancelled_output = Vec::new();
    read_until_visible(
        &mut cancelled_channel,
        &mut cancelled_terminal,
        &mut cancelled_output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    cancelled_channel.close().await?;
    timeout(TEST_TIMEOUT, cancelled_disconnected).await??;
    drop(cancelled);
    sleep(Duration::from_millis(20)).await;

    let (mut lost, lost_disconnected, socket) =
        server.connect_unauthenticated_with_socket().await?;
    assert!(lost.authenticate_none("transport-loss").await?.success());
    let mut lost_channel = lost.channel_open_session().await?;
    lost_channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut lost_channel).await?;
    lost_channel.request_shell(true).await?;
    expect_success(&mut lost_channel).await?;
    let mut lost_terminal = vt100::Parser::new(24, 80, 0);
    let mut lost_output = Vec::new();
    read_until_visible(
        &mut lost_channel,
        &mut lost_terminal,
        &mut lost_output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    socket.shutdown(std::net::Shutdown::Both)?;
    timeout(TEST_TIMEOUT, lost_disconnected).await??;
    drop((lost, lost_channel));
    sleep(Duration::from_millis(20)).await;

    let (_replacement, mut replacement, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(
        &mut replacement,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    replacement.data_bytes(b"q".as_slice()).await?;
    assert_eq!(read_through_close(&mut replacement, &mut output).await?, 0);
    server.stop().await
}

#[tokio::test]
async fn barrier_race_admits_exact_capacity_and_rejects_twenty_concurrent_shells() -> TestResult {
    let limits = AdmissionLimits {
        maximum_handshakes: 32,
        maximum_browsing_sessions: 4,
        maximum_connections_per_source: 30,
        maximum_source_buckets: 40,
        source_attempts_per_window: 30,
        source_attempt_burst: 30,
        source_attempt_window: Duration::from_secs(60),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        limits,
        SessionTimeouts::default(),
    )
    .await?;
    let mut pending = Vec::new();
    for _ in 0..24 {
        let connection = server.connect().await?;
        let mut channel = connection.channel_open_session().await?;
        channel
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await?;
        expect_success(&mut channel).await?;
        pending.push((connection, channel));
    }

    enum RaceResult {
        Active(
            client::Handle<TrustExpectedHostKey>,
            Channel<client::Msg>,
            Vec<u8>,
        ),
        Rejected,
    }

    let barrier = Arc::new(Barrier::new(25));
    let mut racers = tokio::task::JoinSet::new();
    for (connection, mut channel) in pending {
        let barrier = Arc::clone(&barrier);
        racers.spawn(async move {
            barrier.wait().await;
            channel.request_shell(true).await.expect("request shell");
            let mut shell_accepted = false;
            let mut busy = false;
            let mut initial = Vec::new();
            loop {
                match timeout(TEST_TIMEOUT, channel.wait())
                    .await
                    .expect("race response timeout")
                    .expect("race channel closed without response")
                {
                    ChannelMsg::Success => shell_accepted = true,
                    ChannelMsg::Failure => panic!("shell request unexpectedly failed"),
                    ChannelMsg::Data { data } => {
                        assert!(shell_accepted, "data arrived before shell acceptance");
                        initial.extend_from_slice(&data);
                        if initial.starts_with(b"\x1b") {
                            return RaceResult::Active(connection, channel, initial);
                        }
                        if String::from_utf8_lossy(&initial).contains("HNSSH is busy") {
                            busy = true;
                        }
                    }
                    ChannelMsg::Close if busy => return RaceResult::Rejected,
                    ChannelMsg::Close => panic!("admitted shell closed without a screen"),
                    _ => {}
                }
            }
        });
    }
    barrier.wait().await;

    let mut active = Vec::new();
    let mut rejected = 0;
    while let Some(result) = racers.join_next().await {
        match result? {
            RaceResult::Active(connection, channel, output) => {
                active.push((connection, channel, output));
            }
            RaceResult::Rejected => rejected += 1,
        }
    }
    assert_eq!(active.len(), 4);
    assert_eq!(rejected, 20);

    let mut terminal = vt100::Parser::new(24, 80, 0);
    let (_, first_channel, first_output) = &mut active[0];
    terminal.process(first_output);
    read_until_visible(
        first_channel,
        &mut terminal,
        first_output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    first_channel.data_bytes(b"j".as_slice()).await?;
    read_until_visible(first_channel, &mut terminal, first_output, "> 2. Ask HN:").await?;
    for (_, channel, _) in &mut active {
        let _ = channel.data_bytes(b"q".as_slice()).await;
    }
    drop(active);
    server.stop().await
}

#[tokio::test]
async fn production_session_cap_holds_two_hundred_real_browsers_and_rejects_the_next() -> TestResult
{
    let limits = AdmissionLimits {
        maximum_handshakes: 32,
        maximum_browsing_sessions: 200,
        maximum_connections_per_source: 220,
        maximum_source_buckets: 300,
        source_attempts_per_window: 10_000,
        source_attempt_burst: 220,
        source_attempt_window: Duration::from_secs(60),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        limits,
        SessionTimeouts::default(),
    )
    .await?;
    let mut visitors = Vec::with_capacity(200);
    for _ in 0..200 {
        let connection = server.connect().await?;
        let mut channel = connection.channel_open_session().await?;
        channel
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await?;
        expect_success(&mut channel).await?;
        channel.request_shell(true).await?;
        expect_success(&mut channel).await?;
        visitors.push((connection, channel));
    }

    let excess_connection = server.connect().await?;
    let mut excess = excess_connection.channel_open_session().await?;
    excess.request_pty(true, "xterm", 80, 24, 0, 0, &[]).await?;
    expect_success(&mut excess).await?;
    excess.request_shell(true).await?;
    expect_success(&mut excess).await?;
    let mut excess_output = Vec::new();
    assert_eq!(
        read_through_close(&mut excess, &mut excess_output).await?,
        1
    );
    assert!(String::from_utf8_lossy(&excess_output).contains("HNSSH is busy"));

    let mut terminal = vt100::Parser::new(24, 80, 0);
    let mut output = Vec::new();
    read_until_visible(
        &mut visitors[0].1,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    visitors[0].1.data_bytes(b"j".as_slice()).await?;
    read_until_visible(
        &mut visitors[0].1,
        &mut terminal,
        &mut output,
        "> 2. Ask HN:",
    )
    .await?;

    visitors[0].1.data_bytes(b"q".as_slice()).await?;
    assert_eq!(
        read_through_close(&mut visitors[0].1, &mut output).await?,
        0
    );
    let (replacement_connection, mut replacement, mut replacement_terminal, mut replacement_output) =
        open_terminal(&server).await?;
    read_until_visible(
        &mut replacement,
        &mut replacement_terminal,
        &mut replacement_output,
        "Rust reaches the tiny terminal",
    )
    .await?;

    for (_, channel) in visitors.iter_mut().skip(1) {
        let _ = channel.data_bytes(b"q".as_slice()).await;
    }
    replacement.data_bytes(b"q".as_slice()).await?;
    drop(visitors);
    drop(excess_connection);
    drop(replacement_connection);
    server.stop().await
}

#[tokio::test]
async fn idle_warning_resets_on_input_then_timeout_releases_the_session_permit() -> TestResult {
    let limits = AdmissionLimits {
        maximum_browsing_sessions: 1,
        maximum_connections_per_source: 10,
        source_attempts_per_window: 1_000,
        source_attempt_burst: 50,
        ..AdmissionLimits::default()
    };
    let timeouts = SessionTimeouts {
        input_idle: Duration::from_secs(6),
        maximum_lifetime: Duration::from_secs(30),
        warning_lead: Duration::from_secs(3),
    };
    let server =
        TestServer::start_with_policy(Arc::new(FixtureContent::default()), limits, timeouts)
            .await?;
    let (_connection, mut channel, mut terminal, mut output, disconnected) =
        open_terminal_observed(&server).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "WARNING: input-idle timeout",
    )
    .await?;
    // Reset with a multi-second margin, then cross the old deadline. Exact
    // deadline ordering is covered deterministically by the policy unit test.
    sleep(Duration::from_secs(1)).await;
    channel.data_bytes(b"?".as_slice()).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut output,
        "input clears the idle warning",
        |screen| screen.contains("Keyboard help") && !screen.contains("WARNING:"),
    )
    .await?;
    sleep(Duration::from_secs(3)).await;
    channel.data_bytes(&[0x7f][..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut output, "Top feed").await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "WARNING: input-idle timeout",
    )
    .await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 1);
    assert!(String::from_utf8_lossy(&output).contains("without input"));
    timeout(TEST_TIMEOUT, disconnected).await??;
    drop(_connection);

    let (_replacement, mut replacement, mut replacement_terminal, mut replacement_output) =
        open_terminal(&server).await?;
    read_until_visible(
        &mut replacement,
        &mut replacement_terminal,
        &mut replacement_output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    replacement.data_bytes(b"q".as_slice()).await?;
    assert_eq!(
        read_through_close(&mut replacement, &mut replacement_output).await?,
        0
    );
    server.stop().await
}

#[tokio::test]
async fn maximum_lifetime_warns_and_closes_despite_recent_input() -> TestResult {
    let timeouts = SessionTimeouts {
        input_idle: Duration::from_secs(20),
        maximum_lifetime: Duration::from_secs(4),
        warning_lead: Duration::from_secs(2),
    };
    let server = TestServer::start_with_policy(
        Arc::new(FixtureContent::default()),
        AdmissionLimits {
            maximum_connections_per_source: 10,
            source_attempts_per_window: 1_000,
            source_attempt_burst: 50,
            ..AdmissionLimits::default()
        },
        timeouts,
    )
    .await?;
    let (_connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    sleep(Duration::from_secs(1)).await;
    channel.data_bytes(b"j".as_slice()).await?;
    read_until_visible(&mut channel, &mut terminal, &mut output, "> 2. Ask HN:").await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "WARNING: maximum session lifetime",
    )
    .await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 1);
    assert!(String::from_utf8_lossy(&output).contains("maximum lifetime"));
    server.stop().await
}

#[tokio::test]
async fn visitor_opens_resizes_and_quits_anonymous_screen_with_ctrl_c() -> TestResult {
    let server = TestServer::start().await?;
    let connection = server.connect().await?;
    let mut channel = connection.channel_open_session().await?;

    channel
        .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;

    let mut terminal = vt100::Parser::new(24, 80, 0);
    let mut raw_output = Vec::new();
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Top feed").await?;
    let screen = terminal.screen().contents();
    assert!(screen.contains("HNSSH"));
    assert!(screen.contains("Top feed"));
    assert!(screen.contains("q/Ctrl+C quit"));

    terminal.screen_mut().set_size(30, 100);
    channel.window_change(100, 30, 0, 0).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Top feed").await?;

    channel.data_bytes(&[0x03][..]).await?;
    let status = read_through_close(&mut channel, &mut raw_output).await?;
    assert_eq!(status, 0);
    assert!(raw_output.windows(4).any(|bytes| bytes == b"\x1b[0m"));
    assert!(raw_output.windows(8).any(|bytes| bytes == b"\x1b[?1049h"));
    assert!(raw_output.windows(8).any(|bytes| bytes == b"\x1b[?1049l"));
    assert!(raw_output.windows(6).any(|bytes| bytes == b"\x1b[?25h"));

    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn visitor_browses_fixture_details_links_and_help_without_losing_their_place() -> TestResult {
    let server = TestServer::start().await?;
    let connection = server.connect().await?;
    let mut channel = connection.channel_open_session().await?;

    channel
        .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;

    let mut terminal = vt100::Parser::new(24, 80, 0);
    let mut raw_output = Vec::new();
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    let screen = terminal.screen().contents();
    assert!(screen.contains("1. Rust reaches the tiny terminal"));
    assert!(screen.contains("example.com"));
    assert!(screen.contains("342 points"));
    assert!(screen.contains("ada"));
    assert!(screen.contains("2 hours ago"));
    assert!(screen.contains("88 comments"));

    channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;
    channel.data_bytes(&b"\x1b[A"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;
    channel.data_bytes(&b"\x1b[B"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;
    channel.data_bytes(&b"k"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;
    channel.data_bytes(&b"\x1b[6~"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "> 11.").await?;
    channel.data_bytes(&b"\x1b[5~"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;
    channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;

    channel.data_bytes(&b"?"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Keyboard help",
    )
    .await?;
    let help = terminal.screen().contents();
    for expected in [
        "Up / k",
        "Down / j",
        "Page Up / Page Down",
        "g / G",
        "Enter",
        "Escape / Backspace",
        "?",
        "q",
    ] {
        assert!(help.contains(expected), "help did not contain {expected:?}");
    }

    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;

    channel.data_bytes(&b"o"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "No primary external destination",
    )
    .await?;
    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;

    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Enter load/retry",
    )
    .await?;
    let detail = terminal.screen().contents();
    assert!(detail.contains("Ask HN: What are you building this week?"));
    assert!(!detail.contains("HNSSH · Entry"));
    assert!(detail.contains("? help · Enter load/retry · Esc back"));
    assert!(detail.contains("A fixture discussion for trying the detail view."));
    assert!(detail.contains("No comments available."));
    assert!(!detail.contains("View links"));

    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;

    channel.data_bytes(&b"G"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 12. The art of readable terminals",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Enter load/retry",
    )
    .await?;
    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 12. The art of readable terminals",
    )
    .await?;
    channel.data_bytes(&b"g"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);

    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn an_admitted_quit_is_processed_before_following_channel_eof() -> TestResult {
    let server = TestServer::start().await?;
    let (connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(&mut channel, &mut terminal, &mut output, "Top feed").await?;

    channel.data_bytes(&b"q"[..]).await?;
    channel.eof().await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    assert!(output.windows(8).any(|bytes| bytes == b"\x1b[?1049l"));

    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn fragmented_escape_and_utf8_input_preserve_following_navigation() -> TestResult {
    let server = TestServer::start().await?;
    let (connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;

    for fragment in [b"\x1b".as_slice(), b"[", b"B"] {
        channel.data_bytes(fragment).await?;
    }
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;

    let euro = "€".as_bytes();
    channel.data_bytes(&euro[..1]).await?;
    channel.data_bytes(&euro[1..2]).await?;
    let mut final_utf8_and_navigation = euro[2..].to_vec();
    final_utf8_and_navigation.extend_from_slice(b"\x1b[A");
    channel.data_bytes(final_utf8_and_navigation).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;

    channel.data_bytes(&[0xf0, 0x80, 0x80, b'j'][..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn overlong_unterminated_input_is_bounded_and_recovers_for_navigation() -> TestResult {
    const PENDING_INPUT_LIMIT: usize = 8 * 1024;

    let server = TestServer::start().await?;
    let (connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;

    let mut unterminated = Vec::with_capacity(PENDING_INPUT_LIMIT);
    unterminated.extend_from_slice(b"\x1b[");
    unterminated.resize(PENDING_INPUT_LIMIT, b'1');
    channel.data_bytes(unterminated).await?;
    channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;

    channel.data_bytes(&b"\x1b[12\xffk"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn a_sixteen_kibibyte_input_burst_is_tolerated() -> TestResult {
    let server = TestServer::start().await?;
    let (connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;

    channel.data_bytes(vec![b'x'; 16 * 1024]).await?;
    sleep(Duration::from_millis(20)).await;
    channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn extended_data_is_charged_to_the_session_input_budget() -> TestResult {
    let server = TestServer::start().await?;
    let (connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(&mut channel, &mut terminal, &mut output, "Top feed").await?;

    for _ in 0..4 {
        if channel
            .extended_data_bytes(1, vec![b'x'; 16 * 1024])
            .await
            .is_err()
        {
            break;
        }
    }
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 1);
    assert!(String::from_utf8_lossy(&output).contains("input rate stayed above 8 KiB/s"));

    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn sustained_input_excess_closes_only_the_abusive_session() -> TestResult {
    let server = TestServer::start().await?;
    let abusive = open_terminal(&server).await?;
    let healthy = open_terminal(&server).await?;
    let (abusive_connection, mut abusive_channel, mut abusive_terminal, mut abusive_output) =
        abusive;
    let (healthy_connection, mut healthy_channel, mut healthy_terminal, mut healthy_output) =
        healthy;

    let (abusive_ready, healthy_ready) = tokio::join!(
        read_until_visible(
            &mut abusive_channel,
            &mut abusive_terminal,
            &mut abusive_output,
            "Rust reaches the tiny terminal",
        ),
        read_until_visible(
            &mut healthy_channel,
            &mut healthy_terminal,
            &mut healthy_output,
            "Rust reaches the tiny terminal",
        )
    );
    abusive_ready?;
    healthy_ready?;

    for _ in 0..4 {
        if abusive_channel
            .data_bytes(vec![b'x'; 16 * 1024])
            .await
            .is_err()
        {
            break;
        }
    }
    assert_eq!(
        read_through_close(&mut abusive_channel, &mut abusive_output).await?,
        1
    );
    assert!(String::from_utf8_lossy(&abusive_output).contains("input rate stayed above 8 KiB/s"));

    healthy_channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(
        &mut healthy_channel,
        &mut healthy_terminal,
        &mut healthy_output,
        "> 2. Ask HN: What are you building this week?",
    )
    .await?;
    healthy_channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(
        read_through_close(&mut healthy_channel, &mut healthy_output).await?,
        0
    );

    abusive_connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    healthy_connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn loading_fixture_entries_do_not_move_the_visitors_selection() -> TestResult {
    let gate = Arc::new(Semaphore::new(0));
    let content = Arc::new(GatedFixtureContent {
        fixture: FixtureContent::default(),
        entry_gate: Arc::clone(&gate),
    });
    let server = TestServer::start_with_content(content).await?;
    let connection = server.connect().await?;
    let mut channel = connection.channel_open_session().await?;

    channel
        .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;

    let mut terminal = vt100::Parser::new(24, 80, 0);
    let mut raw_output = Vec::new();
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 1. Loading feed entry",
    )
    .await?;
    channel.data_bytes(&b"jj"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 3. Loading feed entry",
    )
    .await?;

    gate.add_permits(12);
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 3. Acme is hiring terminal-minded engineers",
    )
    .await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn a_small_terminal_can_open_help_then_resize_into_the_feed() -> TestResult {
    let server = TestServer::start().await?;
    let connection = server.connect().await?;
    let mut channel = connection.channel_open_session().await?;

    channel
        .request_pty(true, "xterm-256color", 50, 10, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;

    let mut terminal = vt100::Parser::new(10, 50, 0);
    let mut raw_output = Vec::new();
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Terminal too small",
    )
    .await?;
    let resize = terminal.screen().contents();
    assert!(resize.contains("60 x 16"));
    assert!(resize.contains("? help"));
    assert!(resize.contains("q/Ctrl+C quit"));

    channel.data_bytes(&b"?"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Keyboard help",
    )
    .await?;
    channel.data_bytes(&[0x1b][..]).await?;
    terminal.screen_mut().set_size(24, 80);
    channel.window_change(80, 24, 0, 0).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Top feed").await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn live_feed_loads_progressively_in_upstream_order_with_bounded_item_work() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "topstories".to_owned(),
        vec![FakeReply::json(format!(
            "[{}]",
            (1..=35)
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))],
    );
    for id in 1..=35 {
        let delay = if id == 1 {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(50)
        };
        let body = match id {
            2 => r#"{"id":2,"type":"story","title":"Second live entry","by":"sparse","unknown":"ignored"}"#.to_owned(),
            3 => "null".to_owned(),
            4 => "{malformed".to_owned(),
            5 => continue,
            6 => r#"{"id":6,"type":"story","title":7,"url":false,"by":["wrong shape"]}"#.to_owned(),
            _ => format!(
                r#"{{"id":{id},"type":"story","title":"Live entry {id}","url":"https://example.com/{id}","score":{id},"by":"visitor{id}","time":1700000000,"descendants":{id}}}"#
            ),
        };
        routes.insert(
            format!("item/{id}"),
            vec![FakeReply::json(body).delayed(delay)],
        );
    }
    routes.insert(
        "item/5".to_owned(),
        vec![FakeReply::status(StatusCode::SERVICE_UNAVAILABLE).delayed(Duration::from_millis(50))],
    );
    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint(fake.endpoint()));
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 1. Loading feed entry",
    )
    .await?;
    let placeholders = terminal.screen().contents();
    for rank in 1..=10 {
        assert!(
            placeholders.contains(&format!("{rank}. Loading feed entry")),
            "visible rank {rank} should have a stable placeholder"
        );
    }
    channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 2. Second live entry",
    )
    .await?;
    assert!(
        terminal
            .screen()
            .contents()
            .contains("1. Loading feed entry"),
        "a slow first item should not block a later sibling"
    );
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "6. [title unavailable]",
    )
    .await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "5. [feed entry temporarily unavailable]",
    )
    .await?;
    let screen = terminal.screen().contents();
    assert!(screen.contains("> 2. Second live entry"));
    assert!(screen.contains("2. Second live entry"));
    let sparse_metadata = screen
        .lines()
        .find(|line| line.contains("sparse"))
        .expect("sparse entry metadata");
    assert!(!sparse_metadata.contains("points"));
    assert!(screen.contains("3. [feed entry unavailable]"));
    assert!(screen.contains("5. [feed entry temporarily unavailable]"));
    assert!(screen.contains("6. [title unavailable]"));
    assert_eq!(
        fake.request_count("item/31"),
        0,
        "only the first page loads"
    );
    assert!(fake.maximum_active_items() <= 8);

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

#[tokio::test]
async fn visitor_reads_safe_rich_live_content_and_only_validated_links() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert("topstories".to_owned(), vec![FakeReply::json("[1]")]);
    routes.insert(
        "item/1".to_owned(),
        vec![FakeReply::json(
            r##"{"id":1,"type":"story","title":"Safe &amp; <em>sound</em>\u001b[31m\u202e","url":"https://example.com/story?x=1&y=2","score":42,"by":"author","time":1700000000,"descendants":7,"text":"<p>Hello <em>world</em> &amp; <strong>friends</strong>.<br>Next line with <code>let x = 1;</code> and <a href=\"https://docs.example/abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ\">docs</a>.</p><pre>fn main() {\n\tprintln!(\"safe\");\n}</pre><p><marquee>Unknown kept</marquee><script>HIDDEN_SCRIPT</script><style>HIDDEN_STYLE</style><a href=\"javascript:alert(1)\">unsafe js</a><a href=\"https://visitor:secret@example.net/private\">unsafe credentials</a></p>"}"##,
        )],
    );
    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint(fake.endpoint()));
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Safe & sound").await?;
    let feed = terminal.screen().contents();
    assert!(!feed.contains("<em>"));
    assert!(!feed.contains("[31m"));

    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Unknown kept").await?;
    let detail = terminal.screen().contents();
    for expected in [
        "Hello world & friends.",
        "Next line with let x = 1; and docs.",
        "fn main() {",
        "println!(\"safe\");",
        "Unknown kept",
        "Published: Tue, 14 Nov 2023 22:13:20 UTC",
    ] {
        assert!(
            detail.contains(expected),
            "detail did not contain {expected:?}"
        );
    }
    assert!(!detail.contains("HIDDEN_SCRIPT"));
    assert!(!detail.contains("HIDDEN_STYLE"));
    assert!(detail.contains("Unsafe or malformed links omitted: 2"));

    let long_destination = "https://docs.example/abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let osc_start = format!("\x1b]8;;{long_destination}\x1b\\");
    assert!(
        raw_output
            .windows(osc_start.len())
            .any(|window| window == osc_start.as_bytes()),
        "inline story link was not emitted as an OSC 8 hyperlink"
    );
    assert!(!detail.contains("javascript:"));
    assert!(!detail.contains("visitor:secret"));

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

#[tokio::test]
async fn visitor_can_reach_the_end_of_a_long_detail_view() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert("topstories".to_owned(), vec![FakeReply::json("[1]")]);
    let text = (1..=100)
        .map(|line| format!("<p>Detail line {line}</p>"))
        .collect::<String>();
    routes.insert(
        "item/1".to_owned(),
        vec![FakeReply::json(format!(
            r#"{{"id":1,"type":"story","title":"Long detail","text":"{text}"}}"#
        ))],
    );
    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint(fake.endpoint()));
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Long detail").await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Detail line 1",
    )
    .await?;
    channel.data_bytes(&b"G"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Detail line 100",
    )
    .await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

#[tokio::test]
async fn visitor_reads_live_poll_options_with_partial_failures() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert("topstories".to_owned(), vec![FakeReply::json("[10,20]")]);
    routes.insert(
        "item/10".to_owned(),
        vec![FakeReply::json(
            r#"{"id":10,"type":"poll","title":"Choose safely","parts":[11,12,13]}"#,
        )],
    );
    routes.insert(
        "item/11".to_owned(),
        vec![FakeReply::json(
            r#"{"id":11,"type":"pollopt","text":"First &amp; <em>best</em>","score":12}"#,
        )],
    );
    routes.insert("item/12".to_owned(), vec![FakeReply::json("null")]);
    routes.insert(
        "item/13".to_owned(),
        vec![FakeReply::json(
            r#"{"id":13,"type":"pollopt","text":"<strong>Still usable</strong>","score":1}"#,
        )],
    );
    routes.insert(
        "item/20".to_owned(),
        vec![FakeReply::json(
            r#"{"id":20,"type":"job","title":"Maintenance role","url":"https://jobs.example/role","score":99,"by":"should-not-show","descendants":88,"time":18446744073709551615,"text":"Build reliable systems."}"#,
        )],
    );
    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint(fake.endpoint()));
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Choose safely",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Still usable").await?;
    let detail = terminal.screen().contents();
    assert!(detail.contains("First & best — 12 points"));
    assert!(detail.contains("[poll option unavailable] · not loaded"));
    assert!(detail.contains("Still usable — 1 point"));
    assert_eq!(fake.request_count("item/11"), 1);
    assert_eq!(fake.request_count("item/12"), 1);
    assert_eq!(fake.request_count("item/13"), 1);

    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Top feed").await?;
    channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 2. Maintenance role",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Job · jobs.example",
    )
    .await?;
    let job = terminal.screen().contents();
    assert!(!job.contains("Score:"));
    assert!(job.contains("should-not-show"));
    assert!(!job.contains("Comments:"));
    assert!(!job.contains("Published:"));

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

#[tokio::test]
async fn concurrent_sessions_share_live_feed_and_item_requests() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "topstories".to_owned(),
        vec![FakeReply::json("[1]").delayed(Duration::from_millis(100))],
    );
    routes.insert(
        "item/1".to_owned(),
        vec![
            FakeReply::json(r#"{"id":1,"type":"story","title":"One shared entry"}"#)
                .delayed(Duration::from_millis(100)),
        ],
    );
    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint(fake.endpoint()));
    let server = TestServer::start_with_content(content).await?;
    let first = open_terminal(&server).await?;
    let second = open_terminal(&server).await?;
    let (first_connection, mut first_channel, mut first_terminal, mut first_output) = first;
    let (second_connection, mut second_channel, mut second_terminal, mut second_output) = second;

    let (first_loaded, second_loaded) = tokio::join!(
        read_until_visible(
            &mut first_channel,
            &mut first_terminal,
            &mut first_output,
            "One shared entry",
        ),
        read_until_visible(
            &mut second_channel,
            &mut second_terminal,
            &mut second_output,
            "One shared entry",
        )
    );
    first_loaded?;
    second_loaded?;
    assert_eq!(fake.request_count("topstories"), 1);
    assert_eq!(fake.request_count("item/1"), 1);

    first_channel.data_bytes(&b"q"[..]).await?;
    second_channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(
        read_through_close(&mut first_channel, &mut first_output).await?,
        0
    );
    assert_eq!(
        read_through_close(&mut second_channel, &mut second_output).await?,
        0
    );
    first_connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    second_connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

#[tokio::test]
async fn unavailable_live_feed_can_be_retried_without_restarting_the_process() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "topstories".to_owned(),
        vec![
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE),
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE),
            FakeReply::json("[1]"),
        ],
    );
    routes.insert(
        "item/1".to_owned(),
        vec![FakeReply::json(
            r#"{"id":1,"type":"story","title":"Recovered live feed"}"#,
        )],
    );
    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint(fake.endpoint()));
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Top feed unavailable",
    )
    .await?;
    assert!(terminal.screen().contents().contains("Press r to retry"));
    channel.data_bytes(&b"r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Recovered live feed",
    )
    .await?;
    assert_eq!(fake.request_count("topstories"), 3);

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

fn fast_hacker_news_options() -> HackerNewsOptions {
    HackerNewsOptions {
        request_timeout: Duration::from_millis(100),
        retry_base_delay: Duration::from_millis(5),
        maximum_retry_delay: Duration::from_millis(30),
        top_feed_fresh_for: Duration::from_secs(60),
        top_feed_stale_for: Duration::from_secs(15 * 60),
        item_fresh_for: Duration::from_secs(2 * 60),
        comment_fresh_for: Duration::from_secs(5 * 60),
        poll_option_fresh_for: Duration::from_secs(5 * 60),
        item_stale_for: Duration::from_secs(30 * 60),
        revalidation_interval: Duration::from_millis(80),
        maximum_cache_entries: 20_000,
        maximum_cache_weight: 64 * 1024 * 1024,
        upstream_concurrency: 16,
    }
}

#[tokio::test]
async fn visitor_loads_top_feed_metadata_in_explicit_thirty_entry_pages() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "topstories".to_owned(),
        vec![FakeReply::json(format!(
            "[{}]",
            (1..=65)
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))],
    );
    for id in 1..=65 {
        routes.insert(
            format!("item/{id}"),
            vec![FakeReply::json(format!(
                r#"{{"id":{id},"type":"story","title":"Paged entry {id}"}}"#
            ))],
        );
    }

    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint(fake.endpoint()));
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Paged entry 1",
    )
    .await?;
    assert_eq!(fake.request_count("item/31"), 0);

    channel.data_bytes(&b"G"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> Load 30 more entries",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 31. Paged entry 31",
    )
    .await?;
    assert_eq!(fake.request_count("item/31"), 1);
    assert_eq!(fake.request_count("item/61"), 0);

    channel.data_bytes(&b"G"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> Load 5 more entries",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "> 61. Paged entry 61",
    )
    .await?;
    assert_eq!(fake.request_count("item/65"), 1);

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

#[tokio::test]
async fn refresh_is_coalesced_throttled_and_keeps_failed_content_visibly_stale() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "topstories".to_owned(),
        vec![
            FakeReply::json("[1,2]"),
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE).delayed(Duration::from_millis(20)),
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE).delayed(Duration::from_millis(20)),
            FakeReply::json("[2,3]"),
        ],
    );
    routes.insert(
        "item/1".to_owned(),
        vec![FakeReply::json(
            r#"{"id":1,"type":"story","title":"Still usable entry"}"#,
        )],
    );
    routes.insert(
        "item/2".to_owned(),
        vec![
            FakeReply::json(r#"{"id":2,"type":"story","title":"Selected entry"}"#),
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE),
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE),
            FakeReply::json(r#"{"id":2,"type":"story","title":"Selected entry refreshed"}"#),
        ],
    );
    routes.insert(
        "item/3".to_owned(),
        vec![FakeReply::json(
            r#"{"id":3,"type":"story","title":"Recovered ranking"}"#,
        )],
    );

    let fake = FakeHackerNews::start(routes).await?;
    // A wide revalidation window makes the throttle assertion below depend on
    // policy rather than on scheduling latency between test steps.
    let mut options = fast_hacker_news_options();
    options.revalidation_interval = Duration::from_secs(1);
    let content = Arc::new(HackerNewsContent::for_endpoint_with_options(
        fake.endpoint(),
        options,
    ));
    let server = TestServer::start_with_content(content).await?;
    let first = open_terminal(&server).await?;
    let second = open_terminal(&server).await?;
    let (first_connection, mut first_channel, mut first_terminal, mut first_output) = first;
    let (second_connection, mut second_channel, mut second_terminal, mut second_output) = second;

    let (first_ready, second_ready) = tokio::join!(
        read_until_visible(
            &mut first_channel,
            &mut first_terminal,
            &mut first_output,
            "Still usable entry",
        ),
        read_until_visible(
            &mut second_channel,
            &mut second_terminal,
            &mut second_output,
            "Still usable entry",
        )
    );
    first_ready?;
    second_ready?;
    first_channel.data_bytes(&b"j"[..]).await?;
    second_channel.data_bytes(&b"j"[..]).await?;
    let (first_selected, second_selected) = tokio::join!(
        read_until_visible(
            &mut first_channel,
            &mut first_terminal,
            &mut first_output,
            "> 2. Selected entry",
        ),
        read_until_visible(
            &mut second_channel,
            &mut second_terminal,
            &mut second_output,
            "> 2. Selected entry",
        )
    );
    first_selected?;
    second_selected?;

    first_channel.data_bytes(&b"r"[..]).await?;
    second_channel.data_bytes(&b"r"[..]).await?;
    let (first_refreshing, second_refreshing) = tokio::join!(
        read_until_visible(
            &mut first_channel,
            &mut first_terminal,
            &mut first_output,
            "refreshing",
        ),
        read_until_visible(
            &mut second_channel,
            &mut second_terminal,
            &mut second_output,
            "refreshing",
        )
    );
    first_refreshing?;
    second_refreshing?;
    let (first_stale, second_stale) = tokio::join!(
        read_until_visible(
            &mut first_channel,
            &mut first_terminal,
            &mut first_output,
            "stale",
        ),
        read_until_visible(
            &mut second_channel,
            &mut second_terminal,
            &mut second_output,
            "stale",
        )
    );
    first_stale?;
    second_stale?;
    assert!(
        first_terminal.screen().contents().contains("refreshing"),
        "refresh progress should remain visible while entry metadata revalidates"
    );
    assert!(
        first_terminal
            .screen()
            .contents()
            .contains("> 2. Selected entry")
    );
    assert!(
        second_terminal
            .screen()
            .contents()
            .contains("> 2. Selected entry")
    );
    read_until_row_metadata(
        &mut first_channel,
        &mut first_terminal,
        &mut first_output,
        "> 2. Selected entry",
        "stale",
    )
    .await?;
    assert!(row_metadata_contains(
        &first_terminal.screen().contents(),
        "> 2. Selected entry",
        "old"
    ));
    assert_eq!(fake.request_count("topstories"), 3);

    first_channel.data_bytes(&b"r"[..]).await?;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        fake.request_count("topstories"),
        3,
        "revalidation is throttled for the same resource"
    );

    sleep(Duration::from_millis(1_500)).await;
    first_channel.data_bytes(&b"r"[..]).await?;
    read_until_visible(
        &mut first_channel,
        &mut first_terminal,
        &mut first_output,
        "Selected entry refreshed",
    )
    .await?;
    assert!(
        first_terminal
            .screen()
            .contents()
            .contains("> 1. Selected entry refreshed"),
        "refresh should keep focus on the selected feed entry as its rank changes"
    );
    assert_eq!(fake.request_count("topstories"), 4);

    first_channel.data_bytes(&b"q"[..]).await?;
    second_channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(
        read_through_close(&mut first_channel, &mut first_output).await?,
        0
    );
    assert_eq!(
        read_through_close(&mut second_channel, &mut second_output).await?,
        0
    );
    first_connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    second_connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

#[tokio::test]
async fn upstream_retry_classification_timeout_and_retry_after_are_bounded() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "item/1".to_owned(),
        vec![
            FakeReply::status(StatusCode::INTERNAL_SERVER_ERROR),
            FakeReply::json(r#"{"id":1,"type":"story","title":"Retried 5xx"}"#),
        ],
    );
    routes.insert(
        "item/2".to_owned(),
        vec![
            FakeReply::status(StatusCode::TOO_MANY_REQUESTS).retry_after("999"),
            FakeReply::json(r#"{"id":2,"type":"story","title":"Retried 429"}"#),
        ],
    );
    routes.insert(
        "item/3".to_owned(),
        vec![
            FakeReply::status(StatusCode::NOT_FOUND),
            FakeReply::json(r#"{"id":3,"type":"story","title":"Not blindly retried"}"#),
        ],
    );
    routes.insert("item/4".to_owned(), vec![FakeReply::json("null")]);
    routes.insert("item/5".to_owned(), vec![FakeReply::json("{malformed")]);
    routes.insert(
        "item/6".to_owned(),
        vec![FakeReply::json("null").delayed(Duration::from_millis(100))],
    );
    routes.insert(
        "item/7".to_owned(),
        vec![
            FakeReply::status(StatusCode::NOT_FOUND),
            FakeReply::status(StatusCode::NOT_FOUND),
            FakeReply::json(r#"{"id":7,"type":"story","title":"Throttled recovery"}"#),
        ],
    );

    let fake = FakeHackerNews::start(routes).await?;
    let mut options = fast_hacker_news_options();
    options.request_timeout = Duration::from_millis(20);
    let content = HackerNewsContent::for_endpoint_with_options(fake.endpoint(), options);

    assert!(content.feed_entry(1).await.is_ok());
    assert!(content.feed_entry(2).await.is_ok());
    assert_eq!(content.feed_entry(3).await, Err(ContentError::Unavailable));
    assert!(matches!(
        content.feed_entry(4).await?.value,
        FeedEntry::Unavailable { id: 4 }
    ));
    assert_eq!(content.feed_entry(5).await, Err(ContentError::Unavailable));
    assert_eq!(content.feed_entry(6).await, Err(ContentError::Retryable));
    assert_eq!(content.feed_entry(7).await, Err(ContentError::Unavailable));
    assert_eq!(
        content.refresh_feed_entry(7).await,
        Err(ContentError::Unavailable)
    );
    assert_eq!(
        content.refresh_feed_entry(7).await,
        Err(ContentError::Unavailable)
    );
    assert_eq!(fake.request_count("item/7"), 2);
    sleep(Duration::from_millis(90)).await;
    assert_eq!(
        content.refresh_feed_entry(7).await?.value.title(),
        "Throttled recovery"
    );

    assert_eq!(fake.request_count("item/1"), 2);
    assert_eq!(fake.request_count("item/2"), 2);
    assert_eq!(fake.request_count("item/3"), 1);
    assert_eq!(fake.request_count("item/4"), 1);
    assert_eq!(fake.request_count("item/5"), 1);
    assert_eq!(fake.request_count("item/6"), 2);
    assert_eq!(fake.request_count("item/7"), 3);
    let retry_times = fake.request_times("item/2");
    assert!(retry_times[1].duration_since(retry_times[0]) >= Duration::from_millis(20));
    assert!(retry_times[1].duration_since(retry_times[0]) < Duration::from_millis(200));

    fake.stop().await
}

#[tokio::test]
async fn cache_uses_five_minute_freshness_class_for_comments_and_poll_options() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "item/10".to_owned(),
        vec![FakeReply::json(
            r#"{"id":10,"type":"story","title":"Story metadata"}"#,
        )],
    );
    routes.insert(
        "item/11".to_owned(),
        vec![FakeReply::json(r#"{"id":11,"type":"comment"}"#)],
    );
    routes.insert(
        "item/12".to_owned(),
        vec![FakeReply::json(r#"{"id":12,"type":"pollopt"}"#)],
    );
    let fake = FakeHackerNews::start(routes).await?;
    let mut options = fast_hacker_news_options();
    options.item_fresh_for = Duration::from_millis(20);
    options.comment_fresh_for = Duration::from_millis(100);
    options.poll_option_fresh_for = Duration::from_millis(100);
    let content = HackerNewsContent::for_endpoint_with_options(fake.endpoint(), options);

    content.feed_entry(10).await?;
    content.comment(11).await?;
    content.feed_entry(12).await?;
    sleep(Duration::from_millis(30)).await;
    assert!(content.feed_entry(10).await?.freshness.is_stale());
    assert!(content.comment(11).await?.freshness.is_fresh());
    assert!(content.feed_entry(12).await?.freshness.is_fresh());
    sleep(Duration::from_millis(80)).await;
    assert!(content.comment(11).await?.freshness.is_stale());
    assert!(content.feed_entry(12).await?.freshness.is_stale());
    assert_eq!(fake.request_count("item/10"), 1);
    assert_eq!(fake.request_count("item/11"), 1);
    assert_eq!(fake.request_count("item/12"), 1);

    fake.stop().await
}

#[tokio::test]
async fn upstream_requests_share_one_global_sixteen_request_limit() -> TestResult {
    let mut routes = HashMap::new();
    for id in 1..=40 {
        routes.insert(
            format!("item/{id}"),
            vec![
                FakeReply::json(format!(
                    r#"{{"id":{id},"type":"story","title":"Concurrent {id}"}}"#
                ))
                .delayed(Duration::from_millis(30)),
            ],
        );
    }
    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint_with_options(
        fake.endpoint(),
        fast_hacker_news_options(),
    ));
    let mut requests = tokio::task::JoinSet::new();
    for id in 1..=40 {
        let content = Arc::clone(&content);
        requests.spawn(async move { content.feed_entry(id).await });
    }
    while let Some(result) = requests.join_next().await {
        result??;
    }

    assert!(fake.maximum_active_items() <= 16);
    assert!(fake.maximum_active_items() > 8);
    fake.stop().await
}

#[tokio::test]
async fn cache_observes_fresh_stale_and_lru_resource_bounds() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "topstories".to_owned(),
        vec![
            FakeReply::json("[1]"),
            FakeReply::status(StatusCode::NOT_FOUND),
            FakeReply::json("[2]"),
        ],
    );
    for id in 1..=3 {
        routes.insert(
            format!("item/{id}"),
            vec![FakeReply::json(format!(
                r#"{{"id":{id},"type":"story","title":"Cache entry {id}"}}"#
            ))],
        );
    }
    let fake = FakeHackerNews::start(routes).await?;
    let mut options = fast_hacker_news_options();
    options.top_feed_fresh_for = Duration::from_millis(20);
    options.top_feed_stale_for = Duration::from_millis(90);
    options.revalidation_interval = Duration::ZERO;
    options.maximum_cache_entries = 2;
    let content = HackerNewsContent::for_endpoint_with_options(fake.endpoint(), options);

    assert!(content.top_feed().await?.freshness.is_fresh());
    assert!(content.top_feed().await?.freshness.is_fresh());
    assert_eq!(fake.request_count("topstories"), 1);
    sleep(Duration::from_millis(30)).await;
    assert!(content.top_feed().await?.freshness.is_stale());
    assert!(content.refresh_top_feed().await?.freshness.is_stale());
    assert_eq!(fake.request_count("topstories"), 2);
    sleep(Duration::from_millis(80)).await;
    assert_eq!(content.top_feed().await?.value, vec![2]);
    assert_eq!(fake.request_count("topstories"), 3);

    content.feed_entry(1).await?;
    content.feed_entry(2).await?;
    content.feed_entry(1).await?;
    content.feed_entry(3).await?;
    content.feed_entry(2).await?;
    assert_eq!(
        fake.request_count("item/2"),
        2,
        "the least-recently-used inactive record is evicted at the entry cap"
    );

    fake.stop().await
}

#[tokio::test]
async fn item_stale_fallback_stops_at_the_configured_limit() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert(
        "item/1".to_owned(),
        vec![
            FakeReply::json(r#"{"id":1,"type":"story","title":"Bounded stale item"}"#),
            FakeReply::status(StatusCode::NOT_FOUND),
            FakeReply::status(StatusCode::NOT_FOUND),
        ],
    );
    let fake = FakeHackerNews::start(routes).await?;
    let mut options = fast_hacker_news_options();
    options.item_fresh_for = Duration::from_millis(20);
    options.item_stale_for = Duration::from_millis(80);
    options.revalidation_interval = Duration::ZERO;
    let content = HackerNewsContent::for_endpoint_with_options(fake.endpoint(), options);

    content.feed_entry(1).await?;
    sleep(Duration::from_millis(30)).await;
    assert!(content.feed_entry(1).await?.freshness.is_stale());
    assert!(content.refresh_feed_entry(1).await?.freshness.is_stale());
    sleep(Duration::from_millis(60)).await;
    assert_eq!(
        content.refresh_feed_entry(1).await,
        Err(ContentError::Unavailable),
        "expired item content must not survive a failed refresh"
    );
    assert_eq!(fake.request_count("item/1"), 3);

    fake.stop().await
}

#[tokio::test]
async fn cache_weight_cap_evicts_large_inactive_records() -> TestResult {
    let large_title = "x".repeat(500);
    let mut routes = HashMap::new();
    for id in 1..=2 {
        routes.insert(
            format!("item/{id}"),
            vec![FakeReply::json(format!(
                r#"{{"id":{id},"type":"story","title":"{large_title}"}}"#
            ))],
        );
    }
    let fake = FakeHackerNews::start(routes).await?;
    let mut options = fast_hacker_news_options();
    options.maximum_cache_entries = 10;
    options.maximum_cache_weight = 700;
    let content = HackerNewsContent::for_endpoint_with_options(fake.endpoint(), options);

    content.feed_entry(1).await?;
    content.feed_entry(2).await?;
    content.feed_entry(1).await?;
    assert_eq!(
        fake.request_count("item/1"),
        2,
        "the content-weight cap evicts an inactive large record"
    );

    fake.stop().await
}

#[tokio::test]
async fn ssh_disconnect_cancels_in_flight_session_comment_loads() -> TestResult {
    let started = Arc::new(AtomicUsize::new(0));
    let cancelled = Arc::new(AtomicUsize::new(0));
    let content = Arc::new(PendingCommentContent {
        started: Arc::clone(&started),
        cancelled: Arc::clone(&cancelled),
    });
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Cancellation story",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    timeout(TEST_TIMEOUT, async {
        while started.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;

    connection
        .disconnect(
            russh::Disconnect::ByApplication,
            "simulate transport loss",
            "en",
        )
        .await?;
    drop(channel);
    drop(connection);
    timeout(TEST_TIMEOUT, async {
        while cancelled.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);

    server.stop().await
}

#[tokio::test]
async fn visitor_sees_depth_and_node_limit_markers_without_fetching_past_them() -> TestResult {
    let content = Arc::new(LimitFixtureContent {
        requested_comments: Arc::new(Mutex::new(Vec::new())),
    });
    let server = TestServer::start_with_content(content.clone()).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Depth limit story",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "deep-author-10000",
    )
    .await?;
    for id in 10_001..=10_063 {
        channel.data_bytes(&b"N"[..]).await?;
        read_until_visible(
            &mut channel,
            &mut terminal,
            &mut raw_output,
            &format!("deep-author-{id}"),
        )
        .await?;
    }
    channel.data_bytes(&b"\x1b[6~"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "[thread depth limit of 64 reached]",
    )
    .await?;
    assert_eq!(content.request_count(10_064), 0);

    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Top feed").await?;
    channel.data_bytes(&b"j\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "wide-author-20000",
    )
    .await?;
    let visit_wide_siblings = vec![b'n'; 999];
    channel.data_bytes(visit_wide_siblings).await?;
    // Wait for the selected comment result before explicit paging so its
    // identity-reveal redraw cannot legitimately bring the header back over
    // the marker after Page Down.
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "wide-author-20999",
    )
    .await?;
    channel.data_bytes(&b"\x1b[6~"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "[thread node limit of 1000 reached]",
    )
    .await?;
    assert_eq!(content.request_count(20_999), 1);
    assert_eq!(content.request_count(21_000), 0);

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}

#[tokio::test]
async fn visitor_browses_a_lazy_bounded_comment_thread_with_local_failures() -> TestResult {
    let mut routes = HashMap::new();
    routes.insert("topstories".to_owned(), vec![FakeReply::json("[1]")]);
    routes.insert(
        "item/1".to_owned(),
        vec![FakeReply::json(format!(
            r#"{{"id":1,"type":"story","title":"Threaded story","url":"https://example.com/threaded","descendants":24,"kids":[{}]}}"#,
            (10..=31)
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))],
    );
    routes.insert(
        "item/10".to_owned(),
        vec![FakeReply::json(
            r#"{"id":10,"type":"comment","parent":1,"by":"root-author","time":1700000000,"text":"Root &amp; safe","kids":[100,101,10,100]}"#,
        )],
    );
    routes.insert(
        "item/11".to_owned(),
        vec![FakeReply::json(
            r#"{"id":11,"type":"comment","parent":1,"deleted":true}"#,
        )],
    );
    routes.insert(
        "item/12".to_owned(),
        vec![FakeReply::json(
            r#"{"id":12,"type":"comment","parent":1,"dead":true}"#,
        )],
    );
    routes.insert("item/13".to_owned(), vec![FakeReply::json("null")]);
    routes.insert(
        "item/14".to_owned(),
        vec![
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE),
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE),
            FakeReply::json(
                r#"{"id":14,"type":"comment","parent":1,"by":"recovered","text":"Recovered comment"}"#,
            ),
        ],
    );
    routes.insert(
        "item/15".to_owned(),
        vec![FakeReply::json(
            r#"{"id":15,"type":"story","title":"Wrong child type"}"#,
        )],
    );
    routes.insert(
        "item/16".to_owned(),
        vec![
            FakeReply::json(
                r#"{"id":16,"type":"comment","parent":1,"by":"stale-author","text":"Stale comment remains readable"}"#,
            ),
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE),
            FakeReply::status(StatusCode::SERVICE_UNAVAILABLE),
        ],
    );
    for id in 17..=31 {
        let kids = if id == 29 { r#","kids":[290]"# } else { "" };
        routes.insert(
            format!("item/{id}"),
            vec![FakeReply::json(format!(
                r#"{{"id":{id},"type":"comment","parent":1,"by":"author-{id}","text":"Comment {id}"{kids}}}"#
            ))],
        );
    }
    routes.insert(
        "item/290".to_owned(),
        vec![FakeReply::json(
            r#"{"id":290,"type":"comment","parent":29,"by":"near-viewport-child","text":"Loaded only near its parent"}"#,
        )],
    );
    routes.insert(
        "item/100".to_owned(),
        vec![FakeReply::json(
            r#"{"id":100,"type":"comment","parent":10,"by":"nested-a","text":"Nested <a href=\"https://comment.example/first\">first</a>"}"#,
        )],
    );
    routes.insert(
        "item/101".to_owned(),
        vec![FakeReply::json(
            r#"{"id":101,"type":"comment","parent":10,"by":"nested-b","text":"Nested second"}"#,
        )],
    );

    let fake = FakeHackerNews::start(routes).await?;
    let mut options = fast_hacker_news_options();
    options.comment_fresh_for = Duration::from_millis(10);
    options.revalidation_interval = Duration::ZERO;
    let content = Arc::new(HackerNewsContent::for_endpoint_with_options(
        fake.endpoint(),
        options,
    ));
    content.comment(16).await?;
    sleep(Duration::from_millis(20)).await;
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Threaded story",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Enter load/retry",
    )
    .await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Root & safe").await?;

    channel.data_bytes(&b"C"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "[comment temporarily unavailable]",
    )
    .await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "stale").await?;
    timeout(TEST_TIMEOUT, async {
        while fake.request_count("item/16") < 3 {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert_eq!(fake.request_count("item/16"), 3);
    assert_eq!(fake.request_count("item/29"), 1);
    assert_eq!(fake.request_count("item/30"), 0);
    assert_eq!(fake.request_count("item/290"), 0);
    assert!(fake.maximum_active_items() <= 8);
    let collapsed = terminal.screen().contents();
    assert!(collapsed.contains("[deleted comment]"));
    assert!(collapsed.contains("[dead comment]"));
    assert!(collapsed.contains("[comment missing]"));
    assert!(collapsed.contains("[unsupported story]"));
    assert!(collapsed.contains("stale-author"));
    assert!(collapsed.contains("stale"));
    assert!(collapsed.contains("root-author"));
    assert!(!collapsed.contains("Root & safe"));

    sleep(Duration::from_millis(90)).await;
    channel.data_bytes(&b"nnnn\r"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "recovered").await?;
    assert_eq!(fake.request_count("item/14"), 3);
    let recovered = terminal.screen().contents();
    let recovered_header = recovered
        .lines()
        .find(|line| line.contains(">>") && line.contains("recovered"))
        .expect("recovered comment is selected");
    assert!(recovered_header.contains("next [n]"));
    assert!(recovered_header.contains("prev [p]"));

    channel.data_bytes(&b"ppppEN"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "selected nested-a",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("nested-a"))
        },
    )
    .await?;
    let first_child = terminal.screen().contents();
    let first_child_header = first_child
        .lines()
        .find(|line| line.contains(">>") && line.contains("nested-a"))
        .expect("first child is selected");
    assert!(first_child_header.contains("parent [P]"));
    assert!(first_child_header.contains("next [n]"));

    let osc_start = b"\x1b]8;;https://comment.example/first\x1b\\";
    assert!(
        raw_output
            .windows(osc_start.len())
            .any(|window| window == osc_start),
        "inline comment link was not emitted as an OSC 8 hyperlink"
    );
    channel.data_bytes(&b"n"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "selected nested-b",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("nested-b"))
        },
    )
    .await?;
    channel.data_bytes(&b"pP"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "selected root comment",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("root-author"))
        },
    )
    .await?;
    let expanded = terminal.screen().contents();
    assert!(expanded.contains("├─"));
    assert!(expanded.contains("│"));
    assert!(expanded.contains("[cycle to comment 10 omitted]"));
    assert!(expanded.contains("[duplicate comment 100 omitted]"));
    assert_eq!(fake.request_count("item/100"), 1);
    assert_eq!(fake.request_count("item/101"), 1);

    channel.data_bytes(&b" "[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "collapsed selected root",
        |screen| {
            !screen.contains("Root & safe")
                && screen
                    .lines()
                    .any(|line| line.contains(">>") && line.contains("root-author"))
        },
    )
    .await?;
    let locally_collapsed = terminal.screen().contents();
    assert!(!locally_collapsed.contains("Root & safe"));
    assert!(!locally_collapsed.contains("Nested first"));
    let root_header = locally_collapsed
        .lines()
        .find(|line| line.contains(">>") && line.contains("root-author"))
        .expect("collapsed root remains selected");
    assert!(root_header.contains("2 comments collapsed"));
    assert!(!root_header.contains("years ago"));
    channel.data_bytes(&b" "[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Nested first").await?;
    assert_eq!(fake.request_count("item/100"), 1);
    channel.data_bytes(&b"h"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "root collapsed with h",
        |screen| !screen.contains("Nested first") && screen.contains("root-author"),
    )
    .await?;
    channel.data_bytes(&b"l"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Nested first").await?;
    assert_eq!(fake.request_count("item/100"), 1);

    channel.data_bytes(&b"nnnnnnnnnnnnnnnnnnnn"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "author-30").await?;
    assert_eq!(fake.request_count("item/30"), 1);
    assert_eq!(fake.request_count("item/31"), 1);
    assert_eq!(fake.request_count("item/290"), 1);

    let destination_output_start = raw_output.len();
    channel.data_bytes(&b"o"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "https://example.com/threaded",
    )
    .await?;
    assert!(terminal.screen().contents().contains("Primary destination"));
    let destination_output = &raw_output[destination_output_start..];
    for expected in [
        b"\x1b]8;;https://example.com/threaded\x1b\\".as_slice(),
        b"https://example.com/threaded\x1b]8;;\x1b\\".as_slice(),
    ] {
        assert!(
            destination_output
                .windows(expected.len())
                .any(|window| window == expected),
            "destination output did not contain OSC 8 sequence {expected:?}"
        );
    }
    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "author-30").await?;

    channel.data_bytes(&b"?"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Keyboard help",
    )
    .await?;
    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "author-30").await?;

    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Top feed").await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "author-30").await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

#[tokio::test]
async fn long_comment_paging_async_insertion_resize_and_reopen_keep_identity() -> TestResult {
    let long_text = format!(
        "<p>LONG-START selected comment</p>{}<p>LONG-END selected comment</p>",
        (1..=45)
            .map(|line| format!(
                "<p>Wrapped comment line {line} has enough prose to exercise physical terminal line ranges.</p>"
            ))
            .collect::<String>()
    );
    let mut routes = HashMap::new();
    routes.insert("topstories".to_owned(), vec![FakeReply::json("[1]")]);
    routes.insert(
        "item/1".to_owned(),
        vec![FakeReply::json(
            r#"{"id":1,"type":"story","title":"Viewport story","text":"ENTRY-TOP-CONTEXT","kids":[10,11],"descendants":3}"#,
        )],
    );
    routes.insert(
        "item/10".to_owned(),
        vec![FakeReply::json(format!(
            r#"{{"id":10,"type":"comment","parent":1,"by":"long-author","text":"{long_text}","kids":[100]}}"#
        ))
        .delayed(Duration::from_millis(80))],
    );
    routes.insert(
        "item/11".to_owned(),
        vec![FakeReply::json(
            r#"{"id":11,"type":"comment","parent":1,"by":"second-author","text":"Second root remains selected"}"#,
        )],
    );
    routes.insert(
        "item/100".to_owned(),
        vec![FakeReply::json(
            r#"{"id":100,"type":"comment","parent":10,"by":"inserted-child","text":"Arrived asynchronously"}"#,
        )],
    );

    let fake = FakeHackerNews::start(routes).await?;
    let content = Arc::new(HackerNewsContent::for_endpoint_with_options(
        fake.endpoint(),
        fast_hacker_news_options(),
    ));
    let server = TestServer::start_with_content(content).await?;
    let (connection, mut channel, mut terminal, mut raw_output) = open_terminal(&server).await?;

    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Viewport story",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "second-author",
    )
    .await?;
    channel.data_bytes(&b"n"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "selected second root",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("second-author"))
        },
    )
    .await?;

    sleep(Duration::from_millis(150)).await;
    channel.data_bytes(&b"?"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Keyboard help",
    )
    .await?;
    assert_eq!(fake.request_count("item/100"), 1);
    channel.data_bytes(&[0x1b][..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "identity after asynchronous insertion",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("second-author"))
        },
    )
    .await?;

    channel.data_bytes(&b"p"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "selected first root",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("long-author"))
        },
    )
    .await?;
    channel.data_bytes(&b"k"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "ENTRY-TOP-CONTEXT",
    )
    .await?;
    channel.data_bytes(&b"n"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "selected second root after returning from Entry top",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("second-author"))
        },
    )
    .await?;

    terminal.screen_mut().set_size(18, 60);
    channel.window_change(60, 18, 0, 0).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "identity after narrow resize",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("second-author"))
        },
    )
    .await?;

    channel.data_bytes(&b"p"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "selected long root",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("long-author"))
        },
    )
    .await?;
    channel
        .data_bytes(
            &b"\x1b[6~\x1b[6~\x1b[6~\x1b[6~\x1b[6~\x1b[6~\x1b[6~\x1b[6~\x1b[6~\x1b[6~\x1b[6~\x1b[6~"[..],
        )
        .await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "LONG-END").await?;
    assert!(
        !terminal
            .screen()
            .contents()
            .lines()
            .any(|line| line.contains(">>") && line.contains("long-author")),
        "explicit paging may move the selected header off-screen"
    );

    channel.data_bytes(&b"n"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "selected second root after paging",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("second-author"))
        },
    )
    .await?;
    channel.data_bytes(&[0x1b][..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Top feed").await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "restored second root",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("second-author"))
        },
    )
    .await?;

    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut raw_output).await?, 0);
    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await?;
    fake.stop().await
}

async fn expect_failure(channel: &mut Channel<client::Msg>) -> TestResult {
    timeout(TEST_TIMEOUT, async {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Failure) => return Ok(()),
                Some(ChannelMsg::Data { .. }) | Some(ChannelMsg::WindowAdjusted { .. }) => {}
                message => return Err(format!("expected request failure, got {message:?}").into()),
            }
        }
    })
    .await?
}

/// A request sent after the server intentionally tears down on channel EOF may
/// observe a `Failure` frame or lose the channel/connection before any reply is
/// delivered. Either proves the request was not accepted, so only `Success` is
/// a failure; teardown no longer has to wait for a rejection round trip.
async fn expect_failure_or_teardown(channel: &mut Channel<client::Msg>) -> TestResult {
    timeout(TEST_TIMEOUT, async {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Failure) => return Ok(()),
                Some(ChannelMsg::Success) => {
                    return Err("request after EOF was unexpectedly accepted".into());
                }
                Some(ChannelMsg::Data { .. }) | Some(ChannelMsg::WindowAdjusted { .. }) => {}
                Some(_) | None => return Ok(()),
            }
        }
    })
    .await?
}

fn assert_channel_denied(result: Result<Channel<client::Msg>, russh::Error>) {
    assert!(
        matches!(
            result,
            Err(russh::Error::ChannelOpenFailure(
                russh::ChannelOpenFailure::AdministrativelyProhibited
            ))
        ),
        "expected administrative channel rejection, got {result:?}"
    );
}

async fn reject_unsupported_requests(channel: &mut Channel<client::Msg>) -> TestResult {
    channel
        .exec(true, "touch /tmp/hnssh-must-not-execute")
        .await?;
    expect_failure(channel).await?;
    channel.request_subsystem(true, "sftp").await?;
    expect_failure(channel).await?;
    channel.set_env(true, "PATH", "/tmp").await?;
    expect_failure(channel).await?;
    channel
        .request_x11(true, false, "MIT-MAGIC-COOKIE-1", "0123456789abcdef", 0)
        .await?;
    expect_failure(channel).await?;
    channel.agent_forward(true).await?;
    expect_failure(channel).await?;
    // No-reply requests must not leave stray success/failure replies that could
    // be mistaken for the next (valid) request's acknowledgement.
    channel.exec(false, "sh").await?;
    channel.request_subsystem(false, "sftp").await?;
    channel.set_env(false, "TERM", "dumb").await?;
    channel
        .request_x11(false, false, "MIT-MAGIC-COOKIE-1", "bad-cookie", 0)
        .await?;
    channel.agent_forward(false).await?;
    Ok(())
}

#[tokio::test]
async fn duplicate_and_unsupported_requests_preserve_the_browsing_session() -> TestResult {
    let server = TestServer::start().await?;
    let connection = server.connect().await?;
    let mut channel = connection.channel_open_session().await?;
    assert_channel_denied(timeout(TEST_TIMEOUT, connection.channel_open_session()).await?);
    reject_unsupported_requests(&mut channel).await?;

    channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_pty(true, "xterm", 1, 1, 0, 0, &[]).await?;
    expect_failure(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;
    let mut terminal = vt100::Parser::new(24, 80, 0);
    let mut output = Vec::new();
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;

    channel.request_shell(true).await?;
    expect_failure(&mut channel).await?;
    channel.request_pty(true, "xterm", 1, 1, 0, 0, &[]).await?;
    expect_failure(&mut channel).await?;
    reject_unsupported_requests(&mut channel).await?;
    assert_channel_denied(timeout(TEST_TIMEOUT, connection.channel_open_session()).await?);
    assert_channel_denied(
        timeout(
            TEST_TIMEOUT,
            connection.channel_open_direct_tcpip("127.0.0.1", 22, "127.0.0.1", 1234),
        )
        .await?,
    );
    assert_channel_denied(
        timeout(TEST_TIMEOUT, connection.channel_open_x11("127.0.0.1", 6000)).await?,
    );
    assert_channel_denied(
        timeout(
            TEST_TIMEOUT,
            connection.channel_open_direct_streamlocal("/tmp/hnssh-not-a-socket"),
        )
        .await?,
    );
    assert!(matches!(
        timeout(TEST_TIMEOUT, connection.tcpip_forward("127.0.0.1", 0)).await?,
        Err(russh::Error::RequestDenied)
    ));
    assert!(matches!(
        timeout(
            TEST_TIMEOUT,
            connection.cancel_tcpip_forward("127.0.0.1", 1234)
        )
        .await?,
        Err(russh::Error::RequestDenied)
    ));
    assert!(matches!(
        timeout(
            TEST_TIMEOUT,
            connection.streamlocal_forward("/tmp/hnssh-not-a-socket")
        )
        .await?,
        Err(russh::Error::RequestDenied)
    ));
    assert!(matches!(
        timeout(
            TEST_TIMEOUT,
            connection.cancel_streamlocal_forward("/tmp/hnssh-not-a-socket")
        )
        .await?,
        Err(russh::Error::RequestDenied)
    ));

    channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut output, "> 2. Ask HN:").await?;
    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    timeout(TEST_TIMEOUT, async {
        while !connection.is_closed() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert!(connection.channel_open_session().await.is_err());
    server.stop().await
}

#[tokio::test]
async fn forwarding_agent_forwarding_rejections_do_not_cross_request_scopes() -> TestResult {
    let server = TestServer::start().await?;
    let (connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;

    for agent_wants_reply in [true, false] {
        // Leave a replied-to global request before the channel request. Pipeline
        // the next global request so a stray reply cannot be silently discarded.
        assert!(matches!(
            timeout(TEST_TIMEOUT, connection.tcpip_forward("127.0.0.1", 0)).await?,
            Err(russh::Error::RequestDenied)
        ));
        channel.agent_forward(agent_wants_reply).await?;
        let result = timeout(TEST_TIMEOUT, connection.tcpip_forward("127.0.0.1", 0)).await?;
        assert!(
            matches!(result, Err(russh::Error::RequestDenied)),
            "forwarding after agent rejection: {result:?}"
        );
        if agent_wants_reply {
            expect_failure(&mut channel).await?;
        }
        channel.request_shell(true).await?;
        expect_failure(&mut channel).await?;
    }

    channel.data_bytes(&b"j"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut output, "> 2. Ask HN:").await?;
    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    server.stop().await
}

#[derive(Clone, Copy, Debug)]
enum NoReplyForwarding {
    None,
    Tcp,
    CancelTcp,
    Unix,
    CancelUnix,
}

struct OrderedForwardingReply {
    arrived_before_marker: bool,
    reply: oneshot::Receiver<Option<u32>>,
}

struct ForwardingOrderClient {
    expected_key: ssh_key::PublicKey,
    no_reply: NoReplyForwarding,
    agent_wants_reply: bool,
    later_reply: Option<oneshot::Receiver<Option<u32>>>,
    observed: Option<oneshot::Sender<OrderedForwardingReply>>,
}

impl client::Handler for ForwardingOrderClient {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &ssh_key::PublicKey) -> Result<bool, Self::Error> {
        Ok(key == &self.expected_key)
    }

    async fn channel_open_confirmation(
        &mut self,
        channel: russh::ChannelId,
        _max_packet_size: u32,
        _window_size: u32,
        session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Session's public API supports no-reply global requests; Handle's does
        // not. Queue one batch in wire order, without timers or server hooks.
        match self.no_reply {
            // Preserve the preceding replied-to global request's state, then
            // interleave agent -> marker -> global in one deterministic batch.
            NoReplyForwarding::None => {}
            NoReplyForwarding::Tcp => session.tcpip_forward(None, "127.0.0.1", 0)?,
            NoReplyForwarding::CancelTcp => {
                session.cancel_tcpip_forward(None, "127.0.0.1", 1234)?
            }
            NoReplyForwarding::Unix => {
                session.streamlocal_forward(None, "/tmp/hnssh-not-a-socket")?
            }
            NoReplyForwarding::CancelUnix => {
                session.cancel_streamlocal_forward(None, "/tmp/hnssh-not-a-socket")?
            }
        }
        session.agent_forward(channel, self.agent_wants_reply)?;
        // The PTY success is a wire-order marker before the next global reply.
        session.request_pty(channel, true, "xterm", 80, 24, 0, 0, &[])?;
        let (sender, receiver) = oneshot::channel();
        session.tcpip_forward(Some(sender), "127.0.0.1", 0)?;
        self.later_reply = Some(receiver);
        Ok(())
    }

    async fn channel_success(
        &mut self,
        _channel: russh::ChannelId,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if let Some(mut reply) = self.later_reply.take() {
            let arrived_before_marker =
                !matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            if let Some(observed) = self.observed.take() {
                let _ = observed.send(OrderedForwardingReply {
                    arrived_before_marker,
                    reply,
                });
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn no_reply_forwarding_and_agent_requests_do_not_emit_unsolicited_replies() -> TestResult {
    let server = TestServer::start().await?;
    for no_reply in [
        NoReplyForwarding::None,
        NoReplyForwarding::Tcp,
        NoReplyForwarding::CancelTcp,
        NoReplyForwarding::Unix,
        NoReplyForwarding::CancelUnix,
    ] {
        for agent_wants_reply in [false, true] {
            let (observed, observation) = oneshot::channel();
            let handler = ForwardingOrderClient {
                expected_key: server.public_key.clone(),
                no_reply,
                agent_wants_reply,
                later_reply: None,
                observed: Some(observed),
            };
            let mut connection = timeout(
                TEST_TIMEOUT,
                client::connect(Arc::new(client::Config::default()), server.address, handler),
            )
            .await??;
            assert!(connection.authenticate_none("visitor").await?.success());
            // Start with a replied-to global request, as in the original bug.
            assert!(matches!(
                timeout(TEST_TIMEOUT, connection.tcpip_forward("127.0.0.1", 0)).await?,
                Err(russh::Error::RequestDenied)
            ));
            let mut channel = timeout(TEST_TIMEOUT, connection.channel_open_session()).await??;
            if agent_wants_reply {
                expect_failure(&mut channel).await?;
            }
            // An unsolicited channel failure (including for agent want_reply=false)
            // cannot masquerade as the expected successful PTY acknowledgement.
            expect_success(&mut channel).await?;
            let observed = timeout(TEST_TIMEOUT, observation)
                .await
                .map_err(|_| "no PTY marker observation")??;
            assert!(
                !observed.arrived_before_marker,
                "unsolicited global reply for {no_reply:?}, agent want_reply={agent_wants_reply}"
            );
            assert_eq!(
                timeout(TEST_TIMEOUT, observed.reply)
                    .await
                    .map_err(|_| "no global forwarding reply after marker")??,
                None
            );

            channel.request_shell(true).await?;
            expect_success(&mut channel).await?;
            let mut terminal = vt100::Parser::new(24, 80, 0);
            let mut output = Vec::new();
            read_until_visible(
                &mut channel,
                &mut terminal,
                &mut output,
                "Rust reaches the tiny terminal",
            )
            .await?;
            channel.data_bytes(&b"j"[..]).await?;
            read_until_visible(&mut channel, &mut terminal, &mut output, "> 2. Ask HN:").await?;
            channel.data_bytes(&b"q"[..]).await?;
            assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
        }
    }
    server.stop().await
}

#[tokio::test]
async fn data_and_resize_before_pty_do_not_establish_or_mutate_a_shell() -> TestResult {
    let server = TestServer::start().await?;
    let connection = server.connect().await?;
    let mut channel = connection.channel_open_session().await?;
    channel.data_bytes(&b"qjj"[..]).await?;
    channel.window_change(1, 1, 0, 0).await?;
    channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    // A resize after PTY but before shell is legal and must not count as a PTY.
    channel.window_change(100, 30, 0, 0).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;
    let mut terminal = vt100::Parser::new(30, 100, 0);
    let mut output = Vec::new();
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "> 1. Rust reaches the tiny terminal",
    )
    .await?;
    assert!(!terminal.screen().contents().contains("Terminal too small"));
    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    server.stop().await
}

#[tokio::test]
async fn eof_during_setup_prevents_later_pty_or_shell_acceptance() -> TestResult {
    let server = TestServer::start().await?;
    for with_pty in [false, true] {
        let connection = server.connect().await?;
        let mut channel = connection.channel_open_session().await?;
        if with_pty {
            channel
                .request_pty(true, "xterm", 80, 24, 0, 0, &[])
                .await?;
            expect_success(&mut channel).await?;
        }
        channel.eof().await?;
        // The EOF teardown may close the transport before these requests can
        // even be written or answered. Neither is acceptance; a send error or
        // channel/connection closure is an acceptable rejection outcome.
        if channel
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await
            .is_ok()
        {
            expect_failure_or_teardown(&mut channel).await?;
        }
        if channel.request_shell(true).await.is_ok() {
            expect_failure_or_teardown(&mut channel).await?;
        }
        timeout(TEST_TIMEOUT, async {
            while !connection.is_closed() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        assert!(connection.channel_open_session().await.is_err());
    }
    server.stop().await
}

#[tokio::test]
async fn setup_deadline_starts_at_authentication_and_is_not_extended_by_requests() -> TestResult {
    let server = TestServer::start().await?;
    let (mut connection, disconnected) = server.connect_unauthenticated().await?;
    sleep(Duration::from_secs(6)).await;
    assert!(connection.authenticate_none("visitor").await?.success());
    let authenticated_at = Instant::now();
    sleep(Duration::from_secs(6)).await;
    // More than ten seconds since connection, but still inside the setup budget.
    let mut channel = connection.channel_open_session().await?;
    channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.exec(true, "sh").await?;
    expect_failure(&mut channel).await?;
    let reason = timeout(Duration::from_secs(7), disconnected).await??;
    assert!(reason.contains("ssh -t"), "unexpected disconnect: {reason}");
    assert!(authenticated_at.elapsed() >= Duration::from_secs(9));
    assert!(authenticated_at.elapsed() < Duration::from_secs(13));
    server.stop().await
}

#[tokio::test]
async fn setup_deadlines_cover_no_channel_no_pty_and_no_shell_without_harming_browsers()
-> TestResult {
    let server = TestServer::start().await?;
    let (_healthy, mut browsing, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(
        &mut browsing,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    let mut pending = Vec::new();
    let started = Instant::now();
    for stage in 0..3 {
        let (mut connection, disconnected) = server.connect_unauthenticated().await?;
        assert!(connection.authenticate_none("visitor").await?.success());
        let channel = if stage > 0 {
            let mut channel = connection.channel_open_session().await?;
            if stage == 2 {
                channel
                    .request_pty(true, "xterm", 80, 24, 0, 0, &[])
                    .await?;
                expect_success(&mut channel).await?;
            }
            Some(channel)
        } else {
            None
        };
        pending.push((connection, channel, disconnected));
    }
    // All three expire concurrently at the production deadline. Keeping their
    // handles/channels alive ensures client teardown is not the cause of closure.
    for (connection, _channel, disconnected) in pending {
        let reason = timeout(Duration::from_secs(15), disconnected).await??;
        assert!(
            reason.contains("PTY and shell"),
            "unexpected disconnect: {reason}"
        );
        assert!(reason.contains("ssh -t"));
        assert!(connection.is_closed());
    }
    assert!(started.elapsed() >= Duration::from_secs(9));
    assert!(started.elapsed() < Duration::from_secs(15));
    // A successfully established shell is not subject to the setup deadline.
    browsing.data_bytes(&b"j"[..]).await?;
    read_until_visible(&mut browsing, &mut terminal, &mut output, "> 2. Ask HN:").await?;
    browsing.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut browsing, &mut output).await?, 0);
    server.stop().await
}

async fn expect_transport_closed(stream: &mut TcpStream) -> TestResult {
    let mut bytes = [0; 4096];
    loop {
        match stream.read(&mut bytes).await {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[tokio::test]
async fn handshake_deadline_covers_stalled_identification_key_exchange_and_authentication()
-> TestResult {
    let server = TestServer::start().await?;
    let started = Instant::now();
    let mut silent = TcpStream::connect(server.address).await?;
    let mut stalled_kex = TcpStream::connect(server.address).await?;
    stalled_kex.write_all(b"SSH-2.0-hnssh-test\r\n").await?;
    let (_unauthenticated, disconnected) = server.connect_unauthenticated().await?;
    timeout(Duration::from_secs(15), async {
        expect_transport_closed(&mut silent).await?;
        expect_transport_closed(&mut stalled_kex).await?;
        let reason = disconnected.await?;
        assert!(
            reason.contains("handshake timed out"),
            "unexpected disconnect: {reason}"
        );
        assert!(reason.contains("Reconnect"));
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    assert!(started.elapsed() >= Duration::from_secs(9));
    // One connection's handshake expiry does not stop acceptance.
    let (_connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(&mut channel, &mut terminal, &mut output, "Top feed").await?;
    channel.data_bytes(&b"q"[..]).await?;
    assert_eq!(read_through_close(&mut channel, &mut output).await?, 0);
    server.stop().await
}

#[tokio::test]
async fn shutdown_closes_connections_at_every_setup_stage_without_waiting_for_deadlines()
-> TestResult {
    let server = TestServer::start().await?;
    let mut silent = TcpStream::connect(server.address).await?;
    let mut stalled_kex = TcpStream::connect(server.address).await?;
    stalled_kex.write_all(b"SSH-2.0-hnssh-test\r\n").await?;
    let (_unauthenticated, unauthenticated_closed) = server.connect_unauthenticated().await?;
    let (mut authenticated, authenticated_closed) = server.connect_unauthenticated().await?;
    assert!(authenticated.authenticate_none("visitor").await?.success());
    let (mut with_pty, pty_closed) = server.connect_unauthenticated().await?;
    assert!(with_pty.authenticate_none("visitor").await?.success());
    let mut channel = with_pty.channel_open_session().await?;
    channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;

    timeout(TEST_TIMEOUT, async {
        server.stop().await?;
        expect_transport_closed(&mut silent).await?;
        expect_transport_closed(&mut stalled_kex).await?;
        for closed in [unauthenticated_closed, authenticated_closed, pty_closed] {
            assert!(closed.await?.contains("shutting down"));
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn cancelling_the_server_future_closes_setup_transports() -> TestResult {
    let server = TestServer::start().await?;
    let mut silent = TcpStream::connect(server.address).await?;
    let mut stalled_kex = TcpStream::connect(server.address).await?;
    stalled_kex.write_all(b"SSH-2.0-hnssh-test\r\n").await?;
    let (mut connection, disconnected) = server.connect_unauthenticated().await?;
    assert!(connection.authenticate_none("visitor").await?.success());
    server.task.abort();
    assert!(server.task.await.unwrap_err().is_cancelled());
    timeout(TEST_TIMEOUT, async {
        expect_transport_closed(&mut silent).await?;
        expect_transport_closed(&mut stalled_kex).await?;
        disconnected.await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn shell_without_a_pty_gets_reconnection_help() -> TestResult {
    let server = TestServer::start().await?;
    let connection = server.connect().await?;
    let mut channel = connection.channel_open_session().await?;

    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;

    let mut raw_output = Vec::new();
    let status = read_through_close(&mut channel, &mut raw_output).await?;
    assert_eq!(status, 1);
    let output = String::from_utf8(raw_output)?;
    assert!(output.contains("HNSSH requires an interactive terminal"));
    assert!(output.contains("ssh -t <hostname>"));
    assert!(!output.contains("\x1b[?1049h"));
    timeout(TEST_TIMEOUT, async {
        while !connection.is_closed() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert!(connection.channel_open_session().await.is_err());
    server.stop().await
}
