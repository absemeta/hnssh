use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use hnssh::{Server, ServerConfig};
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(about = "Browse Hacker News over anonymous SSH")]
struct Arguments {
    /// Address used by the development SSH listener.
    #[arg(long, env = "HNSSH_LISTEN_ADDRESS", default_value = "127.0.0.1:2222")]
    listen_address: SocketAddr,

    /// Existing SSH private host key. HNSSH never generates or replaces it.
    #[arg(long, env = "HNSSH_HOST_KEY", value_name = "PATH")]
    host_key: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "hnssh=info".into()))
        .init();

    let arguments = Arguments::parse();
    let server = Server::bind(ServerConfig {
        listen_address: arguments.listen_address,
        host_key_path: arguments.host_key,
    })
    .await?;
    info!(address = %server.local_addr()?, "HNSSH SSH listener ready");

    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });

    server.run(shutdown).await?;
    Ok(())
}
