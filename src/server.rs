use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use russh::keys::{PrivateKey, load_secret_key};
use russh::server::{self, Auth, Handler, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, Disconnect, MethodSet, Pty};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

const DEFAULT_SESSION_SIZE: TerminalSize = TerminalSize {
    columns: 80,
    rows: 24,
};
const MAX_COLUMNS: u16 = 300;
const MAX_ROWS: u16 = 120;
const SESSION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
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
}

impl Server {
    pub async fn bind(config: ServerConfig) -> Result<Self, ServerError> {
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
                    let session_shutdown = shutdown.child_token();
                    sessions.spawn(async move {
                        run_connection(ssh_config, stream, session_shutdown).await
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
    shutdown: CancellationToken,
) -> Result<(), SessionError> {
    let running = server::run_stream(config, stream, BrowsingSession::default()).await?;
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
    #[error("terminal rendering error")]
    Terminal(#[from] io::Error),
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

#[derive(Default)]
struct BrowsingSession {
    channel: Option<ChannelId>,
    pty_size: Option<TerminalSize>,
    terminal: Option<RemoteTerminal>,
    output: OutputBuffer,
    shell_started: bool,
    closed: bool,
}

impl BrowsingSession {
    fn owns_channel(&self, channel: ChannelId) -> bool {
        self.channel == Some(channel)
    }

    fn start_shell(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), SessionError> {
        let size = self.pty_size.unwrap_or(DEFAULT_SESSION_SIZE);
        let backend = CrosstermBackend::new(self.output.clone());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(size.area()),
            },
        )?;
        self.terminal = Some(terminal);
        self.shell_started = true;

        session.data(channel, ENTER_SCREEN.to_vec())?;
        self.render(channel, session)
    }

    fn render(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), SessionError> {
        let size = self.pty_size.unwrap_or(DEFAULT_SESSION_SIZE);
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(());
        };

        terminal.draw(|frame| {
            let welcome = vec![
                Line::from(""),
                Line::from("Hacker News over SSH"),
                Line::from(""),
                Line::from("No account, password, or visitor key required."),
                Line::from(format!("Terminal: {} x {}", size.columns, size.rows)),
                Line::from(""),
                Line::from("Press q to quit"),
            ];
            let panel = Paragraph::new(welcome)
                .alignment(Alignment::Center)
                .block(Block::default().title(" HNSSH ").borders(Borders::ALL));
            frame.render_widget(panel, frame.area());
        })?;

        let output = self.output.take()?;
        if !output.is_empty() {
            session.data(channel, output)?;
        }
        Ok(())
    }

    fn resize(
        &mut self,
        channel: ChannelId,
        columns: u32,
        rows: u32,
        session: &mut Session,
    ) -> Result<(), SessionError> {
        let previous = self.pty_size.unwrap_or(DEFAULT_SESSION_SIZE);
        let size = TerminalSize::from_protocol(columns, rows, previous);
        self.pty_size = Some(size);

        if let Some(terminal) = self.terminal.as_mut() {
            terminal.resize(size.area())?;
            self.render(channel, session)?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        channel: ChannelId,
        status: u32,
        restore_terminal: bool,
        session: &mut Session,
    ) -> Result<(), SessionError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;

        if restore_terminal {
            session.data(channel, LEAVE_SCREEN.to_vec())?;
        }
        session.exit_status_request(channel, status)?;
        session.eof(channel)?;
        session.close(channel)?;
        self.terminal = None;
        Ok(())
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

        self.resize(channel, columns, rows, session)?;
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
            return self.finish(channel, 1, false, session);
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

        self.resize(channel, columns, rows, session)?;
        session.channel_success(channel)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.owns_channel(channel) && self.shell_started && !self.closed && data.contains(&b'q')
        {
            self.finish(channel, 0, true, session)?;
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
            self.terminal = None;
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
            self.terminal = None;
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
