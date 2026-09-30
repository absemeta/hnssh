//! Uses the same real-SSH / fake-content / terminal-emulator seam as the
//! browsing tests. Pausing encrypted transport reads is intentional: unlike
//! leaving Channel::wait unread, it actually stops SSH WINDOW_ADJUST packets.
use super::*;
use std::sync::atomic::AtomicBool;

#[path = "ssh_adversarial_transport.rs"]
mod adversarial_transport;
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Default)]
struct ReadGate {
    paused: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl ReadGate {
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }
    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }
}

struct ControlledTransport {
    stream: TcpStream,
    gate: Arc<ReadGate>,
}

impl AsyncRead for ControlledTransport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.gate.paused.load(Ordering::SeqCst) {
            *this.gate.waker.lock().unwrap() = Some(cx.waker().clone());
            if this.gate.paused.load(Ordering::SeqCst) {
                return Poll::Pending;
            }
        }
        Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for ControlledTransport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

struct ControlledClient {
    expected: ssh_key::PublicKey,
    gate: Arc<ReadGate>,
    successes: usize,
}

impl client::Handler for ControlledClient {
    type Error = russh::Error;
    async fn check_server_key(&mut self, key: &ssh_key::PublicKey) -> Result<bool, Self::Error> {
        Ok(key == &self.expected)
    }
    async fn channel_success(
        &mut self,
        _: russh::ChannelId,
        _: &mut client::Session,
    ) -> Result<(), Self::Error> {
        self.successes += 1;
        // Freeze before receiving screen data, after PTY + shell replies. The
        // small-window tests exhaust SSH credit; large-window tests saturate
        // TCP buffers. Outbound input/resize still travels normally here.
        // There is no application-queue simulation.
        if self.successes == 2 {
            self.gate.pause();
        }
        Ok(())
    }
}

struct FlowVisitor {
    _connection: client::Handle<ControlledClient>,
    channel: Channel<client::Msg>,
    gate: Arc<ReadGate>,
    socket: std::net::TcpStream,
}

impl FlowVisitor {
    async fn open(server: &TestServer) -> TestResult<Self> {
        Self::open_with_window(server, 1024).await
    }

    async fn open_with_window(server: &TestServer, window_size: u32) -> TestResult<Self> {
        let tcp = tokio::net::TcpSocket::new_v4()?;
        if window_size == u32::MAX {
            // Limit TCP receive buffering, independently of SSH credit. This
            // exercises a socket write stall rather than SSH-window exhaustion.
            tcp.set_recv_buffer_size(1024)?;
            assert!(tcp.recv_buffer_size()? <= 16 * 1024);
        }
        let stream = tcp.connect(server.address).await?.into_std()?;
        let socket = stream.try_clone()?;
        let stream = TcpStream::from_std(stream)?;
        let gate = Arc::new(ReadGate::default());
        let config = Arc::new(client::Config {
            window_size,
            maximum_packet_size: if window_size == u32::MAX { 32768 } else { 1024 },
            ..Default::default()
        });
        let mut connection = timeout(
            TEST_TIMEOUT,
            client::connect_stream(
                config,
                ControlledTransport {
                    stream,
                    gate: Arc::clone(&gate),
                },
                ControlledClient {
                    expected: server.public_key.clone(),
                    gate: Arc::clone(&gate),
                    successes: 0,
                },
            ),
        )
        .await??;
        assert!(connection.authenticate_none("visitor").await?.success());
        let mut channel = connection.channel_open_session().await?;
        channel
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await?;
        expect_success(&mut channel).await?;
        channel.request_shell(true).await?;
        expect_success(&mut channel).await?;
        assert!(gate.paused.load(Ordering::SeqCst));
        Ok(Self {
            _connection: connection,
            channel,
            gate,
            socket,
        })
    }
}

impl Drop for FlowVisitor {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        self.gate.resume();
    }
}

struct ObservedFixture {
    fixture: FixtureContent,
    refreshes: Arc<Semaphore>,
}

