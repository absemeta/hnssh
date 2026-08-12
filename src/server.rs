use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use russh::keys::{PrivateKey, load_secret_key};
use russh::server::{self, Auth, Handler, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, Disconnect, MethodSet, Pty};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::browser::{Browser, CommandOutcome};
use crate::content::{ContentError, ContentSource, FeedEntry, FeedEntryId};
use crate::hacker_news::HackerNewsContent;
use crate::input::{InputDecoder, Key};
use crate::ui;

const DEFAULT_SESSION_SIZE: TerminalSize = TerminalSize {
    columns: 80,
    rows: 24,
};
const MAX_COLUMNS: u16 = 300;
const MAX_ROWS: u16 = 120;
const SESSION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const TOP_FEED_PAGE_SIZE: usize = 30;
const PAGE_ITEM_CONCURRENCY: usize = 8;
const ENTER_SCREEN: &[u8] = b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l";
const LEAVE_SCREEN: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?1049l";
const PTY_REQUIRED_MESSAGE: &[u8] =
    b"HNSSH requires an interactive terminal.\r\nReconnect with: ssh -t <hostname>\r\n";

type RemoteTerminal = Terminal<CrosstermBackend<OutputBuffer>>;

#[derive(Clone, Default)]
struct OutputBuffer(Arc<Mutex<Vec<u8>>>);

impl OutputBuffer {
    fn take(&self) -> io::Result<Vec<u8>> {
        let mut buffer = self
            .0
            .lock()
            .map_err(|_| io::Error::other("terminal output buffer lock poisoned"))?;
        Ok(std::mem::take(&mut *buffer))
    }
}

impl io::Write for OutputBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("terminal output buffer lock poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

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
    let running = server::run_stream(config, stream, BrowsingSession::new(content)).await?;
    let handle = running.handle();
    tokio::pin!(running);

    tokio::select! {
        result = &mut running => result,
        _ = shutdown.cancelled() => {
            handle.disconnect(
                Disconnect::ByApplication,
                "HNSSH is shutting down".to_owned(),
                "en".to_owned(),
            ).await?;
            running.await
        }
    }
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

    fn page_len(self) -> usize {
        ui::page_len(self.rows)
    }
}

fn clamp_dimension(value: u32, previous: u16, maximum: u16) -> u16 {
    if value == 0 {
        previous
    } else {
        value.min(u32::from(maximum)) as u16
    }
}

enum ActorEvent {
    TopFeed(Result<Vec<FeedEntryId>, ContentError>),
    FeedEntry {
        index: usize,
        id: FeedEntryId,
        result: Result<FeedEntry, ContentError>,
    },
    Input(Key),
    Resize(TerminalSize),
    Stop,
}

#[derive(Debug, Error)]
enum ActorError {
    #[error("terminal rendering error")]
    Terminal(#[from] io::Error),
    #[error("SSH output channel closed")]
    OutputClosed,
}

struct SessionActor {
    channel: ChannelId,
    size: TerminalSize,
    source: Arc<dyn ContentSource>,
    browser: Browser,
    terminal: RemoteTerminal,
    output: OutputBuffer,
    handle: server::Handle,
}

impl SessionActor {
    fn new(
        channel: ChannelId,
        size: TerminalSize,
        source: Arc<dyn ContentSource>,
        handle: server::Handle,
    ) -> Result<Self, ActorError> {
        let output = OutputBuffer::default();
        let backend = CrosstermBackend::new(output.clone());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(size.area()),
            },
        )?;

