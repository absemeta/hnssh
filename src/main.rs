use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

mod systemd_activation;

use clap::Parser;
use hnssh::{AdmissionLimits, Server, ServerConfig, SessionTimeouts, privacy_safe_process_entry};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(about = "Browse Hacker News over anonymous SSH")]
struct Arguments {
    /// SSH address used when no systemd listener is inherited.
    #[arg(long, env = "HNSSH_LISTEN_ADDRESS", default_value = "127.0.0.1:2222")]
    listen_address: SocketAddr,

    /// Existing SSH private host key. HNSSH never generates or replaces it.
    #[arg(long, env = "HNSSH_HOST_KEY", value_name = "PATH")]
    host_key: PathBuf,

    /// Loopback-only address for liveness, readiness, and aggregate metrics.
    #[arg(
        long,
        env = "HNSSH_OPERATIONS_ADDRESS",
        default_value = "127.0.0.1:9090"
    )]
    operations_address: SocketAddr,

    /// Maximum SSH handshakes and PTY/shell setups in progress.
    #[arg(long, env = "HNSSH_MAX_HANDSHAKES", default_value_t = 32)]
    max_handshakes: usize,

    /// Maximum established browsing sessions.
    #[arg(long, env = "HNSSH_MAX_SESSIONS", default_value_t = 200)]
    max_sessions: usize,

    /// Maximum simultaneous TCP connections from one transformed source.
    #[arg(long, env = "HNSSH_MAX_SOURCE_CONNECTIONS", default_value_t = 8)]
    max_source_connections: usize,

    /// Maximum short-lived transformed-source buckets retained in memory.
    #[arg(long, env = "HNSSH_MAX_SOURCE_BUCKETS", default_value_t = 4096)]
    max_source_buckets: usize,

    /// Source attempts replenished during one configured window.
    #[arg(long, env = "HNSSH_SOURCE_ATTEMPTS_PER_WINDOW", default_value_t = 30)]
    source_attempts_per_window: u32,

    /// Maximum immediate source-attempt burst.
    #[arg(long, env = "HNSSH_SOURCE_ATTEMPT_BURST", default_value_t = 10)]
    source_attempt_burst: u32,

    /// Source-attempt rate window in seconds.
    #[arg(
        long,
        env = "HNSSH_SOURCE_ATTEMPT_WINDOW_SECONDS",
        default_value_t = 60
    )]
    source_attempt_window_seconds: u64,

    /// Seconds without visitor input before session closure.
    #[arg(long, env = "HNSSH_INPUT_IDLE_SECONDS", default_value_t = 15 * 60)]
    input_idle_seconds: u64,

    /// Absolute browsing-session lifetime in seconds.
    #[arg(long, env = "HNSSH_MAX_SESSION_SECONDS", default_value_t = 2 * 60 * 60)]
    max_session_seconds: u64,

    /// Seconds before either timeout to show its warning.
    #[arg(long, env = "HNSSH_TIMEOUT_WARNING_SECONDS", default_value_t = 60)]
    timeout_warning_seconds: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    privacy_safe_process_entry(production_main)
}

fn production_main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "hnssh=info".into()))
        .init();
    let arguments = Arguments::parse();
    let inherited_listener = systemd_activation::take_listener()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(arguments, inherited_listener))
}

async fn run(
    arguments: Arguments,
    inherited_listener: Option<std::net::TcpListener>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        listen_address: arguments.listen_address,
        operations_address: arguments.operations_address,
        host_key_path: arguments.host_key,
        admission_limits: AdmissionLimits {
            maximum_handshakes: arguments.max_handshakes,
            maximum_browsing_sessions: arguments.max_sessions,
            maximum_connections_per_source: arguments.max_source_connections,
            maximum_source_buckets: arguments.max_source_buckets,
            source_attempts_per_window: arguments.source_attempts_per_window,
            source_attempt_burst: arguments.source_attempt_burst,
            source_attempt_window: Duration::from_secs(arguments.source_attempt_window_seconds),
        },
        session_timeouts: SessionTimeouts {
            input_idle: Duration::from_secs(arguments.input_idle_seconds),
            maximum_lifetime: Duration::from_secs(arguments.max_session_seconds),
            warning_lead: Duration::from_secs(arguments.timeout_warning_seconds),
        },
    };
    let socket_activated = inherited_listener.is_some();
    let server = match inherited_listener {
        Some(listener) => Server::from_listener(config, listener).await?,
        None => Server::bind(config).await?,
    };
    if socket_activated {
        info!("HNSSH using systemd-inherited SSH listener");
    }
    info!(listener.address = %server.local_addr()?, "HNSSH SSH listener initialized");
    info!(operations.address = %server.operations_addr(), "HNSSH operations listener initialized");

    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    let signal_task = tokio::spawn(async move {
        match shutdown_signal().await {
            Ok(()) => signal.cancel(),
            Err(error) => {
                warn!(signal.failure = "register", %error, "could not listen for shutdown signal")
            }
        }
    });

    let result = server.run(shutdown).await;
    signal_task.abort();
    let _ = signal_task.await;
    result?;
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