impl ContentSource for ObservedFixture {
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
    fn refresh_top_feed(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>>
                + Send
                + '_,
        >,
    > {
        self.refreshes.add_permits(1);
        self.fixture.top_feed()
    }
    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<FeedEntry>, ContentError>> + Send + '_>>
    {
        self.fixture.feed_entry(id)
    }
}

async fn transport_closes_without_replenishing_ssh_credit(visitor: &FlowVisitor) -> TestResult {
    // Drain encrypted TCP bytes directly, not through the SSH client. This
    // observes FIN/reset without acknowledging any more SSH channel credit.
    // It also proves q/EOF close an unwritable transport before server.stop.
    let mut raw = TcpStream::from_std(visitor.socket.try_clone()?)?;
    timeout(TEST_TIMEOUT, async {
        let mut bytes = [0; 4096];
        loop {
            match raw.read(&mut bytes).await {
                Ok(0) => return Ok::<_, std::io::Error>(()),
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await??;
    Ok(())
}

async fn wait_count(count: &AtomicUsize, expected: usize) -> TestResult {
    timeout(TEST_TIMEOUT, async {
        while count.load(Ordering::SeqCst) < expected {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    Ok(())
}

async fn read_restored_close(
    channel: &mut Channel<client::Msg>,
    terminal: &mut vt100::Parser,
) -> TestResult {
    timeout(TEST_TIMEOUT, async {
        let mut status = false;
        let mut eof = false;
        loop {
            match channel
                .wait()
                .await
                .ok_or("closed without ordered cleanup")?
            {
                ChannelMsg::Data { data } => {
                    assert!(!status && !eof, "data after exit status/EOF");
                    terminal.process(&data);
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    assert_eq!(exit_status, 0);
                    assert!(!status && !eof);
                    assert!(!terminal.screen().alternate_screen());
                    assert!(!terminal.screen().hide_cursor());
                    assert_eq!(terminal.screen().fgcolor(), vt100::Color::Default);
                    assert_eq!(terminal.screen().bgcolor(), vt100::Color::Default);
                    status = true;
                }
                ChannelMsg::Eof => {
                    assert!(status);
                    eof = true;
                }
                ChannelMsg::Close => {
                    assert!(status && eof);
                    return Ok::<_, Box<dyn std::error::Error>>(());
                }
                _ => {}
            }
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn huge_and_zero_viewports_preserve_clamped_usable_dimensions() -> TestResult {
    let server = TestServer::start().await?;
    let connection = server.connect().await?;
    let mut channel = connection.channel_open_session().await?;
    channel
        .request_pty(true, "xterm", u32::MAX, u32::MAX, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;
    let mut terminal = vt100::Parser::new(120, 300, 0);
    let mut output = Vec::new();
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut output,
        "300x120 frame",
        |screen| {
            screen.contains("Rust reaches the tiny terminal")
                && screen
                    .lines()
                    .nth(118)
                    .is_some_and(|row| row.starts_with('└') && row.chars().count() == 300)
        },
    )
    .await?;
    channel.window_change(0, 0, 0, 0).await?;
    channel.data_bytes(&b"?"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut output,
        "zero resize retains 300x120",
        |screen| {
            screen.contains("Keyboard help")
                && screen
                    .lines()
                    .nth(119)
                    .is_some_and(|row| row.starts_with('└') && row.chars().count() == 300)
        },
    )
    .await?;
    // Partial zero dimensions preserve each previous usable axis independently.
    terminal.screen_mut().set_size(24, 80);
    channel.window_change(80, 24, 0, 0).await?;
    channel.window_change(0, 18, 0, 0).await?;
    terminal.screen_mut().set_size(18, 80);
    channel.data_bytes(&[0x7f][..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut output,
        "80x18 after zero columns",
        |screen| {
            screen.contains("Top feed")
                && screen
                    .lines()
                    .nth(16)
                    .is_some_and(|row| row.starts_with('└') && row.chars().count() == 80)
        },
    )
    .await?;
    channel.window_change(60, 0, 0, 0).await?;
    terminal.screen_mut().set_size(18, 60);
    channel.data_bytes(&b"?"[..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut output,
        "60x18 after zero rows",
        |screen| {
            screen.contains("Keyboard help")
                && screen
                    .lines()
                    .nth(17)
                    .is_some_and(|row| row.starts_with('└') && row.chars().count() == 60)
        },
    )
    .await?;
    terminal.screen_mut().set_size(120, 300);
    channel.window_change(u32::MAX, u32::MAX, 0, 0).await?;
    channel.data_bytes(&[0x7f][..]).await?;
    read_until_screen_matches(
        &mut channel,
        &mut terminal,
        &mut output,
        "huge window-change clamps to 300x120",
        |screen| {
            screen.contains("Top feed")
                && screen
                    .lines()
                    .nth(118)
                    .is_some_and(|row| row.starts_with('└') && row.chars().count() == 300)
        },
    )
    .await?;
    channel.data_bytes(&b"q"[..]).await?;
    read_restored_close(&mut channel, &mut terminal).await?;
    server.stop().await
}

#[tokio::test]
async fn exhausted_ssh_window_coalesces_frames_and_resize_storms_then_recovers_exact_screen()
-> TestResult {
    let refreshes = Arc::new(Semaphore::new(0));
    let server = TestServer::start_with_content(Arc::new(ObservedFixture {
        fixture: FixtureContent::default(),
        refreshes: Arc::clone(&refreshes),
    }))
    .await?;
    let mut slow = FlowVisitor::open(&server).await?;
    slow.gate.resume();
    let mut terminal = vt100::Parser::new(24, 80, 0);
    let mut output = Vec::new();
    read_until_visible(
        &mut slow.channel,
        &mut terminal,
        &mut output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    slow.gate.pause();
    let (_healthy, mut healthy_channel, mut healthy_terminal, mut healthy_output) =
        open_terminal(&server).await?;
    read_until_visible(
        &mut healthy_channel,
        &mut healthy_terminal,
        &mut healthy_output,
        "Rust reaches the tiny terminal",
    )
    .await?;
    // Exercise thousands of resizes without waiting for any output. The
    // authenticated healthy visitor must remain usable in the meantime.
    timeout(TEST_TIMEOUT, async {
        for n in 0..2000 {
            slow.channel
                .window_change(
                    if n % 2 == 0 { u32::MAX } else { 60 },
                    if n % 3 == 0 { 120 } else { 18 },
                    0,
                    0,
                )
                .await?;
        }
        slow.channel.window_change(80, 24, 0, 0).await?;
        slow.channel.data_bytes(b"?\x7fjjg?r".as_slice()).await?;
        // Return to Feed then refresh: observable acquisition is a barrier
        // proving input is processed while the SSH receive window is frozen.
        slow.channel.data_bytes(b"\x7fr".as_slice()).await?;
        Ok::<_, russh::Error>(())
    })
    .await??;
    let permit = timeout(TEST_TIMEOUT, refreshes.acquire()).await??;
    permit.forget();
    healthy_channel.data_bytes(b"j?".as_slice()).await?;
    read_until_visible(
        &mut healthy_channel,
        &mut healthy_terminal,
        &mut healthy_output,
        "Keyboard help",
    )
    .await?;
    slow.channel.data_bytes(b"?".as_slice()).await?;
    slow.gate.resume();
    read_until_screen_matches(
        &mut slow.channel,
        &mut terminal,
        &mut output,
        "exact healthy help screen after dropped updates",
        |screen| screen == healthy_terminal.screen().contents(),
    )
    .await?;
    assert!(terminal.screen().alternate_screen());
    assert!(terminal.screen().hide_cursor());
    assert!(
        output.len() < 2 * 1024 * 1024,
        "superseded resize frames accumulated: {} output bytes",
        output.len()
    );
    // Recovery must still handle incremental navigation, not just one repaint.
    slow.channel.data_bytes(b"\x7fgj".as_slice()).await?;
    read_until_visible(
        &mut slow.channel,
        &mut terminal,
        &mut output,
        "> 2. Ask HN:",
    )
    .await?;
    // Repeat without a viewport change: recovery must preserve true diffs,
    // including removing superseded focus markers, not rely on resize clears.
    slow.gate.pause();
    slow.channel.data_bytes(b"jjjkkjjjjr".as_slice()).await?;
    let permit = timeout(TEST_TIMEOUT, refreshes.acquire()).await??;
    permit.forget();
    healthy_channel
        .data_bytes(b"\x7fgjjjjjjr".as_slice())
        .await?;
    read_until_screen_matches(
        &mut healthy_channel,
        &mut healthy_terminal,
        &mut healthy_output,
        "refreshed healthy Feed position",
        |screen| {
            screen
                .lines()
                .nth(1)
                .is_some_and(|row| row.contains("> 7."))
                && !screen.contains("refreshing")
        },
    )
    .await?;
    slow.gate.resume();
    read_until_screen_matches(
        &mut slow.channel,
        &mut terminal,
        &mut output,
        "exact Feed screen after coalesced same-size diffs",
        |screen| screen == healthy_terminal.screen().contents(),
    )
    .await?;
    slow.channel.data_bytes(b"q".as_slice()).await?;
    read_restored_close(&mut slow.channel, &mut terminal).await?;
    healthy_channel.data_bytes(b"q".as_slice()).await?;
    read_restored_close(&mut healthy_channel, &mut healthy_terminal).await?;
    server.stop().await
}

#[tokio::test]
async fn eof_quit_and_shutdown_restore_writable_screens_and_cancel_acquisition() -> TestResult {
    for ending in ["eof", "q", "ctrl-c", "shutdown"] {
        let started = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let server = TestServer::start_with_content(Arc::new(PendingCommentContent {
            started: Arc::clone(&started),
            cancelled: Arc::clone(&cancelled),
        }))
        .await?;
        let (_connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
        read_until_visible(
            &mut channel,
            &mut terminal,
            &mut output,
            "Cancellation story",
        )
        .await?;
        channel.data_bytes(b"\r".as_slice()).await?;
        wait_count(&started, 1).await?;
        match ending {
            "eof" => channel.eof().await?,
            "q" => channel.data_bytes(b"q".as_slice()).await?,
            "ctrl-c" => channel.data_bytes(b"\x03".as_slice()).await?,
            "shutdown" => server.shutdown.cancel(),
            _ => unreachable!(),
        }
        read_restored_close(&mut channel, &mut terminal).await?;
        wait_count(&cancelled, 1).await?;
        assert_eq!(
            cancelled.load(Ordering::SeqCst),
            started.load(Ordering::SeqCst)
        );
        server.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn nonreading_peers_cancel_work_on_quit_eof_close_loss_shutdown_and_forced_cancellation()
-> TestResult {
    for ending in ["q", "eof", "close", "loss", "shutdown", "abort-server"] {
        let started = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let server = TestServer::start_with_content(Arc::new(PendingCommentContent {
            started: Arc::clone(&started),
            cancelled: Arc::clone(&cancelled),
        }))
        .await?;
        let mut visitor = FlowVisitor::open(&server).await?;
        // A healthy screen proves entry data is ready for the paused visitor.
        let (_healthy, mut healthy, mut terminal, mut output) = open_terminal(&server).await?;
        read_until_visible(
            &mut healthy,
            &mut terminal,
            &mut output,
            "Cancellation story",
        )
        .await?;
        visitor.gate.resume();
        let mut slow_terminal = vt100::Parser::new(24, 80, 0);
        let mut slow_output = Vec::new();
        read_until_visible(
            &mut visitor.channel,
            &mut slow_terminal,
            &mut slow_output,
            "Cancellation story",
        )
        .await?;
        visitor.gate.pause();
        visitor.channel.data_bytes(b"\r".as_slice()).await?;
        wait_count(&started, 1).await?;
        // Output remains unwritable throughout cancellation; no read resumes
        // until after the server has cancelled the pending content future.
        match ending {
            "q" => visitor.channel.data_bytes(b"q".as_slice()).await?,
            "eof" => visitor.channel.eof().await?,
            "close" => visitor.channel.close().await?,
            "loss" => visitor.socket.shutdown(std::net::Shutdown::Both)?,
            "shutdown" => server.shutdown.cancel(),
            "abort-server" => server.task.abort(),
            _ => unreachable!(),
        }
        wait_count(&cancelled, 1).await?;
        assert_eq!(
            cancelled.load(Ordering::SeqCst),
            started.load(Ordering::SeqCst)
        );
        if ending != "close" {
            transport_closes_without_replenishing_ssh_credit(&visitor)
                .await
                .map_err(|error| {
                    format!("{ending}: transport did not close without new SSH credit: {error}")
                })?;
        }
        if ending == "abort-server" {
            assert!(
                timeout(TEST_TIMEOUT, server.task)
                    .await?
                    .unwrap_err()
                    .is_cancelled()
            );
        } else {
            if !matches!(ending, "shutdown") {
                healthy.data_bytes(b"?".as_slice()).await?;
                read_until_visible(&mut healthy, &mut terminal, &mut output, "Keyboard help")
                    .await?;
            }
            server.stop().await?;
        }
        visitor.gate.resume();
        // Server closure is observable once reads resume; no detached sender
        // can keep the connection or acquisition alive.
        timeout(TEST_TIMEOUT, async {
            while visitor.channel.wait().await.is_some() {}
        })
        .await?;
    }
    Ok(())
}
