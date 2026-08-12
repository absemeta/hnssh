use std::collections::{HashMap, VecDeque};
use std::fs;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hnssh::{
    ContentError, ContentSource, FeedEntry, FeedEntryId, FixtureContent, HackerNewsContent, Server,
    ServerConfig,
};
use russh::client;
use russh::keys::{Algorithm, PrivateKey, ssh_key};
use russh::{Channel, ChannelMsg, client::AuthResult};
use tempfile::TempDir;
use tokio::sync::Semaphore;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct TrustExpectedHostKey {
    expected: ssh_key::PublicKey,
}

impl client::Handler for TrustExpectedHostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(server_public_key == &self.expected)
    }
}

struct TestServer {
    address: SocketAddr,
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
        let (key_dir, host_key_path, public_key) = make_host_key()?;
        let server = Server::bind_with_content(
            ServerConfig {
                listen_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                host_key_path,
            },
            content,
        )
        .await?;
        let address = server.local_addr()?;
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.run(shutdown.clone()));

        Ok(Self {
            address,
            shutdown,
            task,
            _key_dir: key_dir,
            public_key,
        })
    }

    async fn connect(&self) -> TestResult<client::Handle<TrustExpectedHostKey>> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(TEST_TIMEOUT),
            ..Default::default()
        });
        let handler = TrustExpectedHostKey {
            expected: self.public_key.clone(),
        };
        let mut connection = client::connect(config, self.address, handler).await?;
        let result: AuthResult = connection.authenticate_none("ignored-visitor-name").await?;
        assert!(result.success(), "none authentication should succeed");
        Ok(connection)
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
    ) -> Pin<Box<dyn Future<Output = Result<Vec<FeedEntryId>, ContentError>> + Send + '_>> {
        self.fixture.top_feed()
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<FeedEntry, ContentError>> + Send + '_>> {
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

#[derive(Clone)]
struct FakeReply {
    status: StatusCode,
    body: String,
    delay: Duration,
}

impl FakeReply {
    fn json(body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn status(status: StatusCode) -> Self {
        Self {
            status,
            body: "null".to_owned(),
            delay: Duration::ZERO,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

struct FakeHackerNewsState {
    replies: Mutex<HashMap<String, VecDeque<FakeReply>>>,
    requests: Mutex<HashMap<String, usize>>,
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

    (
        reply.status,
        [("content-type", "application/json")],
        reply.body,
    )
        .into_response()
}

fn make_host_key() -> TestResult<(TempDir, PathBuf, ssh_key::PublicKey)> {
    let key_dir = tempfile::tempdir()?;
    let path = key_dir.path().join("host_key");
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let public_key = key.public_key().clone();
    fs::write(&path, key.to_openssh(ssh_key::LineEnding::LF)?.as_bytes())?;
    Ok((key_dir, path, public_key))
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
    timeout(TEST_TIMEOUT, async {
        loop {
            let message = channel
                .wait()
                .await
                .ok_or("SSH channel closed before expected screen appeared")?;
            if let ChannelMsg::Data { data } = message {
                raw_output.extend_from_slice(&data);
                terminal.process(&data);
                if terminal.screen().contents().contains(expected) {
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
    let connection = server.connect().await?;
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
async fn visitor_opens_resizes_and_quits_anonymous_screen() -> TestResult {
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
    assert!(screen.contains("q quit"));

    terminal.screen_mut().set_size(30, 100);
    channel.window_change(100, 30, 0, 0).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Top feed").await?;

    channel.data_bytes(&b"q"[..]).await?;
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

    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Entry details",
    )
    .await?;
    let detail = terminal.screen().contents();
    assert!(detail.contains("Ask HN: What are you building this week?"));
    assert!(detail.contains("A fixture discussion for trying the detail view."));
    assert!(detail.contains("Enter  View links"));

    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Entry links").await?;
    assert!(
        terminal
            .screen()
            .contents()
            .contains("https://doc.rust-lang.org/book/")
    );

    channel.data_bytes(&[0x7f][..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "Entry details",
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
        "Entry details",
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
    assert!(resize.contains("q quit"));

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
    let screen = terminal.screen().contents();
    assert!(screen.contains("> 2. Second live entry"));
    assert!(screen.contains("2. Second live entry"));
    assert!(screen.contains("sparse"));
    assert!(!screen.contains("0 points"));
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
    assert_eq!(fake.request_count("topstories"), 2);

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

    connection
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await
        .ok();
    server.stop().await
}