        Ok(Self {
            channel,
            size,
            source,
            browser: Browser::new(),
            terminal,
            output,
            handle,
        })
    }

    async fn run(
        mut self,
        mut events: UnboundedReceiver<ActorEvent>,
        event_sender: UnboundedSender<ActorEvent>,
    ) -> Result<(), ActorError> {
        self.render().await?;
        self.load_top_feed(event_sender.clone());

        while let Some(event) = events.recv().await {
            match event {
                ActorEvent::TopFeed(result) => {
                    let result = result.map(|mut ids| {
                        ids.truncate(TOP_FEED_PAGE_SIZE);
                        ids
                    });
                    let ids = result.as_ref().ok().cloned();
                    self.browser.set_top_feed(result);
                    self.render().await?;
                    if let Some(ids) = ids {
                        self.load_entries(ids, &event_sender);
                    }
                }
                ActorEvent::FeedEntry { index, id, result } => {
                    self.browser.set_feed_entry(index, id, result);
                    self.render().await?;
                }
                ActorEvent::Input(key) => {
                    match self.browser.handle_key(key, self.size.page_len()) {
                        CommandOutcome::Redraw => self.render().await?,
                        CommandOutcome::ReloadTopFeed => {
                            self.render().await?;
                            self.load_top_feed(event_sender.clone());
                        }
                        CommandOutcome::Unchanged => {}
                        CommandOutcome::Quit => {
                            self.finish().await;
                            return Ok(());
                        }
                    }
                }
                ActorEvent::Resize(size) => {
                    self.size = size;
                    self.terminal.resize(size.area())?;
                    self.render().await?;
                }
                ActorEvent::Stop => return Ok(()),
            }
        }

        Ok(())
    }

    fn load_top_feed(&self, events: UnboundedSender<ActorEvent>) {
        let source = Arc::clone(&self.source);
        tokio::spawn(async move {
            let result = source.top_feed().await;
            let _ = events.send(ActorEvent::TopFeed(result));
        });
    }

    fn load_entries(&self, ids: Vec<FeedEntryId>, events: &UnboundedSender<ActorEvent>) {
        let page_limit = Arc::new(Semaphore::new(PAGE_ITEM_CONCURRENCY));
        for (index, id) in ids.into_iter().enumerate() {
            let source = Arc::clone(&self.source);
            let events = events.clone();
            let page_limit = Arc::clone(&page_limit);
            tokio::spawn(async move {
                let result = match page_limit.acquire_owned().await {
                    Ok(_permit) => source.feed_entry(id).await,
                    Err(_) => Err(ContentError::Retryable),
                };
                let _ = events.send(ActorEvent::FeedEntry { index, id, result });
            });
        }
    }

    async fn render(&mut self) -> Result<(), ActorError> {
        let browser = &self.browser;
        let size = self.size.area();
        self.terminal
            .draw(|frame| ui::render(frame, browser, size))?;
        let output = self.output.take()?;
        if !output.is_empty() {
            self.handle
                .data(self.channel, output)
                .await
                .map_err(|_| ActorError::OutputClosed)?;
        }
        Ok(())
    }

    async fn finish(&mut self) {
        let _ = self.handle.data(self.channel, LEAVE_SCREEN.to_vec()).await;
        let _ = self.handle.exit_status_request(self.channel, 0).await;
        let _ = self.handle.eof(self.channel).await;
        let _ = self.handle.close(self.channel).await;
    }
}

fn spawn_session_actor(
    channel: ChannelId,
    size: TerminalSize,
    source: Arc<dyn ContentSource>,
    handle: server::Handle,
) -> (UnboundedSender<ActorEvent>, JoinHandle<()>) {
    let (event_sender, event_receiver) = unbounded_channel();
    let actor_events = event_sender.clone();
    let task = tokio::spawn(async move {
        let result = match SessionActor::new(channel, size, source, handle) {
            Ok(actor) => actor.run(event_receiver, actor_events).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            debug!(%error, "browsing session actor ended with an error");
        }
    });
    (event_sender, task)
}

struct BrowsingSession {
    channel: Option<ChannelId>,
    pty_size: Option<TerminalSize>,
    shell_started: bool,
    closed: bool,
    content: Arc<dyn ContentSource>,
    input: InputDecoder,
    events: Option<UnboundedSender<ActorEvent>>,
    actor: Option<JoinHandle<()>>,
}

impl BrowsingSession {
    fn new(content: Arc<dyn ContentSource>) -> Self {
        Self {
            channel: None,
            pty_size: None,
            shell_started: false,
            closed: false,
            content,
            input: InputDecoder::default(),
            events: None,
            actor: None,
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
        session.data(channel, ENTER_SCREEN.to_vec())?;
        let (events, actor) =
            spawn_session_actor(channel, size, Arc::clone(&self.content), session.handle());
        self.events = Some(events);
        self.actor = Some(actor);
        Ok(())
    }

    fn resize(&mut self, size: TerminalSize) {
        self.pty_size = Some(size);
        if let Some(events) = &self.events {
            let _ = events.send(ActorEvent::Resize(size));
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

    fn stop_actor(&mut self) {
        if let Some(events) = self.events.take() {
            let _ = events.send(ActorEvent::Stop);
        }
    }
}

impl Drop for BrowsingSession {
    fn drop(&mut self) {
        self.stop_actor();
        if let Some(actor) = self.actor.take() {
            actor.abort();
        }
    }
}

impl Handler for BrowsingSession {
    type Error = SessionError;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
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
            reply.accept().await;
        }
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
        if !self.owns_channel(channel) || self.pty_size.is_some() || self.shell_started {
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
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.owns_channel(channel) || !self.shell_started || self.closed {
            return Ok(());
        }

        if let Some(events) = &self.events {
            for key in self.input.push(data) {
                let _ = events.send(ActorEvent::Input(key));
            }
        }
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

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.owns_channel(channel) {
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
            self.closed = true;
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
}
