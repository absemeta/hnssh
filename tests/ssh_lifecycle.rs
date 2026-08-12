use std::fs;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hnssh::{
    ContentError, ContentSource, FeedEntry, FeedEntryId, FixtureContent, Server, ServerConfig,
};
use russh::client;
use russh::keys::{Algorithm, PrivateKey, ssh_key};
use russh::{Channel, ChannelMsg, client::AuthResult};
use tempfile::TempDir;
use tokio::sync::Semaphore;
use tokio::time::timeout;
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
