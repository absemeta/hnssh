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
    ContentError, ContentSnapshot, ContentSource, FeedEntry, FeedEntryId, FixtureContent,
    HackerNewsContent, HackerNewsOptions, Server, ServerConfig,
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
        "HNSSH · Entry",
    )
    .await?;
    let detail = terminal.screen().contents();
    assert!(detail.contains("Ask HN: What are you building this week?"));
    assert!(detail.contains("A fixture discussion for trying the detail view."));
    assert!(detail.contains("No comments available."));
    assert!(!detail.contains("View links"));

    channel.data_bytes(&b"l"[..]).await?;
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
        "HNSSH · Entry",
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
        "HNSSH · Entry",
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

    channel.data_bytes(&b"l"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Entry links").await?;
    let links = terminal.screen().contents();
    assert!(links.contains("https://example.com/story?x=1&y=2"));
    let long_destination = "https://docs.example/abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let compact_links = links
        .chars()
        .filter(|character| {
            !character.is_whitespace() && !matches!(character, '│' | '┌' | '┐' | '└' | '┘' | '─')
        })
        .collect::<String>();
    assert!(compact_links.contains(long_destination));
    assert!(!links.contains("javascript:"));
    assert!(!links.contains("visitor:secret"));

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
    let content = Arc::new(HackerNewsContent::for_endpoint_with_options(
        fake.endpoint(),
        fast_hacker_news_options(),
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
    sleep(Duration::from_millis(30)).await;
    assert_eq!(
        fake.request_count("topstories"),
        3,
        "revalidation is throttled for the same resource"
    );

    sleep(Duration::from_millis(70)).await;
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
    for id in 16..=31 {
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
        "Threaded story",
    )
    .await?;
    channel.data_bytes(&b"\r"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "HNSSH · Entry",
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
    assert_eq!(fake.request_count("item/29"), 1);
    assert_eq!(fake.request_count("item/30"), 0);
    assert_eq!(fake.request_count("item/290"), 0);
    assert!(fake.maximum_active_items() <= 8);
    let collapsed = terminal.screen().contents();
    assert!(collapsed.contains("[deleted comment]"));
    assert!(collapsed.contains("[dead comment]"));
    assert!(collapsed.contains("[comment missing]"));
    assert!(collapsed.contains("[unsupported story]"));
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

    channel.data_bytes(&b"l"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "https://comment.example/first",
    )
    .await?;
    assert!(terminal.screen().contents().contains("Comment 100 links"));
    channel.data_bytes(&[0x1b][..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "restored selected nested-a",
        |screen| {
            screen
                .lines()
                .any(|line| line.contains(">>") && line.contains("nested-a"))
        },
    )
    .await?;

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
    assert!(!root_header.contains("years ago"));
    channel.data_bytes(&b" "[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Nested first").await?;
    assert_eq!(fake.request_count("item/100"), 1);
    channel.data_bytes(&b"\x1b[D"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "root collapsed with Left",
        |screen| !screen.contains("Nested first") && screen.contains("root-author"),
    )
    .await?;
    channel.data_bytes(&b"\x1b[C"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "Nested first").await?;
    assert_eq!(fake.request_count("item/100"), 1);

    channel.data_bytes(&b"nnnnnnnnnnnnnnnnnnnn"[..]).await?;
    read_until_visible(&mut channel, &mut terminal, &mut raw_output, "author-30").await?;
    assert_eq!(fake.request_count("item/30"), 1);
    assert_eq!(fake.request_count("item/31"), 1);
    assert_eq!(fake.request_count("item/290"), 1);

    channel.data_bytes(&b"o"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "https://example.com/threaded",
    )
    .await?;
    assert!(terminal.screen().contents().contains("Primary destination"));
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
    channel.data_bytes(&b"l"[..]).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut raw_output,
        "https://example.com/threaded",
    )
    .await?;
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
            r#"{"id":1,"type":"story","title":"Viewport story","kids":[10,11],"descendants":3}"#,
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
